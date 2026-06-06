//! The stateful LSP layer: one language server per `(workspace_root, language)`, the buffers open
//! against each, and the lifecycle that ties them to editor state.
//!
//! Document sync ([`LspClient::notify`]) is synchronous — a channel send — so `didOpen`/`didChange`/
//! `didClose` are fired straight from the locked handler sections (see the `notify_*` methods). Only
//! the handshake awaits, so launching a server happens in a background task ([`launch`]) that never
//! blocks a handler under the state lock.
//!
//! Each handle carries a **generation**: restarting removes the old handle (killing its process) and
//! creates a fresh one with a new generation. The old process's reader task will eventually report
//! the connection closed, but its terminal "crashed" update is keyed by generation and so can't
//! clobber the freshly-relaunched server.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::lsp::{LspServerStatus, LspStatus, LspStatusChanged};
use aether_protocol::BufferId;
use serde_json::Value;
use tokio::sync::mpsc;

use super::client::{LspClient, LspInbound};
use super::config::{self, LspServerSpec, WorkspaceMarker};
use super::position::PositionEncoding;
use super::{lifecycle, process, uri};
use crate::state::{ServerState, SharedState};

/// Identifies a server instance: one per workspace root per language.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LspServerKey {
    pub root: PathBuf,
    pub language: String,
}

/// A language server and the buffers synced against it.
pub struct LspHandle {
    pub language: String,
    pub workspace_root: PathBuf,
    /// Command name until the handshake completes, then the server-reported `serverInfo.name`.
    pub server_name: String,
    pub status: LspStatus,
    /// Distinguishes successive processes for the same key across restarts (see module docs).
    pub generation: u64,
    /// `Some` once the handshake completes; `None` while `Starting`/`Crashed`.
    pub client: Option<LspClient>,
    pub position_encoding: PositionEncoding,
    /// Whether the server advertises whole-document formatting (set from the handshake).
    pub document_formatting: bool,
    /// Buffers we've sent `didOpen` for (and not yet `didClose`).
    pub open_buffers: HashSet<BufferId>,
    /// Buffers that want this server but were registered before it became `Ready`; opened in bulk
    /// once the handshake lands.
    pub registered_buffers: HashSet<BufferId>,
    /// Kept alive so the subprocess isn't reaped (`kill_on_drop`); dropping the handle kills it.
    child: Option<tokio::process::Child>,
}

#[derive(Default)]
pub struct LspManager {
    pub servers: HashMap<LspServerKey, LspHandle>,
    /// Which server each open document is synced against, for `didChange`/`didClose` routing.
    pub doc_server: HashMap<BufferId, LspServerKey>,
    next_generation: u64,
}

impl LspManager {
    /// Ensure a handle exists for `key`. Returns `Some(generation)` if a fresh one was created (the
    /// caller should spawn its [`launch`] task), or `None` if one already existed.
    pub fn ensure(&mut self, key: &LspServerKey, server_name: &str) -> Option<u64> {
        if self.servers.contains_key(key) {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.servers.insert(
            key.clone(),
            LspHandle {
                language: key.language.clone(),
                workspace_root: key.root.clone(),
                server_name: server_name.to_string(),
                status: LspStatus::Starting,
                generation,
                client: None,
                position_encoding: PositionEncoding::Utf16,
                document_formatting: false,
                open_buffers: HashSet::new(),
                registered_buffers: HashSet::new(),
                child: None,
            },
        );
        Some(generation)
    }

    /// Record that `buffer_id` belongs to `key`'s server (for later routing).
    pub fn register_doc(&mut self, buffer_id: BufferId, key: &LspServerKey) {
        self.doc_server.insert(buffer_id, key.clone());
        if let Some(h) = self.servers.get_mut(key) {
            h.registered_buffers.insert(buffer_id);
        }
    }

    /// Send `didOpen` for a buffer if its server is ready (idempotent). A no-op while the server is
    /// still starting — [`launch`] opens all registered buffers once it reaches `Ready`.
    pub fn notify_open(
        &mut self,
        buffer_id: BufferId,
        key: &LspServerKey,
        uri: &str,
        language: &str,
        version: i64,
        text: &str,
    ) {
        let Some(h) = self.servers.get_mut(key) else { return };
        if h.open_buffers.contains(&buffer_id) {
            return;
        }
        if let (LspStatus::Ready, Some(client)) = (&h.status, &h.client) {
            if lifecycle::did_open(client, uri, language, version, text).is_ok() {
                h.open_buffers.insert(buffer_id);
            }
        }
    }

    /// Send `didChange` (full document) for a buffer that's open against a ready server.
    pub fn notify_change(&mut self, buffer_id: BufferId, uri: &str, version: i64, text: &str) {
        let Some(key) = self.doc_server.get(&buffer_id) else { return };
        let Some(h) = self.servers.get(key) else { return };
        if !h.open_buffers.contains(&buffer_id) {
            return;
        }
        if let Some(client) = &h.client {
            let _ = lifecycle::did_change_full(client, uri, version, text);
        }
    }

    /// Send `didClose` and forget the buffer. If that was the server's last buffer, tear the
    /// server down — drop its handle (killing the process via `kill_on_drop`) — and return its key
    /// so the caller can refresh any open status views. Returns `None` when the server stays up.
    pub fn notify_close(&mut self, buffer_id: BufferId, uri: &str) -> Option<LspServerKey> {
        let key = self.doc_server.remove(&buffer_id)?;
        let idle = {
            let h = self.servers.get_mut(&key)?;
            h.registered_buffers.remove(&buffer_id);
            if h.open_buffers.remove(&buffer_id) {
                if let Some(client) = &h.client {
                    let _ = lifecycle::did_close(client, uri);
                }
            }
            h.open_buffers.is_empty() && h.registered_buffers.is_empty()
        };
        if idle {
            // Last buffer gone → shut the server down. Dropping the handle drops its `Child`
            // (`kill_on_drop`). The old reader task will observe EOF and try a `Crashed` update,
            // but `set_status` finds no handle and no-ops; a later reopen gets a fresh generation.
            self.servers.remove(&key);
            Some(key)
        } else {
            None
        }
    }

    /// Snapshot of every server whose root falls under one of `project_roots` — drives
    /// `lsp/server_status`.
    pub fn status_for_roots(&self, project_roots: &[PathBuf]) -> Vec<LspServerStatus> {
        self.servers
            .values()
            .filter(|h| project_roots.iter().any(|r| h.workspace_root.starts_with(r)))
            .map(handle_status)
            .collect()
    }
}

fn handle_status(h: &LspHandle) -> LspServerStatus {
    LspServerStatus {
        name: h.server_name.clone(),
        language: h.language.clone(),
        workspace_root: h.workspace_root.display().to_string(),
        status: h.status.clone(),
    }
}

/// Find the server root for `file`, searching ancestors up to (but not above) the project root
/// that contains it. Precedence:
/// 1. **Workspace root** — the *outermost* ancestor matching `workspace` (a Cargo `[workspace]` /
///    `go.work`), so a whole workspace gets one server instead of one per crate/module.
/// 2. else the **nearest** ancestor holding one of `root_markers`.
/// 3. else the project root, else the file's own directory.
pub fn discover_root(
    file: &Path,
    root_markers: &[&str],
    workspace: WorkspaceMarker,
    project_roots: &[PathBuf],
) -> PathBuf {
    let project_root = project_roots
        .iter()
        .filter(|r| file.starts_with(r))
        .max_by_key(|r| r.components().count());

    // Ancestor dirs from the file up to (and including) the project root — nearest first.
    let mut dirs: Vec<&Path> = Vec::new();
    let mut dir = file.parent();
    while let Some(d) = dir {
        dirs.push(d);
        match project_root {
            Some(pr) if d == pr => break, // don't climb above the project root
            Some(_) => dir = d.parent(),
            None => break, // no project context: only the file's own directory
        }
    }

    // 1. Workspace root wins: the outermost ancestor matching the workspace marker.
    let is_workspace = |d: &Path| match workspace {
        WorkspaceMarker::None => false,
        WorkspaceMarker::File(f) => d.join(f).exists(),
        WorkspaceMarker::FileContaining { file: f, needle } => file_has_line(&d.join(f), needle),
    };
    if let Some(d) = dirs.iter().rev().find(|d| is_workspace(d)) {
        return d.to_path_buf();
    }

    // 2. Nearest root marker.
    if let Some(d) = dirs
        .iter()
        .find(|d| root_markers.iter().any(|m| d.join(m).exists()))
    {
        return d.to_path_buf();
    }

    // 3. Fallbacks.
    project_root
        .cloned()
        .or_else(|| file.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| file.to_path_buf())
}

/// Whether `path` is readable and has a line that (after leading whitespace) starts with `needle`
/// — used to spot a Cargo `[workspace]` table without a full TOML parse.
fn file_has_line(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|c| c.lines().any(|l| l.trim_start().starts_with(needle)))
}

/// Background task: spawn the subprocess, hand off to [`bring_up`]. Marks the handle `Crashed` if
/// the process can't be spawned.
pub async fn launch(state: SharedState, key: LspServerKey, spec: LspServerSpec, generation: u64) {
    let proc = match process::spawn(spec.command, spec.args) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(server = %key.language, error = %e, "failed to spawn language server");
            set_status(&state, &key, generation, LspStatus::Crashed { code: None, message: format!("spawn failed: {e}") }).await;
            return;
        }
    };
    bring_up(&state, key, generation, proc.client, proc.inbound, Some(proc.child)).await;
}

/// Perform the handshake, mark the server `Ready`, open every registered buffer, push the status
/// change, then drain the server's inbound channel until it closes.
async fn bring_up(
    state: &SharedState,
    key: LspServerKey,
    generation: u64,
    client: LspClient,
    inbound: mpsc::UnboundedReceiver<LspInbound>,
    child: Option<tokio::process::Child>,
) {
    // Handshake must NOT hold the state lock (it awaits a round-trip). Server-specific
    // `initializationOptions` come from the config table (e.g. the vscode servers' formatter opt-in).
    let init_options = config::server_spec(&key.language)
        .and_then(|s| s.init_options)
        .and_then(|s| serde_json::from_str(s).ok());
    let caps = match lifecycle::initialize(&client, &key.root, init_options).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(server = %key.language, error = %e, "lsp handshake failed");
            set_status(state, &key, generation, LspStatus::Crashed { code: None, message: format!("handshake failed: {e}") }).await;
            return;
        }
    };

    let pushes = {
        let mut guard = state.lock().await;
        let s = &mut *guard;
        // Bail if a newer instance superseded us (restart) or the handle was removed while we
        // handshook. Returning here drops `child`, killing this now-orphaned process.
        if s.lsp.servers.get(&key).map(|h| h.generation) != Some(generation) {
            return;
        }
        {
            let h = s.lsp.servers.get_mut(&key).expect("just checked");
            h.client = Some(client.clone());
            h.position_encoding = caps.position_encoding;
            h.document_formatting = caps.document_formatting;
            // Keep the launch command as the name when the server reports none (vscode json/css/
            // html) rather than overwriting it with a placeholder.
            if let Some(name) = &caps.name {
                h.server_name = name.clone();
            }
            h.status = LspStatus::Ready;
            h.child = child;
        }

        // didOpen every still-present, file-backed buffer that registered before we were ready.
        let registered: Vec<BufferId> =
            s.lsp.servers[&key].registered_buffers.iter().copied().collect();
        for bid in registered {
            if s.lsp.servers[&key].open_buffers.contains(&bid) {
                continue;
            }
            let Some(buf) = s.buffers.get(&bid) else { continue };
            let Some(path) = buf.canonical_path.as_deref() else { continue };
            let doc_uri = uri::path_to_uri(path);
            let text = buf.text.to_string();
            let version = buf.revision as i64;
            if lifecycle::did_open(&client, &doc_uri, &key.language, version, &text).is_ok() {
                s.lsp.servers.get_mut(&key).expect("present").open_buffers.insert(bid);
            }
        }

        tracing::info!(server = caps.name.as_deref().unwrap_or(&key.language), language = %key.language, root = %key.root.display(), "language server ready");
        let mut out = collect_status_pushes(s, &key);
        out.extend(crate::handlers::refresh_lsp_server_pickers(s));
        out
    };
    send_all(pushes).await;

    inbound_loop(state.clone(), key, generation, inbound).await;
}

/// Drain a server's inbound channel. Phase 1: log diagnostics (Phase 2 renders them) and answer
/// server-initiated requests minimally so the server isn't left blocking. On channel close the
/// server has exited — mark it `Crashed` (if still the current generation).
async fn inbound_loop(
    state: SharedState,
    key: LspServerKey,
    generation: u64,
    mut inbound: mpsc::UnboundedReceiver<LspInbound>,
) {
    while let Some(msg) = inbound.recv().await {
        match msg {
            LspInbound::Notification { method, params } if method == "textDocument/publishDiagnostics" => {
                let count = params.get("diagnostics").and_then(|d| d.as_array()).map_or(0, Vec::len);
                tracing::debug!(server = %key.language, count, "lsp diagnostics");
                handle_publish_diagnostics(&state, &key, &params).await;
            }
            LspInbound::Notification { method, .. } => {
                tracing::debug!(server = %key.language, %method, "lsp notification");
            }
            LspInbound::Request { id, method, params } => {
                // Minimal answers for the server→client requests we don't yet act on. Specific
                // handling (applyEdit, …) arrives with the features that need it.
                let result = server_request_response(&method, &params);
                let client = {
                    let g = state.lock().await;
                    g.lsp.servers.get(&key).and_then(|h| h.client.clone())
                };
                if let Some(c) = client {
                    let _ = c.respond(id, result);
                }
                tracing::debug!(server = %key.language, %method, "lsp server request answered");
            }
        }
    }
    tracing::warn!(server = %key.language, "language server connection closed");
    set_status(&state, &key, generation, LspStatus::Crashed { code: None, message: "connection closed".into() }).await;
}

/// Build our reply to a server→client request we don't actively handle yet.
///
/// `workspace/configuration` is special: the spec wants an **array sized to `params.items`**, one
/// settings value per requested section. We have no config system, so every entry is `null` (the
/// server falls back to its defaults) — but it must be a correctly-sized array, not a bare `null`
/// (lenient servers tolerate the latter; conformant ones expect the array). Everything else
/// (`workspace/applyEdit`, `window/workDoneProgress/create`, …) gets a minimal `null`.
fn server_request_response(method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => {
            let n = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Value::Array(vec![Value::Null; n])
        }
        _ => Value::Null,
    }
}

/// Resolve a `publishDiagnostics` payload to a buffer, convert it to buffer coordinates using the
/// server's negotiated encoding, store it, and re-render the buffer's viewports.
async fn handle_publish_diagnostics(state: &SharedState, key: &LspServerKey, params: &Value) {
    let Some(doc_uri) = params.get("uri").and_then(Value::as_str) else { return };
    let Some(path) = uri::uri_to_path(doc_uri) else { return };
    let diags_json = params.get("diagnostics").cloned().unwrap_or(Value::Null);

    let pushes = {
        let mut guard = state.lock().await;
        let s = &mut *guard;
        let Some(buffer_id) = s.buffers.iter().find_map(|(id, b)| {
            (b.canonical_path.as_deref() == Some(path.as_path())).then_some(*id)
        }) else {
            return; // diagnostics for a buffer we don't have open
        };
        let encoding = s
            .lsp
            .servers
            .get(key)
            .map(|h| h.position_encoding)
            .unwrap_or(PositionEncoding::Utf16);
        let diags = {
            let buf = &s.buffers[&buffer_id];
            super::diagnostics::from_lsp(&diags_json, &buf.text, encoding)
        };
        crate::handlers::set_diagnostics_and_refresh(s, buffer_id, diags)
    };
    send_all(pushes).await;
}

/// Restart every server for `language` whose root is under one of `project_roots`: tear down the old
/// process and relaunch, re-registering the documents that were open against it so they reopen once
/// the new process is ready.
pub async fn restart(state: &SharedState, language: &str, project_roots: &[PathBuf]) {
    let keys: Vec<LspServerKey> = {
        let guard = state.lock().await;
        guard
            .lsp
            .servers
            .keys()
            .filter(|k| k.language == language && project_roots.iter().any(|r| k.root.starts_with(r)))
            .cloned()
            .collect()
    };

    for key in keys {
        let Some(spec) = config::server_spec(&key.language) else { continue };
        let relaunch = {
            let mut guard = state.lock().await;
            let s = &mut *guard;
            // Drop the old handle (kills its process); its inbound loop's terminal update is keyed
            // by the old generation and so won't touch the new handle.
            s.lsp.servers.remove(&key);
            let generation = s.lsp.ensure(&key, spec.command).expect("just removed");
            let docs: Vec<BufferId> = s
                .lsp
                .doc_server
                .iter()
                .filter(|(_, k)| **k == key)
                .map(|(b, _)| *b)
                .collect();
            if let Some(h) = s.lsp.servers.get_mut(&key) {
                h.registered_buffers.extend(docs);
            }
            generation
        };
        push_status(state, &key).await;
        tokio::spawn(launch(state.clone(), key, spec, relaunch));
    }
}

/// Set a server's status (only if `generation` is still current) and push `lsp/status_changed`.
async fn set_status(state: &SharedState, key: &LspServerKey, generation: u64, status: LspStatus) {
    let pushes = {
        let mut guard = state.lock().await;
        let Some(h) = guard.lsp.servers.get_mut(key) else { return };
        if h.generation != generation {
            return; // superseded by a newer instance
        }
        h.status = status;
        if matches!(h.status, LspStatus::Crashed { .. } | LspStatus::Stopped) {
            h.client = None;
        }
        let mut out = collect_status_pushes(&guard, key);
        out.extend(crate::handlers::refresh_lsp_server_pickers(&mut guard));
        out
    };
    send_all(pushes).await;
}

/// Push the current status of `key` to interested clients (no state change).
async fn push_status(state: &SharedState, key: &LspServerKey) {
    let pushes = {
        let mut guard = state.lock().await;
        let mut out = collect_status_pushes(&guard, key);
        out.extend(crate::handlers::refresh_lsp_server_pickers(&mut guard));
        out
    };
    send_all(pushes).await;
}

/// Build `lsp/status_changed` notifications for every client whose active project contains `key`'s
/// root.
fn collect_status_pushes(
    s: &ServerState,
    key: &LspServerKey,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let Some(handle) = s.lsp.servers.get(key) else { return Vec::new() };
    let params = serde_json::to_value(handle_status(handle)).expect("infallible");
    s.clients
        .values()
        .filter(|c| {
            c.active_project
                .as_deref()
                .and_then(|p| s.projects.get(p))
                .is_some_and(|proj| proj.contains(&key.root))
        })
        .map(|c| {
            (
                c.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: LspStatusChanged::NAME.into(),
                    params: params.clone(),
                },
            )
        })
        .collect()
}

async fn send_all(pushes: Vec<(mpsc::Sender<Notification>, Notification)>) {
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::client::connect;
    use crate::lsp::transport;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::{AsyncRead, AsyncWrite, BufReader};

    // ---- discover_root --------------------------------------------------------------------------

    #[test]
    fn discover_root_finds_nearest_marker() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        let src = root.join("crates/x/src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "").unwrap();

        let found = discover_root(&file, &["Cargo.toml"], WorkspaceMarker::None, &[root.to_path_buf()]);
        assert_eq!(found, root);
    }

    #[test]
    fn discover_root_prefers_inner_marker() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let inner = root.join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("Cargo.toml"), "").unwrap();
        let file = inner.join("lib.rs");
        std::fs::write(&file, "").unwrap();

        let found = discover_root(&file, &["Cargo.toml"], WorkspaceMarker::None, &[root.to_path_buf()]);
        assert_eq!(found, inner);
    }

    #[test]
    fn discover_root_falls_back_to_project_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("main.rs");
        std::fs::write(&file, "").unwrap();

        let found = discover_root(&file, &["Cargo.toml"], WorkspaceMarker::None, &[root.to_path_buf()]);
        assert_eq!(found, root);
    }

    #[test]
    fn discover_root_prefers_cargo_workspace_over_crate() {
        // Workspace root (Cargo.toml with `[workspace]`) + a member crate with its own Cargo.toml.
        // A file in the member must resolve to the *workspace* root, not the crate — one
        // rust-analyzer for the whole workspace.
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/x\"]\n").unwrap();
        let crate_dir = root.join("crates/x");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let file = crate_dir.join("src/main.rs");
        std::fs::write(&file, "").unwrap();

        let ws = WorkspaceMarker::FileContaining { file: "Cargo.toml", needle: "[workspace]" };
        let found = discover_root(&file, &["Cargo.toml"], ws, &[root.to_path_buf()]);
        assert_eq!(found, root, "should resolve to the workspace root, not the crate");
    }

    #[test]
    fn discover_root_without_workspace_table_uses_nearest_crate() {
        // No `[workspace]` anywhere → fall back to nearest-marker (a standalone crate).
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"root\"\n").unwrap();
        let inner = root.join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("Cargo.toml"), "[package]\nname = \"sub\"\n").unwrap();
        let file = inner.join("lib.rs");
        std::fs::write(&file, "").unwrap();

        let ws = WorkspaceMarker::FileContaining { file: "Cargo.toml", needle: "[workspace]" };
        let found = discover_root(&file, &["Cargo.toml"], ws, &[root.to_path_buf()]);
        assert_eq!(found, inner);
    }

    #[test]
    fn discover_root_prefers_go_work() {
        // `go.work` at the root + a module with `go.mod` → resolve to the go.work root.
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.work"), "go 1.22\n").unwrap();
        let module = root.join("svc");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("go.mod"), "module svc\n").unwrap();
        let file = module.join("main.go");
        std::fs::write(&file, "").unwrap();

        let found = discover_root(
            &file,
            &["go.mod", "go.work"],
            WorkspaceMarker::File("go.work"),
            &[root.to_path_buf()],
        );
        assert_eq!(found, root);
    }

    // ---- notify routing -------------------------------------------------------------------------

    /// Mock server: replies to `initialize`, forwards every notification it receives to `events`.
    async fn mock_server<R, W>(reader: R, mut writer: W, events: mpsc::UnboundedSender<(String, Value)>)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        while let Ok(Some(body)) = transport::read_frame(&mut reader).await {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = msg.get("id") {
                let result = if method == "initialize" {
                    json!({"capabilities": {"positionEncoding": "utf-8"}, "serverInfo": {"name": "mock"}})
                } else {
                    Value::Null
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                transport::write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap())
                    .await
                    .unwrap();
                let _ = events.send((format!("request:{method}"), params));
            } else {
                let _ = events.send((method, params));
            }
        }
    }

    fn ready_handle_to_mock(
        key: &LspServerKey,
    ) -> (LspHandle, mpsc::UnboundedReceiver<(String, Value)>) {
        let (client_io, server_io) = tokio::io::duplex(16384);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        tokio::spawn(mock_server(sr, sw, ev_tx));
        let (client, _inbound) = connect(cr, cw);
        let handle = LspHandle {
            language: key.language.clone(),
            workspace_root: key.root.clone(),
            server_name: "mock".into(),
            status: LspStatus::Ready,
            generation: 0,
            client: Some(client),
            position_encoding: PositionEncoding::Utf8,
            document_formatting: true,
            open_buffers: HashSet::new(),
            registered_buffers: HashSet::new(),
            child: None,
        };
        (handle, ev_rx)
    }

    async fn recv(rx: &mut mpsc::UnboundedReceiver<(String, Value)>) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("closed")
    }

    #[tokio::test]
    async fn open_change_close_reach_the_server() {
        let key = LspServerKey { root: PathBuf::from("/proj"), language: "rust".into() };
        let (handle, mut ev) = ready_handle_to_mock(&key);
        let mut mgr = LspManager::default();
        mgr.servers.insert(key.clone(), handle);
        mgr.register_doc(7, &key);

        let uri = "file:///proj/src/main.rs";
        mgr.notify_open(7, &key, uri, "rust", 1, "fn main() {}");
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didOpen");
        assert_eq!(p["textDocument"]["uri"], uri);
        assert_eq!(p["textDocument"]["version"], 1);

        mgr.notify_change(7, uri, 2, "fn main() { todo!() }");
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didChange");
        assert_eq!(p["textDocument"]["version"], 2);
        assert_eq!(p["contentChanges"][0]["text"], "fn main() { todo!() }");

        mgr.notify_close(7, uri);
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didClose");
        assert_eq!(p["textDocument"]["uri"], uri);

        // After close the doc is forgotten: a further change is a no-op (no message).
        mgr.notify_change(7, uri, 3, "x");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), ev.recv()).await.is_err(),
            "no message expected after close"
        );
    }

    #[tokio::test]
    async fn notify_close_tears_down_idle_server() {
        let key = LspServerKey { root: PathBuf::from("/proj"), language: "rust".into() };
        let (handle, mut ev) = ready_handle_to_mock(&key);
        let mut mgr = LspManager::default();
        mgr.servers.insert(key.clone(), handle);
        mgr.register_doc(7, &key);
        let uri = "file:///proj/src/main.rs";
        mgr.notify_open(7, &key, uri, "rust", 1, "fn main() {}");
        let _ = recv(&mut ev).await; // didOpen

        // Closing the only buffer tears the server down and hands back its key.
        let stopped = mgr.notify_close(7, uri);
        assert_eq!(stopped.as_ref(), Some(&key));
        assert!(!mgr.servers.contains_key(&key), "idle server removed");
        let (m, _) = recv(&mut ev).await; // didClose still sent before teardown
        assert_eq!(m, "textDocument/didClose");
    }

    #[tokio::test]
    async fn notify_close_keeps_server_with_other_buffers() {
        let key = LspServerKey { root: PathBuf::from("/proj"), language: "rust".into() };
        let (handle, mut ev) = ready_handle_to_mock(&key);
        let mut mgr = LspManager::default();
        mgr.servers.insert(key.clone(), handle);
        mgr.register_doc(7, &key);
        mgr.register_doc(8, &key);
        mgr.notify_open(7, &key, "file:///proj/a.rs", "rust", 1, "");
        mgr.notify_open(8, &key, "file:///proj/b.rs", "rust", 1, "");
        let _ = recv(&mut ev).await;
        let _ = recv(&mut ev).await;

        // One of two buffers closing leaves the server up.
        assert_eq!(mgr.notify_close(7, "file:///proj/a.rs"), None);
        assert!(mgr.servers.contains_key(&key));
    }

    #[tokio::test]
    async fn change_before_open_is_dropped() {
        let key = LspServerKey { root: PathBuf::from("/proj"), language: "rust".into() };
        let (handle, mut ev) = ready_handle_to_mock(&key);
        let mut mgr = LspManager::default();
        mgr.servers.insert(key.clone(), handle);
        mgr.register_doc(7, &key);

        // No didOpen yet → didChange must not be sent (the server doesn't know the doc).
        mgr.notify_change(7, "file:///proj/x.rs", 2, "x");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), ev.recv()).await.is_err(),
            "didChange before didOpen should be suppressed"
        );
    }

    #[test]
    fn ensure_is_idempotent_and_bumps_generation() {
        let mut mgr = LspManager::default();
        let a = LspServerKey { root: PathBuf::from("/a"), language: "rust".into() };
        let b = LspServerKey { root: PathBuf::from("/b"), language: "go".into() };
        let g0 = mgr.ensure(&a, "rust-analyzer").expect("created");
        assert!(mgr.ensure(&a, "rust-analyzer").is_none(), "second ensure is a no-op");
        let g1 = mgr.ensure(&b, "gopls").expect("created");
        assert_ne!(g0, g1, "distinct handles get distinct generations");
    }

    #[test]
    fn status_snapshot_filters_by_project_root() {
        let mut mgr = LspManager::default();
        let in_proj = LspServerKey { root: PathBuf::from("/proj/a"), language: "rust".into() };
        let out_proj = LspServerKey { root: PathBuf::from("/other"), language: "go".into() };
        mgr.ensure(&in_proj, "rust-analyzer");
        mgr.ensure(&out_proj, "gopls");

        let snap = mgr.status_for_roots(&[PathBuf::from("/proj")]);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].language, "rust");
        assert!(matches!(snap[0].status, LspStatus::Starting));
    }

    #[test]
    fn workspace_configuration_reply_is_a_sized_null_array() {
        // One null per requested item (servers fall back to defaults) — a bare null is off-spec.
        let params = json!({"items": [{"section": "rust-analyzer"}, {"section": "files"}]});
        assert_eq!(
            server_request_response("workspace/configuration", &params),
            json!([null, null])
        );
        assert_eq!(server_request_response("workspace/configuration", &json!({})), json!([]));
        // Other server requests get a bare null.
        assert_eq!(server_request_response("workspace/applyEdit", &json!({})), json!(null));
    }
}
