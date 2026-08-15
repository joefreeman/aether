//! Server lifecycle: bind the fixed loopback port, write the runtime discovery file, accept
//! connections, clean up on shutdown.
//!
//! The server is multi-workspace. Workspaces are loaded lazily by `workspace/activate` — no workspace
//! is read from disk at startup.

use crate::config::{self};
use crate::state::{ServerState, SharedState};
use crate::watcher;
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

/// Public entry point: bind the active profile's port, manage the runtime file, run the server.
///
/// The profile is read from the process-global set by `main` (`config::set_active_profile`); its
/// port comes from `profile.toml` (created on first use). `idle_timeout` controls the auto-reaper:
/// `Some(d)` makes this a client-conjured instance that shuts itself down after `d` with no
/// connected clients and no unsaved buffers; `None` (the `ae server` daemon) runs until signalled.
/// See [`idle_reaper`].
pub async fn run(idle_timeout: Option<Duration>) -> anyhow::Result<()> {
    let profile = config::active_profile();
    let port = config::ensure_profile_port()?;
    let bind_addr = format!("127.0.0.1:{port}");

    // Reject early if the recorded port is already taken: a live server for this profile (the pid
    // file says so), or some unrelated process squatting it. We fail loudly rather than reallocate
    // — a recorded port is a stable address (e.g. a bookmarked web URL), so we never move it
    // silently. See `docs/profiles.md`.
    let runtime_path = config::runtime_info_path()?;
    handle_existing_runtime_file(&runtime_path)?;

    let listener = TcpListener::bind(&bind_addr).await.with_context(|| {
        format!(
            "binding {bind_addr} for profile '{profile}' — is the port in use by another process?"
        )
    })?;
    let port = listener.local_addr()?.port();

    // The instance's start stamp lives on `ServerState` — it's reported to clients on
    // `workspace/activate` for restart detection. The runtime file no longer mirrors it (or the
    // port): it's now just the pid, the per-profile singleton marker.
    let state = Arc::new(Mutex::new(ServerState::new()));
    // Point the real server at the on-disk session file (workspace recency + buffer restore). Left
    // unset by `ServerState::new` so in-process tests and embeddings never touch the user's file;
    // this is the one place that opts the production daemon in. A resolution failure (no XDG base
    // dirs) just disables the feature rather than refusing to boot.
    {
        let mut s = state.lock().await;
        s.sessions_path = config::workspace_sessions_path().ok();
        // Opt the production daemon into unsaved-buffer backups (left unset elsewhere — see
        // `ServerState::backups_path`). A resolution failure just disables the feature.
        s.backups_path = config::backups_dir().ok();
        // Hint learning state (docs/hints.md): same opt-in shape as sessions. Loaded
        // once here; a corrupt file logs and starts fresh rather than refusing to boot.
        s.hints_path = config::hints_state_path().ok();
        if let Some(path) = s.hints_path.clone() {
            match config::load_hints_at(&path) {
                Ok(hints) => s.hints = hints,
                Err(e) => tracing::warn!(error = %e, "could not load hint state; starting fresh"),
            }
        }
        // Input-history lists (docs/input-history.md): same opt-in shape again.
        s.history_path = config::history_state_path().ok();
        if let Some(path) = s.history_path.clone() {
            match config::load_history_at(&path) {
                Ok(history) => s.history = history,
                Err(e) => {
                    tracing::warn!(error = %e, "could not load input history; starting fresh")
                }
            }
        }
    }
    config::write_runtime_pid(&runtime_path, std::process::id())?;
    // Log the web URL too: the browser client has no config/CLI access, so a human reads (and
    // bookmarks) this address — which is why a profile's port, once recorded, never moves.
    tracing::info!(
        profile,
        port,
        url = %format!("http://127.0.0.1:{port}/"),
        runtime_file = %runtime_path.display(),
        "aether server listening"
    );

    // Drop guard to clean up the runtime file regardless of how we exit.
    let _guard = RuntimeFileGuard(runtime_path);

    run_with_listener(listener, state, idle_timeout).await
}

/// Run the accept loop with an already-bound listener and constructed state. Used by tests to
/// embed the server in-process without touching the filesystem-based runtime file (they pass
/// `idle_timeout: None` so the test server never reaps itself out from under the test).
pub async fn run_with_listener(
    listener: TcpListener,
    state: SharedState,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;

    // Record the reaper setting so `/status` can report persistent vs. auto-started, and the bound
    // port alongside it (taken from the listener, so it's the port in use rather than the one
    // `profile.toml` recorded). Also no-op the watcher spawn when it's already running (e.g.
    // `spawn_for_test` initialized it ahead of the run task to register workspace paths
    // synchronously).
    let bound_port = listener.local_addr().ok().map(|a| a.port());
    let already_started = {
        let mut s = state.lock().await;
        s.idle_timeout = idle_timeout;
        s.port = bound_port;
        s.watcher.is_some()
    };
    if !already_started {
        if let Err(e) = watcher::spawn(state.clone()).await {
            tracing::warn!(error = %e, "file watcher failed to start; continuing without it");
        }
    }

    // The reaper signals this when an auto-started server has been idle long enough; the accept
    // loop treats it exactly like SIGINT/SIGTERM.
    let idle_shutdown = Arc::new(Notify::new());
    if let Some(timeout) = idle_timeout {
        tokio::spawn(idle_reaper(state.clone(), timeout, idle_shutdown.clone()));
    }

    // When backups are enabled, run the periodic flush that persists unsaved buffer contents (the
    // single writer — see `handlers::flush_backups`). It crash-protects edits to within one interval;
    // a graceful exit gets a final flush below.
    let backups_enabled = state.lock().await.backups_path.is_some();
    if backups_enabled {
        tokio::spawn(backup_flush_loop(state.clone()));
    }

    // Same shape for the two dirty-flag state files — hint learning and input history: one
    // periodic flush (the debounce — events between ticks coalesce into one write) plus a final
    // flush on graceful exit below. Each flush no-ops when its own path is unset.
    let aggregates_enabled = {
        let s = state.lock().await;
        s.hints_path.is_some() || s.history_path.is_some()
    };
    if aggregates_enabled {
        tokio::spawn(aggregate_flush_loop(state.clone()));
    }

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, addr) = res?;
                // Nagle + the peer's delayed ACK turns any push-then-reply write pair into a
                // ~40ms stall (the reply sits in the kernel until the pushed frame is ACKed).
                // Interactive RPC wants every frame on the wire immediately.
                let _ = stream.set_nodelay(true);
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
            _ = idle_shutdown.notified() => {
                tracing::info!("idle timeout elapsed with no clients; shutting down");
                break;
            }
        }
    }
    // Final synchronous flush so a graceful exit (SIGINT/SIGTERM/idle-reap) captures the latest
    // unsaved content; the periodic loop covers SIGKILL/crash to within one interval. No-op when
    // backups are disabled.
    if backups_enabled {
        crate::handlers::flush_backups(&state).await;
    }
    if aggregates_enabled {
        crate::handlers::flush_hints(&state).await;
        crate::handlers::flush_history(&state).await;
    }
    Ok(())
}

/// How often the aggregate-state flush runs. Long enough that a burst of events coalesces into one
/// write, short enough that a crash loses almost nothing that matters (tutorial progress and
/// recall lists, not user content).
const AGGREGATE_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Periodically flush the client-aggregated state files — hint learning (docs/hints.md) and input
/// history (docs/input-history.md) — until the task is aborted (on server shutdown). See
/// [`crate::handlers::flush_hints`] and [`crate::handlers::flush_history`].
async fn aggregate_flush_loop(state: SharedState) {
    loop {
        tokio::time::sleep(AGGREGATE_FLUSH_INTERVAL).await;
        crate::handlers::flush_hints(&state).await;
        crate::handlers::flush_history(&state).await;
    }
}

/// How often the backup flush runs while the server is up. Short enough that a crash loses at most a
/// fraction of a second of typing, long enough that idle (or untouched-buffer) ticks are nearly free
/// — the flush only writes buffers whose content actually changed.
const BACKUP_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Periodically flush unsaved-buffer backups until the task is aborted (on server shutdown). See
/// [`crate::handlers::flush_backups`].
async fn backup_flush_loop(state: SharedState) {
    loop {
        tokio::time::sleep(BACKUP_FLUSH_INTERVAL).await;
        crate::handlers::flush_backups(&state).await;
    }
}

/// Watchdog for client-conjured servers: once the server is idle, start a clock; if it stays idle
/// for `timeout`, notify `shutdown` so the accept loop exits. A reconnecting client resets the clock.
///
/// "Idle" means no clients connected. A dirty buffer whose content wouldn't survive the process
/// additionally pins the server open — reaping it would silently drop unsaved work. That covers
/// backups being *disabled* (in-process tests/embeddings) and dirty buffers in an *ephemeral*
/// workspace, which is never backed up. Everything else is safe on disk (and re-flushed on
/// shutdown), so it no longer blocks the reap: that interim guard is what backup persistence retires.
async fn idle_reaper(state: SharedState, timeout: Duration, shutdown: Arc<Notify>) {
    // Poll often enough to honour `timeout` without busy-looping; for the long production timeout
    // this lands at the 15s ceiling, while short test timeouts still get a sub-timeout cadence.
    let poll = (timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(15));
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(poll).await;
        let idle = {
            let s = state.lock().await;
            let unsaved_pins = s.has_unprotected_unsaved_buffers();
            s.clients.is_empty() && !unsaved_pins
        };
        if idle {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= timeout {
                shutdown.notify_one();
                return;
            }
        } else {
            idle_since = None;
        }
    }
}

/// Handle to a running server (for in-process embedding by tests). Dropping aborts the server task.
pub struct ServerHandle {
    pub port: u16,
    pub workspace_name: String,
    /// The running server's state, so a test seam can install fixtures (declared projects, say)
    /// that no RPC can set without writing to the real config dir.
    pub state: SharedState,
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
/// discovery file, binds to an ephemeral port, and pre-registers a workspace (so tests can skip
/// laying down `*.toml` files for workspaces they only need in memory). Tests still send a
/// `workspace/activate` RPC on each connection — same shape as the production flow.
pub async fn spawn_for_test(
    workspace_name: impl Into<String>,
    workspace_paths: Vec<PathBuf>,
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_multi(vec![(workspace_name.into(), workspace_paths)]).await
}

/// Multi-workspace variant of [`spawn_for_test`]: pre-registers every `(name, paths)` pair on one
/// server, for tests exercising cross-workspace behavior (e.g. overlapping roots). The handle's
/// `workspace_name` is the first pair's name.
pub async fn spawn_for_test_multi(
    workspaces: Vec<(String, Vec<PathBuf>)>,
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_multi_with_sessions(workspaces, None).await
}

/// As [`spawn_for_test_multi`], but points the server at `sessions_path` for the persisted
/// workspace-session file (recency + buffer restore). Tests pass a throwaway tempfile so they can
/// exercise persistence without touching the developer's real `~/.config/aether/sessions.json`.
pub async fn spawn_for_test_multi_with_sessions(
    workspaces: Vec<(String, Vec<PathBuf>)>,
    sessions_path: Option<PathBuf>,
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_multi_with_persistence(workspaces, sessions_path, None).await
}

/// As [`spawn_for_test_multi_with_sessions`], but also points the server at `backups_dir` so tests
/// can exercise unsaved-buffer backups (write + restore) against a throwaway directory. With
/// `backups_dir` set the periodic flush task runs, so a test typically types, polls the backup file
/// into existence, then restarts a second server over the same `sessions_path` + `backups_dir`.
pub async fn spawn_for_test_multi_with_persistence(
    workspaces: Vec<(String, Vec<PathBuf>)>,
    sessions_path: Option<PathBuf>,
    backups_dir: Option<PathBuf>,
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_full(
        workspaces,
        sessions_path,
        backups_dir,
        None,
        None,
        Vec::new(),
    )
    .await
}

/// As [`spawn_for_test`], but registers in-process **dummy language servers** (see
/// [`crate::lsp::dummy`]) keyed by language, so LSP integration tests run deterministically without
/// a real server binary. Any buffer whose language has an entry launches the dummy instead of the
/// real process.
pub async fn spawn_for_test_with_lsp(
    workspace_name: impl Into<String>,
    workspace_paths: Vec<PathBuf>,
    dummy_lsp: Vec<(String, crate::lsp::dummy::DummyLspConfig)>,
) -> anyhow::Result<ServerHandle> {
    spawn_for_test_full(
        vec![(workspace_name.into(), workspace_paths)],
        None,
        None,
        None,
        None,
        dummy_lsp,
    )
    .await
}

/// **Test seam.** As [`spawn_for_test_with_lsp`], but the workspace also *declares projects*
/// (`docs/projects.md`), so their language servers come up pinned at registration.
///
/// The other seams pre-register workspaces in memory, which means `workspace/activate` takes its
/// already-loaded path and never runs the cold-load reconcile that starts pinned servers in
/// production. This runs the same [`crate::handlers::reconcile_workspace_pins`] the cold path does,
/// so the resulting state is what a real activation would have produced. (Going through
/// `workspace/add_project` instead would write a TOML into the *real* config dir, which is why
/// none of the `workspace/*` config-mutating RPCs are integration-tested.)
pub async fn spawn_for_test_with_projects(
    workspace_name: impl Into<String>,
    workspace_paths: Vec<PathBuf>,
    projects: Vec<crate::config::ProjectRef>,
    dummy_lsp: Vec<(String, crate::lsp::dummy::DummyLspConfig)>,
) -> anyhow::Result<ServerHandle> {
    let workspace_name = workspace_name.into();
    let handle =
        spawn_for_test_with_lsp(workspace_name.clone(), workspace_paths, dummy_lsp).await?;
    let launches = {
        let mut s = handle.state.lock().await;
        if let Some(entry) = s.workspaces.get_mut(&workspace_name) {
            entry.projects = projects;
        }
        crate::handlers::reconcile_workspace_pins(&mut s, &workspace_name)
    };
    for (key, spec, generation) in launches {
        tokio::spawn(crate::lsp::manager::launch(
            handle.state.clone(),
            key,
            spec,
            generation,
        ));
    }
    Ok(handle)
}

/// As [`spawn_for_test_multi_with_persistence`], but also points the server at the two
/// client-aggregated state files — `hints_path` (docs/hints.md) and `history_path`
/// (docs/input-history.md) — so tests can exercise `hints/record` / `history/record` aggregation
/// and persistence against throwaway files. With either set the periodic flush runs, so a test
/// records events then polls the file into existence, like the backup tests do.
pub async fn spawn_for_test_full(
    workspaces: Vec<(String, Vec<PathBuf>)>,
    sessions_path: Option<PathBuf>,
    backups_dir: Option<PathBuf>,
    hints_path: Option<PathBuf>,
    history_path: Option<PathBuf>,
    dummy_lsp: Vec<(String, crate::lsp::dummy::DummyLspConfig)>,
) -> anyhow::Result<ServerHandle> {
    use crate::state::WorkspaceEntry;
    use crate::workspace_index::WorkspaceIndex;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let workspace_name = workspaces
        .first()
        .map(|(name, _)| name.clone())
        .unwrap_or_default();

    let state = Arc::new(Mutex::new(ServerState::new()));
    {
        let mut s = state.lock().await;
        s.sessions_path = sessions_path;
        s.backups_path = backups_dir;
        s.hints_path = hints_path;
        // In-process dummy language servers (test seam — see `lsp::dummy`). Seeded before the run
        // task starts so any buffer opened afterwards launches the dummy, not a real process.
        for (language, config) in dummy_lsp {
            s.lsp.dummy_configs.insert(language, config);
        }
        if let Some(path) = s.hints_path.clone() {
            s.hints = crate::config::load_hints_at(&path).unwrap_or_default();
        }
        s.history_path = history_path;
        if let Some(path) = s.history_path.clone() {
            s.history = crate::config::load_history_at(&path).unwrap_or_default();
        }
        for (name, paths) in &workspaces {
            let workspace_index = Arc::new(WorkspaceIndex::new(paths.clone()));
            s.workspaces.insert(
                name.clone(),
                WorkspaceEntry {
                    id: name.clone(),
                    name: Some(name.clone()),
                    paths: paths.clone(),
                    workspace_index,
                    mru_buffers: std::collections::VecDeque::new(),
                    dormant_buffers: Vec::new(),
                    projects: Vec::new(),
                },
            );
        }
    }

    // Initialize the watcher synchronously, before spawning the run task, so the test can call
    // `watch_workspace_paths` immediately. (The run task also kicks off `watcher::spawn` but it's a
    // no-op once `state.watcher` is set.)
    crate::watcher::spawn(state.clone()).await?;
    {
        let s = state.lock().await;
        if let Some(w) = s.watcher.clone() {
            for (_, paths) in &workspaces {
                crate::watcher::watch_workspace_paths(&w, paths);
            }
        }
    }

    let join = tokio::spawn({
        let state = state.clone();
        async move {
            let _ = run_with_listener(listener, state, None).await;
        }
    });
    Ok(ServerHandle {
        port,
        workspace_name,
        state,
        join,
    })
}

fn handle_existing_runtime_file(path: &std::path::Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match config::read_runtime_pid(path) {
        Ok(pid) if config::pid_is_alive(pid) => {
            bail!("another aether server is already running (pid {pid})");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Buffer, ServerState};
    use std::path::PathBuf;

    /// A reapable server with no clients ever connecting shuts itself down once the idle timeout
    /// elapses — this is the auto-start cleanup path.
    #[tokio::test]
    async fn idle_server_reaps_when_no_clients_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(Mutex::new(ServerState::new()));
        let handle = tokio::spawn(run_with_listener(
            listener,
            state,
            Some(Duration::from_millis(80)),
        ));

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            handle.is_finished(),
            "expected the idle server to reap itself after the timeout"
        );
    }

    /// A dirty buffer pins the server open even with no clients connected: we never reap unsaved
    /// work out from under a disconnected (e.g. crashed) client.
    #[tokio::test]
    async fn dirty_buffer_prevents_idle_reap() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(Mutex::new(ServerState::new()));
        {
            let mut s = state.lock().await;
            let id: aether_protocol::BufferId = 1;
            let mut buf = Buffer::new_at_path(id, PathBuf::from("/tmp/dirty.txt"), None);
            buf.dirty = true;
            s.buffers.insert(id, buf);
        }
        let handle = tokio::spawn(run_with_listener(
            listener,
            state,
            Some(Duration::from_millis(80)),
        ));

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !handle.is_finished(),
            "server reaped itself despite a dirty buffer"
        );
        handle.abort();
    }

    /// With backups on, *where* the dirty buffer lives decides whether it pins the server: a named
    /// workspace's unsaved content is flushed to `backups/` and restored on the next open, so the
    /// reap is safe — but an ephemeral ("no workspace") context is never backed up, so reaping it
    /// would silently discard the edits. Both servers are started together and judged after one
    /// timeout window.
    #[tokio::test]
    async fn only_an_unbacked_dirty_buffer_pins_the_server_open() {
        let backups = tempfile::tempdir().unwrap();
        // One dirty buffer, in a named workspace (backed up) or an ephemeral one (not).
        let dirty_in = |ephemeral: bool| {
            let mut s = ServerState::new();
            s.backups_path = Some(backups.path().to_path_buf());
            let workspace = if ephemeral {
                s.register_ephemeral_workspace()
            } else {
                s.workspaces.insert(
                    "p".into(),
                    crate::state::WorkspaceEntry {
                        id: "p".into(),
                        name: Some("p".into()),
                        paths: Vec::new(),
                        workspace_index: Arc::new(crate::workspace_index::WorkspaceIndex::new(
                            Vec::new(),
                        )),
                        mru_buffers: Default::default(),
                        dormant_buffers: Vec::new(),
                        projects: Vec::new(),
                    },
                );
                "p".to_string()
            };
            let id: aether_protocol::BufferId = 1;
            let mut buf = Buffer::new_at_path(id, PathBuf::from("/tmp/dirty.txt"), None);
            buf.dirty = true;
            s.buffers.insert(id, buf);
            s.buffer_workspaces.insert(id, workspace);
            Arc::new(Mutex::new(s))
        };
        let timeout = Some(Duration::from_millis(80));
        let backed_up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let temporary_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backed_up = tokio::spawn(run_with_listener(
            backed_up_listener,
            dirty_in(false),
            timeout,
        ));
        let temporary = tokio::spawn(run_with_listener(
            temporary_listener,
            dirty_in(true),
            timeout,
        ));

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            backed_up.is_finished(),
            "a dirty buffer whose content is safe in backups/ must not block the reap"
        );
        assert!(
            !temporary.is_finished(),
            "a dirty buffer in a temporary workspace is never backed up — reaping it would lose it"
        );
        temporary.abort();
    }

    /// A persistent (`None` timeout) server — the `ae server` daemon — never reaps, even with no
    /// clients and a clean tree.
    #[tokio::test]
    async fn persistent_server_never_reaps() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(Mutex::new(ServerState::new()));
        let handle = tokio::spawn(run_with_listener(listener, state, None));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !handle.is_finished(),
            "a persistent server must not shut itself down"
        );
        handle.abort();
    }
}
