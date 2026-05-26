//! Server lifecycle: bind the fixed loopback port, write the runtime discovery file, accept
//! connections, clean up on shutdown.
//!
//! The server is multi-project. Projects are loaded lazily by `project/activate` — no project
//! is read from disk at startup.

use crate::config::{self, RuntimeInfo, SERVER_PORT};
use crate::connection;
use crate::state::{ServerState, SharedState};
use crate::watcher;
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Public entry point: bind the fixed port, manage the runtime file, run the server.
pub async fn run() -> anyhow::Result<()> {
    let token = uuid::Uuid::new_v4().to_string();
    let bind_addr = format!("127.0.0.1:{SERVER_PORT}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    let port = listener.local_addr()?.port();

    let runtime_path = config::runtime_info_path()?;
    handle_existing_runtime_file(&runtime_path)?;
    let info = RuntimeInfo {
        pid: std::process::id(),
        port,
        token: token.clone(),
        started_at_unix_ms: now_unix_ms(),
    };
    config::write_runtime_info(&runtime_path, &info)?;
    tracing::info!(
        port,
        runtime_file = %runtime_path.display(),
        "aether server listening"
    );

    // Drop guard to clean up the runtime file regardless of how we exit.
    let _guard = RuntimeFileGuard(runtime_path);

    let state = Arc::new(Mutex::new(ServerState::new(token)));
    run_with_listener(listener, state).await
}

/// Run the accept loop with an already-bound listener and constructed state. Used by tests to
/// embed the server in-process without touching the filesystem-based runtime file.
pub async fn run_with_listener(listener: TcpListener, state: SharedState) -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;

    // No-op when the watcher's already running (e.g. `spawn_for_test` initialized it ahead of
    // the run task to register project paths synchronously).
    let already_started = state.lock().await.watcher.is_some();
    if !already_started {
        if let Err(e) = watcher::spawn(state.clone()).await {
            tracing::warn!(error = %e, "file watcher failed to start; continuing without it");
        }
    }

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
    pub project_name: String,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// WebSocket URL with the token + a dummy client_version baked into the query string. Tests
    /// always connect via this — the production flow does the same thing in the TUI client.
    pub fn ws_url(&self) -> String {
        format!(
            "ws://127.0.0.1:{}/?token={}&client_version=test",
            self.port, self.token
        )
    }

    /// Base URL without query-string credentials. Used by the one test that intentionally tries
    /// a bad token to assert the upgrade rejection path.
    pub fn ws_url_no_auth(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Spawn the server in-process for testing or embedding. Skips the filesystem-based runtime
/// discovery file, binds to an ephemeral port, and pre-registers a project (so tests can skip
/// laying down `*.toml` files for projects they only need in memory). Tests still send a
/// `project/activate` RPC on each connection — same shape as the production flow.
pub async fn spawn_for_test(
    project_name: impl Into<String>,
    project_paths: Vec<PathBuf>,
    token: impl Into<String>,
) -> anyhow::Result<ServerHandle> {
    use crate::state::ProjectEntry;
    use crate::workspace_index::WorkspaceIndex;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token = token.into();
    let project_name = project_name.into();
    let workspace_index = Arc::new(WorkspaceIndex::new(project_paths.clone()));

    let state = Arc::new(Mutex::new(ServerState::new(token.clone())));
    {
        let mut s = state.lock().await;
        s.projects.insert(
            project_name.clone(),
            ProjectEntry {
                name: project_name.clone(),
                paths: project_paths.clone(),
                workspace_index,
                mru_buffers: std::collections::VecDeque::new(),
            },
        );
    }

    // Initialize the watcher synchronously, before spawning the run task, so the test can call
    // `watch_project_paths` immediately. (The run task also kicks off `watcher::spawn` but it's a
    // no-op once `state.watcher` is set.)
    crate::watcher::spawn(state.clone()).await?;
    {
        let s = state.lock().await;
        if let Some(w) = s.watcher.clone() {
            crate::watcher::watch_project_paths(&w, &project_paths);
        }
    }

    let join = tokio::spawn(async move {
        let _ = run_with_listener(listener, state).await;
    });
    Ok(ServerHandle {
        port,
        token,
        project_name,
        join,
    })
}

fn handle_existing_runtime_file(path: &std::path::Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match config::read_runtime_info(path) {
        Ok(existing) if config::pid_is_alive(existing.pid) => {
            bail!(
                "another aether server is already running (pid {})",
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
