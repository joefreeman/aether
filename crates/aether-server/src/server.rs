//! Server lifecycle: bind the fixed loopback port, write the runtime discovery file, accept
//! connections, clean up on shutdown.
//!
//! The server is multi-project. Projects are loaded lazily by `project/activate` — no project
//! is read from disk at startup.

use crate::config::{self, RuntimeInfo, SERVER_PORT};
use crate::state::{ServerState, SharedState};
use crate::watcher;
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Public entry point: bind the fixed port, manage the runtime file, run the server.
pub async fn run() -> anyhow::Result<()> {
    let bind_addr = format!("127.0.0.1:{SERVER_PORT}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    let port = listener.local_addr()?.port();

    let runtime_path = config::runtime_info_path()?;
    handle_existing_runtime_file(&runtime_path)?;

    // The instance's start stamp lives on `ServerState` (it's reported to clients on
    // `project/activate` for restart detection); the runtime file mirrors the same value, so the
    // file and the wire never disagree about which instance this is.
    let state = Arc::new(Mutex::new(ServerState::new()));
    let info = RuntimeInfo {
        pid: std::process::id(),
        port,
        started_at_unix_ms: state.lock().await.started_at_unix_ms,
    };
    config::write_runtime_info(&runtime_path, &info)?;
    tracing::info!(
        port,
        runtime_file = %runtime_path.display(),
        "aether server listening"
    );

    // Drop guard to clean up the runtime file regardless of how we exit.
    let _guard = RuntimeFileGuard(runtime_path);

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
                    if let Err(e) = crate::http::route(stream, state).await {
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
    pub project_name: String,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// WebSocket URL carrying our own build version in the query string — the server's handshake
    /// requires it to match (see `connection`'s version gate), so tests connect with the real
    /// `PROTOCOL_VERSION` exactly as the native clients do. No token: auth is by loopback
    /// `Host`/`Origin` (see `http::is_loopback_authority`), and connecting via `127.0.0.1` satisfies it.
    pub fn ws_url(&self) -> String {
        format!(
            "ws://127.0.0.1:{}/?version={}",
            self.port,
            aether_protocol::PROTOCOL_VERSION
        )
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
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_multi(vec![(project_name.into(), project_paths)]).await
}

/// Multi-project variant of [`spawn_for_test`]: pre-registers every `(name, paths)` pair on one
/// server, for tests exercising cross-project behavior (e.g. overlapping roots). The handle's
/// `project_name` is the first pair's name.
pub async fn spawn_for_test_multi(
    projects: Vec<(String, Vec<PathBuf>)>,
) -> anyhow::Result<ServerHandle> {
    use crate::state::ProjectEntry;
    use crate::workspace_index::WorkspaceIndex;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let project_name = projects
        .first()
        .map(|(name, _)| name.clone())
        .unwrap_or_default();

    let state = Arc::new(Mutex::new(ServerState::new()));
    {
        let mut s = state.lock().await;
        for (name, paths) in &projects {
            let workspace_index = Arc::new(WorkspaceIndex::new(paths.clone()));
            s.projects.insert(
                name.clone(),
                ProjectEntry {
                    id: name.clone(),
                    name: Some(name.clone()),
                    paths: paths.clone(),
                    workspace_index,
                    mru_buffers: std::collections::VecDeque::new(),
                },
            );
        }
    }

    // Initialize the watcher synchronously, before spawning the run task, so the test can call
    // `watch_project_paths` immediately. (The run task also kicks off `watcher::spawn` but it's a
    // no-op once `state.watcher` is set.)
    crate::watcher::spawn(state.clone()).await?;
    {
        let s = state.lock().await;
        if let Some(w) = s.watcher.clone() {
            for (_, paths) in &projects {
                crate::watcher::watch_project_paths(&w, paths);
            }
        }
    }

    let join = tokio::spawn(async move {
        let _ = run_with_listener(listener, state).await;
    });
    Ok(ServerHandle {
        port,
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

struct RuntimeFileGuard(PathBuf);

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
