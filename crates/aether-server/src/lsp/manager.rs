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
use std::time::{Duration, Instant};

use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::lsp::{LspProgress, LspServerStatus, LspStatus, LspStatusChanged};
use aether_protocol::BufferId;
use serde_json::Value;
use tokio::sync::mpsc;

use super::client::{LspClient, LspInbound};
use super::config::{self, LspServerSpec, WorkspaceMarker};
use super::position::PositionEncoding;
use super::{lifecycle, process, shell_env, uri};
use crate::state::{ServerState, SharedState};

/// Identifies a server instance: one per **workspace** per workspace root per language.
///
/// Keying by workspace (not just root) means two workspaces never share a server even when they
/// resolve to the same workspace root (overlapping/nested roots). Combined with the per-workspace
/// buffer scope — within a workspace a file is always exactly one buffer — this makes "one buffer
/// per URI per server" hold by construction, so LSP document sync (`didOpen`/`didChange`/
/// `didClose`, diagnostics) is never ambiguous. The cost is a redundant server when two workspaces
/// genuinely share a workspace root (uncommon); disjoint workspaces already had distinct roots and
/// are unaffected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LspServerKey {
    /// The owning workspace's id (`ServerState` workspace key).
    pub workspace: String,
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
    /// Active `$/progress` work-done operations, keyed by the (stringified) progress token. A
    /// server runs several at once (indexing, `cargo check`, …); non-empty means "busy". Drives the
    /// status-bar busy glyph and the LSP picker's progress display.
    pub progress: HashMap<String, LspProgress>,
    /// When we last pushed a progress update for this server, used to throttle the rapid `report`
    /// stream (see [`handle_progress`]). `None` until the first push.
    pub last_progress_push: Option<Instant>,
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
                progress: HashMap::new(),
                last_progress_push: None,
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
        let Some(h) = self.servers.get_mut(key) else {
            return;
        };
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
        let Some(key) = self.doc_server.get(&buffer_id) else {
            return;
        };
        let Some(h) = self.servers.get(key) else {
            return;
        };
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

    /// The current status of the language server backing `buffer_id`, if one is attached. Lets a
    /// freshly-subscribing client seed the status-bar health glyph without waiting for the next
    /// `lsp/status_changed` transition.
    pub fn status_for_buffer(&self, buffer_id: BufferId) -> Option<LspServerStatus> {
        let key = self.doc_server.get(&buffer_id)?;
        self.servers.get(key).map(handle_status)
    }

    /// Snapshot of every server owned by `workspace_id` — drives the LSP servers picker.
    /// Workspace-keyed (not root-keyed) so a workspace sees exactly its own servers,
    /// even when a sibling workspace shares its workspace root.
    pub fn status_for_workspace(&self, workspace_id: &str) -> Vec<LspServerStatus> {
        self.servers
            .iter()
            .filter(|(key, _)| key.workspace == workspace_id)
            .map(|(_, h)| handle_status(h))
            .collect()
    }
}

fn handle_status(h: &LspHandle) -> LspServerStatus {
    // Sort progress by title for a stable display order (the map's iteration order isn't).
    let mut progress: Vec<LspProgress> = h.progress.values().cloned().collect();
    progress.sort_by(|a, b| a.title.cmp(&b.title));
    LspServerStatus {
        name: h.server_name.clone(),
        language: h.language.clone(),
        workspace_root: h.workspace_root.display().to_string(),
        status: h.status.clone(),
        progress,
    }
}

/// Find the server root for `file`, searching ancestors up to (but not above) the workspace root
/// that contains it. Precedence:
/// 1. **Workspace root** — the *outermost* ancestor matching `workspace` (a Cargo `[workspace]` /
///    `go.work`), so a whole workspace gets one server instead of one per crate/module.
/// 2. else the **nearest** ancestor holding one of `root_markers`.
/// 3. else the workspace root, else the file's own directory.
pub fn discover_root(
    file: &Path,
    root_markers: &[&str],
    workspace: WorkspaceMarker,
    workspace_roots: &[PathBuf],
) -> PathBuf {
    let workspace_root = workspace_roots
        .iter()
        .filter(|r| file.starts_with(r))
        .max_by_key(|r| r.components().count());

    // Ancestor dirs from the file up to (and including) the workspace root — nearest first.
    let mut dirs: Vec<&Path> = Vec::new();
    let mut dir = file.parent();
    while let Some(d) = dir {
        dirs.push(d);
        match workspace_root {
            Some(pr) if d == pr => break, // don't climb above the workspace root
            Some(_) => dir = d.parent(),
            None => break, // no workspace context: only the file's own directory
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
    workspace_root
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
    // Resolve the toolchain environment for this root (mise/direnv/asdf/… activation), falling back
    // to the daemon's own environment when there's nothing to add. See [`shell_env`].
    let env = shell_env::resolve(&key.root).await;
    let proc = match process::spawn(spec.command, spec.args, &key.root, env.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(server = %key.language, error = %e, "failed to spawn language server");
            set_status(
                &state,
                &key,
                generation,
                LspStatus::Crashed {
                    code: None,
                    message: format!("spawn failed: {e}"),
                },
            )
            .await;
            return;
        }
    };
    bring_up(
        &state,
        key,
        generation,
        proc.client,
        proc.inbound,
        Some(proc.child),
    )
    .await;
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
            set_status(
                state,
                &key,
                generation,
                LspStatus::Crashed {
                    code: None,
                    message: format!("handshake failed: {e}"),
                },
            )
            .await;
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
        let registered: Vec<BufferId> = s.lsp.servers[&key]
            .registered_buffers
            .iter()
            .copied()
            .collect();
        for bid in registered {
            if s.lsp.servers[&key].open_buffers.contains(&bid) {
                continue;
            }
            let Some(buf) = s.buffers.get(&bid) else {
                continue;
            };
            let Some(path) = buf.canonical_path.as_deref() else {
                continue;
            };
            let doc_uri = uri::path_to_uri(path);
            let text = buf.text.to_string();
            let version = buf.revision as i64;
            if lifecycle::did_open(&client, &doc_uri, &key.language, version, &text).is_ok() {
                s.lsp
                    .servers
                    .get_mut(&key)
                    .expect("present")
                    .open_buffers
                    .insert(bid);
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
            LspInbound::Notification { method, params }
                if method == "textDocument/publishDiagnostics" =>
            {
                let count = params
                    .get("diagnostics")
                    .and_then(|d| d.as_array())
                    .map_or(0, Vec::len);
                tracing::debug!(server = %key.language, count, "lsp diagnostics");
                handle_publish_diagnostics(&state, &key, &params).await;
            }
            LspInbound::Notification { method, params } if method == "$/progress" => {
                let pushes = handle_progress(&state, &key, &params).await;
                send_all(pushes).await;
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
    set_status(
        &state,
        &key,
        generation,
        LspStatus::Crashed {
            code: None,
            message: "connection closed".into(),
        },
    )
    .await;
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
            let n = params
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; n])
        }
        _ => Value::Null,
    }
}

/// Smallest gap between successive `report`-driven picker refreshes for one server. Servers stream
/// `$/progress` reports rapidly during indexing / `cargo check`; coalescing to this rate keeps the
/// percentage roughly live without flooding the WebSocket (the map is always current, so the next
/// push that does go out carries the latest value).
const PROGRESS_PUSH_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Whether a throttled `report` push is due: always for the first one, otherwise once the minimum
/// interval has elapsed since the last progress push.
fn report_due(last: Option<Instant>, now: Instant, min_interval: Duration) -> bool {
    match last {
        Some(t) => now.saturating_duration_since(t) >= min_interval,
        None => true,
    }
}

/// Apply a `$/progress` work-done notification and return the pushes that surface it. A `begin`
/// adds an entry (keyed by the progress token), `report` updates its message/percentage, `end`
/// removes it; ignored for an unknown server, a missing token, or a `report`/`end` for a token we
/// never saw begin.
///
/// Fan-out is split by kind to keep the socket quiet. `begin`/`end` change the *busy* state (the
/// progress map goes non-empty / empty), so they broadcast `lsp/status_changed` to every workspace
/// client (the status-bar glyph) and refresh open LSP pickers. A `report` changes only the
/// percentage/message — not busy-ness — so it skips the broadcast entirely and only refreshes open
/// LSP pickers (the detail / dialog), and even that is throttled, since reports stream rapidly.
/// In the common case (no LSP picker open anywhere) a `report` produces no messages at all.
async fn handle_progress(
    state: &SharedState,
    key: &LspServerKey,
    params: &Value,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let token = match params.get("token") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(), // numeric tokens → their JSON form
        None => return Vec::new(),
    };
    let Some(value) = params.get("value") else {
        return Vec::new();
    };
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    let message = || {
        value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let percentage = || {
        value
            .get("percentage")
            .and_then(Value::as_u64)
            .map(|p| p as u32)
    };
    let now = Instant::now();

    let mut guard = state.lock().await;
    let Some(h) = guard.lsp.servers.get_mut(key) else {
        return Vec::new();
    };
    // Update the in-memory map (always — so any later push carries the current value), and note
    // whether this was a `report` (busy unchanged → throttled, picker-only fan-out).
    let report_only = match kind {
        "begin" => {
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("working")
                .to_string();
            h.progress.insert(
                token,
                LspProgress {
                    title,
                    message: message(),
                    percentage: percentage(),
                },
            );
            false
        }
        "report" => {
            let Some(entry) = h.progress.get_mut(&token) else {
                return Vec::new();
            };
            if let Some(m) = message() {
                entry.message = Some(m);
            }
            if let Some(p) = percentage() {
                entry.percentage = Some(p);
            }
            true
        }
        "end" => {
            if h.progress.remove(&token).is_none() {
                return Vec::new();
            }
            false
        }
        _ => return Vec::new(),
    };
    if report_only && !report_due(h.last_progress_push, now, PROGRESS_PUSH_MIN_INTERVAL) {
        return Vec::new();
    }
    h.last_progress_push = Some(now);

    let mut out = Vec::new();
    if !report_only {
        // Busy ↔ idle changed: tell every workspace client so their status-bar glyph updates.
        out.extend(collect_status_pushes(&guard, key));
    }
    // Refresh the LSP picker for clients that have it open — the only ones showing the detail / %.
    out.extend(crate::handlers::refresh_lsp_server_pickers(&mut guard));
    out
}

/// Handle a `publishDiagnostics` payload. Always records the file's diagnostics path-keyed and
/// line-granular in `path_diagnostics` — the workspace picker's source, populated for every file a
/// server reports (rust-analyzer's flycheck pushes cover the whole build, opened or not). If the
/// file is also open as a buffer, *separately* converts to byte columns against the buffer text,
/// stores that as the live buffer-keyed set, and re-renders the buffer's viewports (squiggles). The
/// two stores are independent (no merge); an empty push clears both.
async fn handle_publish_diagnostics(state: &SharedState, key: &LspServerKey, params: &Value) {
    let Some(doc_uri) = params.get("uri").and_then(Value::as_str) else {
        return;
    };
    let Some(path) = uri::uri_to_path(doc_uri) else {
        return;
    };
    let diags_json = params.get("diagnostics").cloned().unwrap_or(Value::Null);

    let (pushes, buffer_id) = {
        let mut guard = state.lock().await;
        let s = &mut *guard;

        // Path-keyed line-only store (the workspace picker's source): always updated, for open and
        // closed files alike. An empty push removes the entry so a fixed file stops showing.
        let raw = super::diagnostics::raw_from_lsp(&diags_json);
        if raw.is_empty() {
            s.path_diagnostics.remove(&path);
        } else {
            s.path_diagnostics.insert(path.clone(), raw);
        }
        // Live-refresh any open `Space Alt-d`: it reads `path_diagnostics`, so this push may add or
        // change a file's rows. No-op when no client has the workspace picker open (the common case),
        // and it fires for closed files too — that's how a never-opened file's diagnostics appear.
        let mut pushes = crate::handlers::refresh_workspace_diagnostics_pickers(s);

        // Route to the buffer in *this server's* workspace: a file open in two workspaces has a buffer
        // in each, and each workspace's server publishes for its own buffer (with per-workspace keying
        // there's exactly one such buffer per server).
        let owner = key.workspace.as_str();
        let buffer_id = s.buffers.iter().find_map(|(id, b)| {
            (b.canonical_path.as_deref() == Some(path.as_path())
                && s.buffer_workspaces.get(id).map(String::as_str) == Some(owner))
            .then_some(*id)
        });
        match buffer_id {
            // No open buffer: the path-keyed store + workspace-picker refresh above are all there is —
            // no squiggles to render and no symbol outline to refresh for a file we don't have open.
            None => (pushes, None),
            Some(buffer_id) => {
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
                pushes.extend(crate::handlers::set_diagnostics_and_refresh(
                    s, buffer_id, diags,
                ));
                (pushes, Some(buffer_id))
            }
        }
    };
    // A fresh diagnostics publish for an OPEN buffer means the server just re-analyzed it, so its
    // symbol outline may have changed too — re-fetch it (naturally debounced by the analysis cycle).
    // This keeps the `o` symbol-navigation motion and the `Space o` outline in sync with edits.
    if let Some(buffer_id) = buffer_id {
        crate::handlers::spawn_document_symbol_refresh(state.clone(), buffer_id);
    }
    send_all(pushes).await;
}

/// Restart every server for `language` owned by `workspace_id`: tear down the old process and
/// relaunch, re-registering the documents that were open against it so they reopen once the new
/// process is ready. Workspace-keyed, so it never disturbs a sibling workspace's server.
pub async fn restart(state: &SharedState, language: &str, workspace_id: &str) {
    let keys: Vec<LspServerKey> = {
        let guard = state.lock().await;
        guard
            .lsp
            .servers
            .keys()
            .filter(|k| k.language == language && k.workspace == workspace_id)
            .cloned()
            .collect()
    };

    for key in keys {
        let Some(spec) = config::server_spec(&key.language) else {
            continue;
        };
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
        let Some(h) = guard.lsp.servers.get_mut(key) else {
            return;
        };
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

/// Build `lsp/status_changed` notifications for every client whose active workspace contains `key`'s
/// root.
fn collect_status_pushes(
    s: &ServerState,
    key: &LspServerKey,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let Some(handle) = s.lsp.servers.get(key) else {
        return Vec::new();
    };
    let params = serde_json::to_value(handle_status(handle)).expect("infallible");
    s.clients
        .values()
        .filter(|c| c.active_workspace.as_deref() == Some(key.workspace.as_str()))
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

        let found = discover_root(
            &file,
            &["Cargo.toml"],
            WorkspaceMarker::None,
            &[root.to_path_buf()],
        );
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

        let found = discover_root(
            &file,
            &["Cargo.toml"],
            WorkspaceMarker::None,
            &[root.to_path_buf()],
        );
        assert_eq!(found, inner);
    }

    #[test]
    fn discover_root_falls_back_to_workspace_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("main.rs");
        std::fs::write(&file, "").unwrap();

        let found = discover_root(
            &file,
            &["Cargo.toml"],
            WorkspaceMarker::None,
            &[root.to_path_buf()],
        );
        assert_eq!(found, root);
    }

    #[test]
    fn discover_root_prefers_cargo_workspace_over_crate() {
        // Workspace root (Cargo.toml with `[workspace]`) + a member crate with its own Cargo.toml.
        // A file in the member must resolve to the *workspace* root, not the crate — one
        // rust-analyzer for the whole workspace.
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/x\"]\n",
        )
        .unwrap();
        let crate_dir = root.join("crates/x");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let file = crate_dir.join("src/main.rs");
        std::fs::write(&file, "").unwrap();

        let ws = WorkspaceMarker::FileContaining {
            file: "Cargo.toml",
            needle: "[workspace]",
        };
        let found = discover_root(&file, &["Cargo.toml"], ws, &[root.to_path_buf()]);
        assert_eq!(
            found, root,
            "should resolve to the workspace root, not the crate"
        );
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

        let ws = WorkspaceMarker::FileContaining {
            file: "Cargo.toml",
            needle: "[workspace]",
        };
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
    async fn mock_server<R, W>(
        reader: R,
        mut writer: W,
        events: mpsc::UnboundedSender<(String, Value)>,
    ) where
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
            progress: HashMap::new(),
            last_progress_push: None,
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

    /// A `Ready` handle with no client/process attached — enough to exercise progress tracking,
    /// which only touches the in-memory map.
    fn clientless_ready_handle(key: &LspServerKey) -> LspHandle {
        LspHandle {
            language: key.language.clone(),
            workspace_root: key.root.clone(),
            server_name: "mock".into(),
            status: LspStatus::Ready,
            generation: 0,
            client: None,
            position_encoding: PositionEncoding::Utf8,
            document_formatting: false,
            open_buffers: HashSet::new(),
            registered_buffers: HashSet::new(),
            progress: HashMap::new(),
            last_progress_push: None,
            child: None,
        }
    }

    #[test]
    fn report_due_throttles_to_the_min_interval() {
        let t0 = Instant::now();
        let interval = Duration::from_millis(100);
        // First report always goes out.
        assert!(report_due(None, t0, interval));
        // Within the window → suppressed; at/after the window → due again.
        assert!(!report_due(
            Some(t0),
            t0 + Duration::from_millis(40),
            interval
        ));
        assert!(!report_due(
            Some(t0),
            t0 + Duration::from_millis(99),
            interval
        ));
        assert!(report_due(
            Some(t0),
            t0 + Duration::from_millis(100),
            interval
        ));
        assert!(report_due(
            Some(t0),
            t0 + Duration::from_millis(250),
            interval
        ));
    }

    #[tokio::test]
    async fn progress_begin_report_end_tracks_active_work() {
        let key = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj"),
            language: "rust".into(),
        };
        let mut st = ServerState::new();
        st.lsp
            .servers
            .insert(key.clone(), clientless_ready_handle(&key));
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(st));

        let progress = |state: &SharedState, key: &LspServerKey, params: Value| {
            let state = state.clone();
            let key = key.clone();
            async move {
                let _ = handle_progress(&state, &key, &params).await;
            }
        };

        // begin → one active operation, captured title.
        progress(
            &state,
            &key,
            json!({
                "token": "idx", "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
            }),
        )
        .await;
        {
            let g = state.lock().await;
            let p = handle_status(&g.lsp.servers[&key]).progress;
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].title, "Indexing");
            assert_eq!(p[0].percentage, Some(0));
        }

        // A second concurrent token, plus a report that updates the first.
        progress(
            &state,
            &key,
            json!({
                "token": "chk", "value": { "kind": "begin", "title": "cargo check" }
            }),
        )
        .await;
        progress(&state, &key, json!({
            "token": "idx", "value": { "kind": "report", "message": "120/430", "percentage": 28 }
        })).await;
        {
            let g = state.lock().await;
            let p = handle_status(&g.lsp.servers[&key]).progress; // sorted by title
            assert_eq!(p.len(), 2);
            assert_eq!(p[0].title, "Indexing");
            assert_eq!(p[0].message.as_deref(), Some("120/430"));
            assert_eq!(p[0].percentage, Some(28));
            assert_eq!(p[1].title, "cargo check");
        }

        // end removes just that token; the other stays active (still busy).
        progress(
            &state,
            &key,
            json!({ "token": "idx", "value": { "kind": "end" } }),
        )
        .await;
        {
            let g = state.lock().await;
            let p = handle_status(&g.lsp.servers[&key]).progress;
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].title, "cargo check");
        }

        // Final end → idle (no progress, glyph goes back to ●).
        progress(
            &state,
            &key,
            json!({ "token": "chk", "value": { "kind": "end" } }),
        )
        .await;
        {
            let g = state.lock().await;
            assert!(handle_status(&g.lsp.servers[&key]).progress.is_empty());
        }
    }

    #[tokio::test]
    async fn open_change_close_reach_the_server() {
        let key = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj"),
            language: "rust".into(),
        };
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
            tokio::time::timeout(Duration::from_millis(150), ev.recv())
                .await
                .is_err(),
            "no message expected after close"
        );
    }

    #[tokio::test]
    async fn notify_close_tears_down_idle_server() {
        let key = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj"),
            language: "rust".into(),
        };
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
        let key = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj"),
            language: "rust".into(),
        };
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
        let key = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj"),
            language: "rust".into(),
        };
        let (handle, mut ev) = ready_handle_to_mock(&key);
        let mut mgr = LspManager::default();
        mgr.servers.insert(key.clone(), handle);
        mgr.register_doc(7, &key);

        // No didOpen yet → didChange must not be sent (the server doesn't know the doc).
        mgr.notify_change(7, "file:///proj/x.rs", 2, "x");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), ev.recv())
                .await
                .is_err(),
            "didChange before didOpen should be suppressed"
        );
    }

    #[test]
    fn ensure_is_idempotent_and_bumps_generation() {
        let mut mgr = LspManager::default();
        let a = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/a"),
            language: "rust".into(),
        };
        let b = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/b"),
            language: "go".into(),
        };
        let g0 = mgr.ensure(&a, "rust-analyzer").expect("created");
        assert!(
            mgr.ensure(&a, "rust-analyzer").is_none(),
            "second ensure is a no-op"
        );
        let g1 = mgr.ensure(&b, "gopls").expect("created");
        assert_ne!(g0, g1, "distinct handles get distinct generations");
    }

    #[test]
    fn same_root_different_workspace_are_distinct_servers() {
        // The same file open in two workspaces (overlapping/nested roots resolve to one workspace
        // root) must get a server *per workspace*, so each holds exactly one buffer for the URI —
        // no double didOpen / premature didClose.
        let mut mgr = LspManager::default();
        let a = LspServerKey {
            workspace: "a".into(),
            root: PathBuf::from("/shared"),
            language: "rust".into(),
        };
        let b = LspServerKey {
            workspace: "b".into(),
            root: PathBuf::from("/shared"),
            language: "rust".into(),
        };
        assert!(mgr.ensure(&a, "rust-analyzer").is_some());
        assert!(
            mgr.ensure(&b, "rust-analyzer").is_some(),
            "a different workspace at the same root is a separate server"
        );
        assert_eq!(mgr.servers.len(), 2);
    }

    #[test]
    fn status_snapshot_filters_by_workspace() {
        let mut mgr = LspManager::default();
        let mine = LspServerKey {
            workspace: "proj".into(),
            root: PathBuf::from("/proj/a"),
            language: "rust".into(),
        };
        // Same workspace root, *different workspace* — must not show in `proj`'s snapshot, which is
        // exactly the per-workspace isolation (root-keying would have leaked it).
        let other = LspServerKey {
            workspace: "other".into(),
            root: PathBuf::from("/proj/a"),
            language: "go".into(),
        };
        mgr.ensure(&mine, "rust-analyzer");
        mgr.ensure(&other, "gopls");

        let snap = mgr.status_for_workspace("proj");
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
        assert_eq!(
            server_request_response("workspace/configuration", &json!({})),
            json!([])
        );
        // Other server requests get a bare null.
        assert_eq!(
            server_request_response("workspace/applyEdit", &json!({})),
            json!(null)
        );
    }
}
