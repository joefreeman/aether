//! Server lifecycle: load config, bind a WebSocket listener, write the runtime discovery file,
//! accept connections, clean up on shutdown.

use crate::config::{self, RuntimeInfo};
use crate::connection;
use crate::state::{ServerState, SharedState};
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Public entry point: read project config, manage the runtime file, run the server.
pub async fn run(project_name: &str) -> anyhow::Result<()> {
    let project = config::load_project(project_name)?;
    let canonical_paths = project
        .paths
        .iter()
        .map(|p| config::canonicalize_project_path(p))
        .collect::<Result<Vec<_>, _>>()?;

    let token = uuid::Uuid::new_v4().to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let runtime_path = config::runtime_info_path(project_name)?;
    handle_existing_runtime_file(&runtime_path, project_name)?;
    let info = RuntimeInfo {
        pid: std::process::id(),
        port,
        token: token.clone(),
        started_at_unix_ms: now_unix_ms(),
    };
    config::write_runtime_info(&runtime_path, &info)?;
    tracing::info!(
        project = %project_name,
        port,
        runtime_file = %runtime_path.display(),
        "aether server listening"
    );

    // Drop guard to clean up the runtime file regardless of how we exit.
    let _guard = RuntimeFileGuard(runtime_path);

    let state = Arc::new(Mutex::new(ServerState::new(project.name.clone(), canonical_paths, token)));
    run_with_listener(listener, state).await
}

/// Run the accept loop with an already-bound listener and constructed state. Used by tests to
/// embed the server in-process without touching the filesystem-based config/runtime files.
pub async fn run_with_listener(listener: TcpListener, state: SharedState) -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, addr) = res?;
                tracing::debug!(%addr, "TCP connection accepted");
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection::handle(stream, state).await {
                        tracing::warn!(error = %e, %addr, "connection handler ended with error");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// Handle to a running server (for in-process embedding by tests). Dropping aborts the server task.
pub struct ServerHandle {
    pub port: u16,
    pub token: String,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Spawn the server in-process for testing or embedding. Skips the filesystem-based project
/// config and runtime discovery file.
pub async fn spawn_for_test(
    project_name: impl Into<String>,
    project_paths: Vec<PathBuf>,
    token: impl Into<String>,
) -> anyhow::Result<ServerHandle> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token = token.into();
    let state = Arc::new(Mutex::new(ServerState::new(project_name.into(), project_paths, token.clone())));
    let join = tokio::spawn(async move {
        let _ = run_with_listener(listener, state).await;
    });
    Ok(ServerHandle { port, token, join })
}

fn handle_existing_runtime_file(path: &std::path::Path, project_name: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match config::read_runtime_info(path) {
        Ok(existing) if config::pid_is_alive(existing.pid) => {
            bail!(
                "another aether server is running for project {} (pid {})",
                project_name,
                existing.pid
            );
        }
        Ok(_) => {
            tracing::warn!(
                runtime_file = %path.display(),
                "removing stale runtime file (no live process)"
            );
            std::fs::remove_file(path).context("removing stale runtime file")?;
        }
        Err(e) => {
            tracing::warn!(
                runtime_file = %path.display(),
                error = %e,
                "could not parse existing runtime file; removing"
            );
            std::fs::remove_file(path).context("removing unparseable runtime file")?;
        }
    }
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct RuntimeFileGuard(PathBuf);

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
