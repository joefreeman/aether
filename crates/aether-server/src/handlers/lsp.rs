//! `lsp/*` — diagnostics, hover, definitions, references, symbols, formatting, and the async candidate resolvers.

use super::*;

/// The buffer's language-server diagnostics, or an empty slice when none are known.
pub fn buffer_diagnostics(
    s: &ServerState,
    buffer_id: BufferId,
) -> &[crate::lsp::diagnostics::BufferDiagnostic] {
    s.diagnostics
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Notify the language server of a buffer's new full text (LSP `didChange`). Must be called by
/// *every* path that changes buffer text — edits, undo/redo, reload — or the server's analysis
/// (and its diagnostics) goes stale. A no-op unless the buffer is file-backed and open against a
/// ready server; `notify` is a channel send, so it's fire-and-forget under the lock. The server
/// re-publishes diagnostics on its own after the `didChange`, which the `publishDiagnostics` hook
/// then renders.
pub fn notify_lsp_change(s: &mut ServerState, buffer_id: BufferId) {
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return;
    };
    let Some(uri) = buf
        .canonical_path
        .as_deref()
        .map(crate::lsp::uri::path_to_uri)
    else {
        return;
    };
    let revision = buf.revision as i64;
    let text = buf.text.to_string();
    let Some(document) = s.buffers.get(&buffer_id).map(|b| b.document) else {
        return;
    };
    // One `didChange` per *server*, not per buffer. An edit through one workspace must still reach
    // every server holding the file — two workspaces at different roots are two processes — but
    // several buffers of one document can now share a process, and sending it the same version once
    // per buffer is a protocol violation as well as duplicated reparse work.
    let keys: std::collections::HashSet<crate::lsp::manager::LspServerKey> = s
        .doc_siblings(buffer_id)
        .into_iter()
        .filter_map(|id| s.lsp.doc_server.get(&id).cloned())
        .collect();
    for key in keys {
        s.lsp.notify_change(document, &key, &uri, revision, &text);
    }
}

/// Build the diagnostics-picker candidates for `buffer_id`: one per diagnostic, sorted top-to-bottom
/// by position, carrying the buffer's path for the `FileAt` jump. Empty if the buffer is gone or has
/// no path.
pub fn build_diagnostic_candidates(
    s: &ServerState,
    buffer_id: BufferId,
) -> Vec<picker_state::DiagnosticCandidate> {
    let Some(abs_path) = s
        .try_doc_of(buffer_id)
        .and_then(|b| b.canonical_path.as_deref())
        .map(|p| p.display().to_string())
    else {
        return Vec::new();
    };
    let mut out: Vec<picker_state::DiagnosticCandidate> = buffer_diagnostics(s, buffer_id)
        .iter()
        .map(|d| picker_state::DiagnosticCandidate {
            // The buffer-scoped picker renders flat and opens the current buffer, so it needs no
            // path-index/relative-path — an empty relative path is the "unset" sentinel. The
            // jumplist capture treats that as absent and re-derives the parts from `abs_path`
            // (see `jumplist::relative_parts` / `assign_file_groups`), so don't rely on these here.
            path_index: 0,
            relative_path: String::new(),
            line: d.start.line,
            col: d.start.col,
            end_line: d.end.line,
            end_col: d.end.col,
            severity: d.severity,
            message: d.message.clone(),
            abs_path: abs_path.clone(),
        })
        .collect();
    out.sort_by_key(|c| (c.line, c.col));
    out
}

/// Build the **workspace-wide** diagnostics candidates (the `Space Alt-d` picker), grouped by file.
/// Reads a single source — the path-keyed [`ServerState::path_diagnostics`], which `publishDiagnostics`
/// fills for every file a server reports (rust-analyzer's flycheck covers the whole build). This is
/// kept *separate* from the buffer-scoped picker's live byte-precise set ([`ServerState::diagnostics`]),
/// not merged: every workspace row is line-granular (`col: 0`, lands on the line start), so the list is
/// uniform — "diagnostics as of the last analysis/check" — rather than mixing live and reported rows.
///
/// Synchronous — no LSP round-trip (no configured server answers the workspace pull; everything is
/// already stored). Sorted by (file, line) so the picker groups rows under their file header.
pub fn build_workspace_diagnostic_candidates(
    s: &ServerState,
    client_id: ClientId,
) -> Vec<picker_state::DiagnosticCandidate> {
    let Some(workspace) = s.active_workspace(client_id) else {
        return Vec::new();
    };
    let roots = workspace.paths.clone();

    let mut out: Vec<picker_state::DiagnosticCandidate> = Vec::new();
    for (path, diags) in &s.path_diagnostics {
        let Some((path_index, relative_path)) =
            crate::workspace_index::workspace_relative_parts(path, &roots)
        else {
            continue; // outside the active workspace
        };
        let abs_path = path.to_string_lossy().into_owned();
        for d in diags {
            out.push(picker_state::DiagnosticCandidate {
                path_index,
                relative_path: relative_path.clone(),
                line: d.line,
                col: 0, // line-granular: no buffer text to resolve a byte column
                end_line: d.line,
                end_col: 0,
                severity: d.severity,
                message: d.message.clone(),
                abs_path: abs_path.clone(),
            });
        }
    }

    out.sort_by(|a, b| {
        (a.path_index, a.relative_path.as_str(), a.line).cmp(&(
            b.path_index,
            b.relative_path.as_str(),
            b.line,
        ))
    });
    out
}

/// Build the references-picker candidates: ask the language server for every reference to the
/// symbol at the cursor (`textDocument/references`, including the declaration), then attach a
/// line-text preview and a display label to each. A parallel `textDocument/definition` resolves
/// which of those locations is the symbol's definition, so the picker can split into a `Definition`
/// section and a `References` section (candidates come back ordered definition-first). Returns empty
/// when there's no ready server, the server resolves nothing, or the request fails. Async (off the
/// lock): the two LSP round-trips plus reading each referenced file's line from disk.
async fn build_reference_candidates(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> AsyncResolveAttempt<Vec<picker_state::ReferenceCandidate>> {
    // Resolve the LSP request and the workspace roots under the lock, then release it for the I/O.
    let (resolve, roots) = {
        let s = state.lock().await;
        let roots = s
            .active_workspace(client_id)
            .map(|p| p.paths.clone())
            .unwrap_or_default();
        (lsp_cursor_request(&s, client_id, buffer_id), roots)
    };
    if matches!(resolve, CursorResolve::Starting) {
        return AsyncResolveAttempt::ServerBusy;
    }
    let Some(req) = resolve.ready() else {
        return AsyncResolveAttempt::Done(Vec::new());
    };
    let refs_params = serde_json::json!({
        "textDocument": { "uri": req.uri.clone() },
        "position": { "line": req.line, "character": req.character },
        "context": { "includeDeclaration": true },
    });
    let locations = match req
        .client
        .request("textDocument/references", refs_params)
        .await
    {
        Ok(v) => parse_references(&v, req.encoding),
        // A timeout from a server mid-`$/progress` (indexing) usually means the answer is queued
        // behind the work, not lost — retry so the picker fills when it lands. Any other error
        // (or a timeout from an idle server) settles empty as before.
        Err(crate::lsp::client::LspError::Timeout)
            if buffer_server_busy(state, buffer_id).await =>
        {
            return AsyncResolveAttempt::ServerBusy;
        }
        Err(e) => {
            tracing::debug!(error = %e, "lsp references request failed");
            return AsyncResolveAttempt::Done(Vec::new());
        }
    };
    // Resolve the definition in parallel so its reference row can be split into the Definition
    // section. Non-fatal: on failure (or a server without goto-definition) every row stays a use
    // and the picker shows a single References section.
    let def_params = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
    });
    let definition = match req
        .client
        .request("textDocument/definition", def_params)
        .await
    {
        Ok(v) => parse_definition(&v, req.encoding),
        Err(e) => {
            tracing::debug!(error = %e, "lsp definition request (for references split) failed");
            None
        }
    };

    // Cache each referenced file's lines so a file with many references is read only once. `None`
    // marks a file we couldn't read — its previews fall back to empty.
    let mut file_lines: HashMap<String, Option<Vec<String>>> = HashMap::new();
    let mut out: Vec<picker_state::ReferenceCandidate> = locations
        .into_iter()
        // Workspace-only: a reference that doesn't live under any workspace root (a dependency, the
        // stdlib, generated code outside the tree) is dropped — `workspace_relative_parts` is the
        // gate, and its relative path becomes the display label.
        .filter_map(|loc| {
            let (_, display_path) = crate::workspace_index::workspace_relative_parts(
                std::path::Path::new(&loc.path),
                &roots,
            )?;
            let lines = file_lines.entry(loc.path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&loc.path)
                    .ok()
                    .map(|c| c.lines().map(str::to_string).collect())
            });
            let preview = lines
                .as_ref()
                .and_then(|ls| ls.get(loc.position.line as usize))
                .map(|l| l.trim_end().to_string())
                .unwrap_or_default();
            Some(picker_state::ReferenceCandidate {
                abs_path: loc.path,
                display_path,
                line: loc.position.line,
                col: loc.position.col,
                end_line: loc.end.line,
                end_col: loc.end.col,
                preview,
                is_definition: false,
            })
        })
        .collect();
    // Stable, file-grouped order: by display path, then position. Dedup identical locations (some
    // servers return the declaration twice, or overlapping ranges collapse to the same start).
    out.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then_with(|| (a.line, a.col).cmp(&(b.line, b.col)))
    });
    out.dedup_by(|a, b| a.abs_path == b.abs_path && a.line == b.line && a.col == b.col);
    // Flag the reference that is the definition. Exact `(path, line, col)` first; else the nearest
    // column on the same `(path, line)` — goto-definition may report the name's selection-range
    // start a few columns off from the reference entry. At most one row is flagged.
    if let Some(def) = definition {
        let exact = out.iter().position(|c| {
            c.abs_path == def.path && c.line == def.position.line && c.col == def.position.col
        });
        let chosen = exact.or_else(|| {
            out.iter()
                .enumerate()
                .filter(|(_, c)| c.abs_path == def.path && c.line == def.position.line)
                .min_by_key(|(_, c)| c.col.abs_diff(def.position.col))
                .map(|(i, _)| i)
        });
        if let Some(i) = chosen {
            out[i].is_definition = true;
        }
    }
    // Definition first, then the file-grouped order within each section. Stable `sort_by_key`, so
    // the `(display_path, line, col)` ordering set above survives inside each section.
    out.sort_by_key(|c| !c.is_definition);
    AsyncResolveAttempt::Done(out)
}

/// Build the document-symbols-picker candidates: ask the language server for the picked buffer's
/// symbols (`textDocument/documentSymbol`), flattening any hierarchy into a depth-tagged list.
/// `Done` carries the candidates plus the cursor's buffer position (for the initial
/// cursor-enclosing highlight) — both empty/None when there's no usable server, the buffer isn't
/// file-backed, or the request fails; `ServerStarting` asks the caller to retry once the server's
/// handshake lands. Async (off the lock): one LSP round-trip.
async fn build_symbol_candidates(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> AsyncResolveAttempt<(Vec<picker_state::SymbolCandidate>, Option<LogicalPosition>)> {
    let (resolve, abs_path, cursor, text) = {
        let s = state.lock().await;
        let abs_path = s
            .try_doc_of(buffer_id)
            .and_then(|b| b.canonical_path.clone());
        // The cursor's byte position (for centering on the enclosing symbol), in buffer coords —
        // not the LSP-encoded one in `req`.
        let cursor = s.cursors.get(&(client_id, buffer_id)).map(|c| c.position);
        // Snapshot of the live text for position conversion (a rope clone is cheap) — the server's
        // positions describe the synced buffer content, not whatever is on disk.
        let text = s.try_doc_of(buffer_id).map(|b| b.text.clone());
        (
            lsp_cursor_request(&s, client_id, buffer_id),
            abs_path,
            cursor,
            text,
        )
    };
    if matches!(resolve, CursorResolve::Starting) {
        return AsyncResolveAttempt::ServerBusy;
    }
    let (Some(req), Some(abs_path), Some(text)) = (resolve.ready(), abs_path, text) else {
        return AsyncResolveAttempt::Done((Vec::new(), None));
    };
    // documentSymbol is whole-document: only the URI is needed (no cursor position).
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
    });
    let candidates = match req
        .client
        .request("textDocument/documentSymbol", params_json)
        .await
    {
        Ok(v) => parse_document_symbols(&v, &abs_path.display().to_string(), req.encoding, &text),
        // See `build_reference_candidates`: an indexing server's timeout means "queued", not
        // "no symbols" — retry rather than settling an answer that goes stale when it lands.
        Err(crate::lsp::client::LspError::Timeout)
            if buffer_server_busy(state, buffer_id).await =>
        {
            return AsyncResolveAttempt::ServerBusy;
        }
        Err(e) => {
            tracing::debug!(error = %e, "lsp documentSymbol request failed");
            Vec::new()
        }
    };
    AsyncResolveAttempt::Done((candidates, cursor))
}

/// Everything needed to ask a buffer's language server for its document-symbol outline, resolved
/// under the lock so the caller can drop it before the (awaited) LSP round-trip. Unlike
/// [`LspCursorRequest`] this is client-agnostic — `documentSymbol` is whole-document — so it can be
/// driven by background refreshes (e.g. the `publishDiagnostics` hook) that have no client.
struct LspDocSymbolRequest {
    client: crate::lsp::client::LspClient,
    uri: String,
    encoding: crate::lsp::position::PositionEncoding,
    abs_path: String,
    /// Snapshot of the live text for position conversion (a rope clone is cheap) — the server's
    /// positions describe the synced buffer content, not whatever is on disk.
    text: ropey::Rope,
}

fn lsp_doc_symbol_request(s: &ServerState, buffer_id: BufferId) -> Option<LspDocSymbolRequest> {
    let buf = s.try_doc_of(buffer_id)?;
    let path = buf.canonical_path.as_deref()?;
    let key = s.lsp.doc_server.get(&buffer_id)?;
    let handle = s.lsp.servers.get(key)?;
    if !matches!(handle.status, LspStatus::Ready) {
        return None;
    }
    Some(LspDocSymbolRequest {
        client: handle.client.clone()?,
        uri: crate::lsp::uri::path_to_uri(path),
        encoding: handle.position_encoding,
        abs_path: path.display().to_string(),
        text: buf.text.clone(),
    })
}

/// Re-fetch a buffer's document-symbol outline and store it in `state.document_symbols`, keyed by
/// the revision it was requested against. A no-op when the buffer has no ready server. Drives both
/// the `Space o` picker warmth and the `o` symbol-navigation motion. Off-lock for the LSP
/// round-trip; re-locks only to read the request and to store the result.
pub async fn refresh_document_symbols(state: &SharedState, buffer_id: BufferId) {
    let req = {
        let s = state.lock().await;
        lsp_doc_symbol_request(&s, buffer_id)
    };
    let Some(req) = req else { return };
    let params_json = serde_json::json!({ "textDocument": { "uri": req.uri } });
    let symbols = match req
        .client
        .request("textDocument/documentSymbol", params_json)
        .await
    {
        Ok(v) => parse_document_symbols(&v, &req.abs_path, req.encoding, &req.text),
        Err(e) => {
            tracing::debug!(error = %e, "lsp documentSymbol refresh failed");
            return; // keep any previous cache rather than blanking it on a transient failure
        }
    };
    let pushes = {
        let mut s = state.lock().await;
        if !s.buffers.contains_key(&buffer_id) {
            return;
        }
        s.document_symbols.insert(buffer_id, symbols);
        // The outline typically lands a beat *after* the buffer opens, with the cursor sitting
        // still — no cursor response to ride, so the breadcrumb would stay blank until the next
        // keypress without this.
        collect_symbol_path_pushes(&mut s, buffer_id)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Fire-and-forget [`refresh_document_symbols`] on the runtime — for callers holding the lock or in
/// a sync context (buffer open, the `publishDiagnostics` hook).
pub fn spawn_document_symbol_refresh(state: SharedState, buffer_id: BufferId) {
    tokio::spawn(async move {
        refresh_document_symbols(&state, buffer_id).await;
    });
}

/// The status-bar breadcrumb for `(client, buffer)`: the outline symbols enclosing that client's
/// cursor, outermost first (`impl Foo` › `fn bar`). Empty when the buffer has no cached outline —
/// no language server, or the first `textDocument/documentSymbol` hasn't landed — or the cursor
/// sits outside every symbol, which is the honest answer between two top-level items.
///
/// Cheap enough to run inline under the lock (a linear scan of the cached outline, no I/O), unlike
/// the blame and symbol-highlight followers that spawn for an LSP round-trip.
///
/// The cached outline goes stale between refreshes — it's re-fetched on open and on
/// `publishDiagnostics`, so ranges drift as you type until the server re-analyses. That's the same
/// staleness the `o` motion already navigates by, so the breadcrumb and the motion always agree
/// with each other, which matters more than either agreeing with the un-analysed text.
pub fn symbol_path_for(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Vec<SymbolCrumb> {
    // A **composed** view has an outline of its own — its changes, grouped by file and labelled by
    // the enclosing signature — and the breadcrumb is the path through *that* to the cursor. Asking
    // the language server instead would describe the focused file in terms the outline never uses,
    // so `Space o`, `o`/`Alt-o` and the status bar would each name the same position differently.
    //
    // Checked before the LSP path rather than after: the composed answer is the whole answer there,
    // not a prefix to decorate one with.
    if let Some(path) = crate::handlers::viewport::outline_breadcrumb(s, client_id, buffer_id) {
        return path;
    }
    let (Some(symbols), Some(cursor)) = (
        s.document_symbols.get(&buffer_id),
        s.cursors.get(&(client_id, buffer_id)),
    ) else {
        return Vec::new();
    };
    crate::cursor::enclosing_chain(symbols, cursor.position)
        .into_iter()
        .map(|i| SymbolCrumb {
            name: symbols[i].name.clone(),
            kind: symbols[i].symbol_kind,
        })
        .collect()
}

/// Recompute the breadcrumb for every client viewing `buffer_id`, pushing `lsp/symbol_path_changed`
/// to those whose path actually changed. Moving *within* a function changes nothing, so the common
/// case is a scan and no push at all.
///
/// Mutates `symbol_path_sent` as it goes: a caller that collects these and drops them would
/// suppress the *next* genuine change, so send what this returns.
pub fn collect_symbol_path_pushes(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut clients: Vec<ClientId> = s
        .viewports
        .values()
        .filter(|vp| s.view_of(vp).binds(buffer_id))
        .map(|vp| vp.client_id)
        .collect();
    clients.sort_unstable();
    clients.dedup();

    let mut pushes = Vec::new();
    for client_id in clients {
        let path = symbol_path_for(s, client_id, buffer_id);
        let key = (client_id, buffer_id);
        // "Never sent" reads as empty, so a buffer that never gets an outline (no server) stays
        // silent instead of pushing one empty path per open.
        if s.symbol_path_sent
            .get(&key)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            == path.as_slice()
        {
            continue;
        }
        s.symbol_path_sent.insert(key, path.clone());
        let Some(sender) = s.clients.get(&client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((
            sender,
            Notification {
                jsonrpc: JsonRpc,
                method: LspSymbolPathChanged::NAME.into(),
                params: serde_json::to_value(LspSymbolPathChangedParams { buffer_id, path })
                    .unwrap_or(serde_json::Value::Null),
            },
        ));
    }
    pushes
}

/// Monotonic token minted per async-resolve picker open (References, DocumentSymbols), stored on
/// the picker as `pending_async_load`. Lets a spawned resolve detect that its picker was
/// reset/reopened (a newer epoch) and drop its now-stale result instead of clobbering the current
/// load. Shared across the kinds — they're keyed separately, so the counter just needs uniqueness.
static ASYNC_LOAD_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_async_load_epoch() -> u64 {
    ASYNC_LOAD_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Outcome of one async picker-resolve attempt (References / DocumentSymbols): the candidates to
/// apply, or "the buffer's server can't answer *yet*" — the one miss worth retrying, because it
/// resolves itself and settling empty now would show a false "No symbols found" during exactly
/// the window the picker is most reached for (right after opening a workspace). Two cases land
/// on `ServerBusy`: a handshake still in flight (`Starting`, becomes `Ready` or `Crashed`), and
/// a `Ready` server that withheld its answer past the request timeout while reporting active
/// `$/progress` work — several servers queue requests until indexing lands.
enum AsyncResolveAttempt<T> {
    Done(T),
    ServerBusy,
}

/// How often a picker resolve re-checks a busy server, and the total wall-clock budget before it
/// gives up and settles empty. The budget spans real indexing runs (a cold rust-analyzer on a big
/// workspace); the picker honestly shows its loading state the whole time, and a superseded epoch
/// ends the wait early regardless.
const RESOLVE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const RESOLVE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// One retry beat for a resolve waiting out a busy server: sleep the interval, then report
/// whether this load is still the picker's current one. `false` — the picker was closed, or a
/// newer open minted a fresh epoch — ends the retry loop; the newer load polls for itself.
async fn async_load_still_wanted(
    state: &SharedState,
    client_id: ClientId,
    kind: PickerKind,
    epoch: u64,
) -> bool {
    tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
    let s = state.lock().await;
    s.pickers
        .get(&(client_id, kind))
        .is_some_and(|p| p.pending_async_load == Some(epoch))
}

/// True when `buffer_id`'s server is mid-`$/progress` work (indexing, `cargo check`) — the state
/// in which a timed-out request is worth retrying: the server is alive and will answer once the
/// work lands, so settling empty now would go stale the moment it does.
async fn buffer_server_busy(state: &SharedState, buffer_id: BufferId) -> bool {
    let s = state.lock().await;
    s.lsp
        .doc_server
        .get(&buffer_id)
        .and_then(|key| s.lsp.servers.get(key))
        .is_some_and(|h| !h.progress.is_empty())
}

/// Run one picker resolve to completion, waiting out a server that can't answer yet (handshake
/// in flight, or withholding answers mid-indexing) rather than settling to a false "nothing
/// found" — the picker stays on its loading state (`pending_async_load` uncleared) until an
/// answer lands. `ServerBusy` retries `attempt`: each beat sleeps the interval, then re-checks
/// that this load is still the picker's current one ([`async_load_still_wanted`] — the check
/// runs fresh right before the next attempt, and a `Done` result never pays a sleep). `None` =
/// superseded (picker closed or reopened) — the caller just returns; the newer load polls for
/// itself. Budget exhausted = settle `empty`, so the picker shows its honest empty state
/// instead of loading forever.
///
/// `attempt` returns an owned future (clone the `Arc`s in) rather than borrowing through an
/// `AsyncFnMut`: the callers run under `tokio::spawn`, and a lending closure's future trips
/// rustc's "`Send` is not general enough" limitation there.
async fn resolve_waiting_out_busy_server<T, Fut>(
    state: &SharedState,
    client_id: ClientId,
    kind: PickerKind,
    epoch: u64,
    empty: T,
    mut attempt: impl FnMut() -> Fut,
) -> Option<T>
where
    Fut: std::future::Future<Output = AsyncResolveAttempt<T>>,
{
    let deadline = std::time::Instant::now() + RESOLVE_RETRY_BUDGET;
    loop {
        match attempt().await {
            AsyncResolveAttempt::Done(v) => return Some(v),
            AsyncResolveAttempt::ServerBusy if std::time::Instant::now() < deadline => {
                if !async_load_still_wanted(state, client_id, kind, epoch).await {
                    return None;
                }
            }
            AsyncResolveAttempt::ServerBusy => return Some(empty),
        }
    }
}

/// Pick the References-picker candidate to seed the initial highlight on, returning its
/// `(rank, candidate_index)` — `rank` being its position in `ranked` so the window can frame around
/// it. Among the references in the active file (`cursor_path`), prefer the one whose identifier span
/// *contains* the cursor: find-references fires from inside the identifier, so the seeded cursor
/// usually sits past the span's start column, and a start-only test would skip to the next
/// occurrence (the off-by-one this guards against). Failing containment, take the nearest at-or-after
/// the cursor, wrapping to that file's first — mirroring the grep cursor-nearest rule.
fn seed_reference_center(
    refs: &[picker_state::ReferenceCandidate],
    ranked: &[u32],
    cursor_path: Option<&str>,
    pos: LogicalPosition,
) -> Option<(u32, usize)> {
    ranked
        .iter()
        .map(|&ci| ci as usize)
        .filter(|&ci| Some(refs[ci].abs_path.as_str()) == cursor_path)
        .min_by_key(|&ci| {
            let r = &refs[ci];
            let after = (r.line, r.col) >= (pos.line, pos.col);
            (!r.contains(pos), !after, r.line, r.col)
        })
        .and_then(|ci| {
            ranked
                .iter()
                .position(|&r| r as usize == ci)
                .map(|rank| (rank as u32, ci))
        })
}

/// Apply a freshly-resolved async candidate set to its picker, unless a newer open superseded it
/// (the `pending_async_load` epoch moved past `epoch`) or the picker was hidden/closed. Reranks
/// against whatever query the user typed while the resolve was in flight, clamps the window, clears
/// the loading flag, and pushes the updated window. Shared by the References and DocumentSymbols
/// background resolves — both open empty + `ticking` and are filled by this.
async fn apply_async_candidates(
    state: &SharedState,
    client_id: ClientId,
    kind: PickerKind,
    epoch: u64,
    candidates: picker_state::PickerCandidates,
    cursor: Option<LogicalPosition>,
    // The canonical path of the buffer `cursor` lives in — only needed by References, whose
    // candidates are cross-file, to seed on the occurrence in the active file. `None` for the
    // single-file DocumentSymbols seeding.
    cursor_path: Option<String>,
) {
    let mut s = state.lock().await;
    let key = (client_id, kind);
    let Some(picker) = s.pickers.get_mut(&key) else {
        return; // picker gone (closed/reset)
    };
    if picker.pending_async_load != Some(epoch) {
        return; // superseded by a newer open, or already applied
    }
    picker.pending_async_load = None;
    picker.candidates = candidates;
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let ServerState {
        pickers, matcher, ..
    } = &mut *s;
    let picker = pickers.get_mut(&key).expect("checked above");
    // Rank against the current query — the user may have typed a filter while we resolved.
    picker.rerank(matcher);
    // DocumentSymbols: highlight the symbol the cursor sits in. Picked among the *visible* (ranked)
    // symbols and innermost-first (deepest depth, then latest start), so with the top-level chip on
    // it lands on the enclosing top-level symbol; expanded, on the innermost member. We keep its
    // *rank* (position in `ranked`) so the window can be framed around it — a symbol far down the
    // list (e.g. a field near the bottom of a big file, all levels expanded) would otherwise sit
    // outside the pushed window and never match the client's identity centering.
    let center: Option<(u32, usize)> = match (&picker.candidates, cursor) {
        (picker_state::PickerCandidates::Symbols(syms), Some(pos)) => picker
            .ranked
            .iter()
            .enumerate()
            .filter(|(_, &ci)| syms[ci as usize].contains(pos))
            .max_by_key(|(_, &ci)| {
                let c = &syms[ci as usize];
                (c.depth, c.range_start.line, c.range_start.col)
            })
            .map(|(rank, &ci)| (rank as u32, ci as usize)),
        // References: seed on the occurrence the cursor is on (find-references is invoked from a
        // use of the symbol). See `seed_reference_center`.
        (picker_state::PickerCandidates::References(refs), Some(pos)) => {
            seed_reference_center(refs, &picker.ranked, cursor_path.as_deref(), pos)
        }
        _ => None,
    };
    // Frame the window: around the centered symbol when there is one (it rides the push as
    // `center_on`, and the client adopts this offset), else clamp a stale offset back into range.
    if let Some(window) = picker.subscribed.as_mut() {
        let total = picker.ranked.len() as u32;
        match center {
            Some((rank, _)) => window.offset = rank.saturating_sub(window.limit / 2),
            None if window.offset >= total => window.offset = total.saturating_sub(window.limit),
            None => {}
        }
    }
    let center_on = center.map(|(_, ci)| Box::new(picker.candidates.make_item(ci, Vec::new())));
    let mut update = picker_state::build_update(picker, matcher);
    if let Some(ref mut u) = update {
        u.ticking = false; // resolve finished
        u.center_on = center_on;
    }
    drop(s);
    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender.send(picker_update_notif(params)).await;
    }
}

/// Resolve the References picker's candidates in the background (an LSP round-trip + file reads,
/// off the lock) and push them into the already-open picker. Detached/fire-and-forget.
pub fn spawn_reference_resolve(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
) {
    tokio::spawn(async move {
        let Some(candidates) = resolve_waiting_out_busy_server(
            &state,
            client_id,
            PickerKind::References,
            epoch,
            Vec::new(),
            || {
                let state = state.clone();
                async move { build_reference_candidates(&state, client_id, buffer_id).await }
            },
        )
        .await
        else {
            return; // superseded — the newer load owns the picker now
        };
        // The cursor + its file, so the picker opens on the occurrence find-references was invoked
        // on (the same "land on where you are" the grep / outline pickers do). Use the selection's
        // *leading edge* (min of anchor/position), like the grep picker: a jump lands the identifier
        // selected, so the live cursor's `position` is the name's last char while the occurrence
        // starts at the anchor — seeding off `position` would land at-or-after the name's end and
        // miss it.
        let (cursor, cursor_path) = {
            let s = state.lock().await;
            let cursor = s
                .cursors
                .get(&(client_id, buffer_id))
                .map(|c| motion::ordered(c.position, c.anchor).0);
            let path = s
                .try_doc_of(buffer_id)
                .and_then(|b| b.canonical_path.as_deref())
                .map(|p| p.display().to_string());
            (cursor, path)
        };
        apply_async_candidates(
            &state,
            client_id,
            PickerKind::References,
            epoch,
            picker_state::PickerCandidates::References(candidates),
            cursor,
            cursor_path,
        )
        .await;
    });
}

/// Resolve the DocumentSymbols picker's candidates in the background (one LSP round-trip, off the
/// lock) and push them into the already-open picker. Detached/fire-and-forget.
pub fn spawn_symbol_resolve(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
) {
    tokio::spawn(async move {
        let Some((candidates, cursor)) = resolve_waiting_out_busy_server(
            &state,
            client_id,
            PickerKind::DocumentSymbols,
            epoch,
            (Vec::new(), None),
            || {
                let state = state.clone();
                async move { build_symbol_candidates(&state, client_id, buffer_id).await }
            },
        )
        .await
        else {
            return; // superseded — the newer load owns the picker now
        };
        apply_async_candidates(
            &state,
            client_id,
            PickerKind::DocumentSymbols,
            epoch,
            picker_state::PickerCandidates::Symbols(candidates),
            cursor,
            None, // single-file: the symbol's own buffer, matched by position alone
        )
        .await;
    });
}

/// Build the LSP-servers-picker candidates: one per language server owned by `workspace_id`, sorted
/// by name then language for a stable order. `workspace_roots` is used only for the display labels.
pub fn build_lsp_server_candidates(
    s: &ServerState,
    workspace_id: &str,
    workspace_roots: &[std::path::PathBuf],
) -> Vec<picker_state::LspServerCandidate> {
    let mut out: Vec<picker_state::LspServerCandidate> =
        crate::lsp::manager::status_for_workspace(s, workspace_id)
            .into_iter()
            .map(|st| picker_state::LspServerCandidate {
                root_label: lsp_root_label(&st.workspace_root, workspace_roots),
                name: st.name,
                language: st.language,
                workspace_root: st.workspace_root,
                status: st.status,
                progress: st.progress,
            })
            .collect();
    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.language.cmp(&b.language))
    });
    out
}

/// The picker's display label for a server's workspace root: its path relative to the containing
/// workspace root, or empty when the server is rooted *at* a workspace root (so single-root workspaces
/// show no redundant path — only monorepo sub-roots get a disambiguating label).
fn lsp_root_label(workspace_root: &str, workspace_roots: &[std::path::PathBuf]) -> String {
    let root = std::path::Path::new(workspace_root);
    let base = workspace_roots
        .iter()
        .filter(|r| root.starts_with(r))
        .max_by_key(|r| r.components().count());
    match base.and_then(|b| root.strip_prefix(b).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => String::new(),
    }
}

/// Replace a buffer's diagnostics and re-render the viewports showing it, so the new markers appear
/// without an edit. Returns the notifications to send once the state lock is released (the
/// `watcher.rs:183` pattern). A no-op (empty) when the buffer isn't open.
pub fn set_diagnostics_and_refresh(
    s: &mut ServerState,
    buffer_id: BufferId,
    diagnostics: Vec<crate::lsp::diagnostics::BufferDiagnostic>,
) -> PendingPushes {
    s.diagnostics.insert(buffer_id, diagnostics);
    if !s.buffers.contains_key(&buffer_id) {
        return Vec::new();
    }
    let diags = buffer_diagnostics(s, buffer_id);
    let counts = diagnostic_counts(diags);
    let mut pushes = Vec::new();
    // One `lsp/diagnostics_changed` (buffer-wide counts) per distinct client viewing the buffer,
    // plus the per-viewport `viewport/lines_changed` re-render (squiggles + gutter).
    let mut counted_clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
    for vp in s.viewports.values() {
        if !vp.shows(s.view_of(vp), buffer_id) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        if counted_clients.insert(vp.client_id) {
            pushes.push((sender.clone(), diagnostics_changed_notif(buffer_id, counts)));
        }
        pushes.push((
            sender,
            build_lines_changed_notif(s, vp, lines_changed_cursor(s, vp), SneakLabels::Hidden),
        ));
    }
    pushes
}

/// Per-severity counts over a buffer's diagnostics, for the status-bar summary.
pub fn diagnostic_counts(diags: &[crate::lsp::diagnostics::BufferDiagnostic]) -> DiagnosticCounts {
    use aether_protocol::viewport::DiagnosticSeverity::*;
    let mut c = DiagnosticCounts::default();
    for d in diags {
        match d.severity {
            Error => c.errors += 1,
            Warning => c.warnings += 1,
            Information => c.infos += 1,
            Hint => c.hints += 1,
        }
    }
    c
}

fn diagnostics_changed_notif(buffer_id: BufferId, counts: DiagnosticCounts) -> Notification {
    Notification {
        jsonrpc: JsonRpc,
        method: LspDiagnosticsChanged::NAME.into(),
        params: serde_json::to_value(LspDiagnosticsChangedParams { buffer_id, counts })
            .expect("infallible"),
    }
}

/// The per-line footprint of `diags` on `line_idx`: byte ranges clipped to `[0, line_len]`. A
/// diagnostic spanning multiple lines contributes to each line it covers (start line from its
/// column to EOL, middle lines whole, end line up to its column), carrying the full message so the
/// client can show it wherever the cursor sits. Zero-width diagnostics are kept (the client widens
/// them to one cell).
pub fn diagnostic_spans_on_line(
    diags: &[crate::lsp::diagnostics::BufferDiagnostic],
    line_idx: u32,
    line_len: u32,
) -> Vec<DiagnosticSpan> {
    let mut out = Vec::new();
    for d in diags {
        if line_idx < d.start.line || line_idx > d.end.line {
            continue;
        }
        let s = if line_idx == d.start.line {
            d.start.col
        } else {
            0
        };
        let e = if line_idx == d.end.line {
            d.end.col
        } else {
            line_len
        };
        let s = s.min(line_len);
        let e = e.min(line_len).max(s);
        out.push(DiagnosticSpan {
            start: s,
            end: e,
            severity: d.severity,
            message: d.message.clone(),
        });
    }
    out
}

/// Restart the language server(s) for a language in the client's active workspace.
pub async fn lsp_restart_server(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspRestartServerParams,
) -> Result<(), RpcError> {
    let workspace = {
        let s = state.lock().await;
        s.active_workspace_or_err(ctx.client_id)?.id.clone()
    };
    crate::lsp::manager::restart(state, &params.language, &workspace).await;
    Ok(())
}

/// Everything needed to issue a cursor-positioned LSP request: a cloned client for the buffer's
/// (ready) server, the document URI, and the cursor mapped into the server's position encoding.
struct LspCursorRequest {
    client: crate::lsp::client::LspClient,
    uri: String,
    line: u32,
    character: u32,
    encoding: crate::lsp::position::PositionEncoding,
}

/// Resolve [`LspCursorRequest`] for `client_id`'s cursor in `buffer_id`, or `None` if the buffer
/// isn't file-backed or has no ready language server. Runs under the state lock; the caller must
/// drop the lock before awaiting the request (the LSP round-trip must not hold it).
/// Outcome of resolving a cursor-relative LSP request: either a ready request, or *why* it can't
/// run — so hover / goto-definition can report "still starting" vs. "crashed" vs. "no server"
/// instead of a blank result. Mirrors [`FormatResolve`].
enum CursorResolve {
    Ready(LspCursorRequest),
    /// No server attached (unsupported language, not file-backed, or the buffer's gone).
    NoServer,
    /// A server exists but isn't `Ready` yet.
    Starting,
    /// The attached server crashed or was stopped.
    Unavailable,
}

impl CursorResolve {
    /// The wire-level readiness this resolution reports to the client.
    fn readiness(&self) -> LspReadiness {
        match self {
            CursorResolve::Ready(_) => LspReadiness::Ready,
            CursorResolve::NoServer => LspReadiness::NoServer,
            CursorResolve::Starting => LspReadiness::Starting,
            CursorResolve::Unavailable => LspReadiness::Unavailable,
        }
    }

    /// The request if ready, else `None` — for callers (the references / symbols pickers) that
    /// don't surface a readiness message and just fall back to an empty list.
    fn ready(self) -> Option<LspCursorRequest> {
        match self {
            CursorResolve::Ready(req) => Some(req),
            _ => None,
        }
    }
}

fn lsp_cursor_request(s: &ServerState, client_id: ClientId, buffer_id: BufferId) -> CursorResolve {
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return CursorResolve::NoServer;
    };
    let Some(path) = buf.canonical_path.as_deref() else {
        return CursorResolve::NoServer;
    };
    let Some(key) = s.lsp.doc_server.get(&buffer_id) else {
        return CursorResolve::NoServer;
    };
    let Some(handle) = s.lsp.servers.get(key) else {
        return CursorResolve::NoServer;
    };
    match handle.status {
        LspStatus::Ready => {}
        LspStatus::Starting | LspStatus::Initializing | LspStatus::Restarting => {
            return CursorResolve::Starting
        }
        LspStatus::Crashed { .. } | LspStatus::Stopped => return CursorResolve::Unavailable,
    }
    // `Ready` should always carry a live client; treat a missing one as a transient outage.
    let Some(client) = handle.client.clone() else {
        return CursorResolve::Unavailable;
    };
    let encoding = handle.position_encoding;
    let pos = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default()
        .position;
    let line_text = line_text_no_newline(buf, pos.line);
    let character = crate::lsp::position::byte_to_lsp(&line_text, pos.col as usize, encoding);
    CursorResolve::Ready(LspCursorRequest {
        client,
        uri: crate::lsp::uri::path_to_uri(path),
        line: pos.line,
        character,
        encoding,
    })
}

/// The `(language, workspace_root)` of the language server backing `buffer_id`, if one is attached.
/// Read from the LSP doc routing, so it's correct on first open and reopen alike. The client uses
/// it to show this buffer's server health (servers are keyed by `(language, workspace_root)`).
pub fn buffer_lsp_server_ref(
    s: &ServerState,
    buffer_id: BufferId,
) -> Option<aether_protocol::lsp::LspServerRef> {
    s.lsp
        .doc_server
        .get(&buffer_id)
        .map(|key| aether_protocol::lsp::LspServerRef {
            language: key.language.clone(),
            workspace_root: key.root.display().to_string(),
        })
}

/// A buffer line's text without its trailing newline; empty if `line` is past the end.
fn line_text_no_newline(buf: &Document, line: u32) -> String {
    if line as usize >= buf.text.len_lines() {
        return String::new();
    }
    let mut s: String = buf.text.line(line as usize).chunks().collect();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s
}

/// Hover info at the cursor. Returns empty when there's no ready server or the server has nothing.
pub async fn lsp_hover(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspHoverResult, RpcError> {
    let resolved = {
        let s = state.lock().await;
        lsp_cursor_request(&s, ctx.client_id, params.buffer_id)
    };
    let readiness = resolved.readiness();
    let Some(req) = resolved.ready() else {
        return Ok(LspHoverResult {
            contents: None,
            markdown: false,
            readiness,
        });
    };
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
    });
    let parsed = match req.client.request("textDocument/hover", params_json).await {
        Ok(v) => parse_hover_contents(&v),
        Err(e) => {
            tracing::debug!(error = %e, "lsp hover request failed");
            None
        }
    };
    let (contents, markdown) = match parsed {
        Some((s, md)) => (Some(s), md),
        None => (None, false),
    };
    Ok(LspHoverResult {
        contents,
        markdown,
        readiness: LspReadiness::Ready,
    })
}

/// Definition location for the symbol at the cursor. Returns `None` when there's no ready server
/// or the server resolves nothing.
pub async fn lsp_goto_definition(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspGotoDefinitionResult, RpcError> {
    let resolved = {
        let s = state.lock().await;
        lsp_cursor_request(&s, ctx.client_id, params.buffer_id)
    };
    let readiness = resolved.readiness();
    let Some(req) = resolved.ready() else {
        return Ok(LspGotoDefinitionResult {
            location: None,
            readiness,
        });
    };
    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "position": { "line": req.line, "character": req.character },
    });
    let location = match req
        .client
        .request("textDocument/definition", params_json)
        .await
    {
        Ok(v) => parse_definition(&v, req.encoding),
        Err(e) => {
            tracing::debug!(error = %e, "lsp definition request failed");
            None
        }
    };
    Ok(LspGotoDefinitionResult {
        location,
        readiness: LspReadiness::Ready,
    })
}

/// Debounce window before a settled cursor triggers a `documentHighlight` round-trip. Long enough
/// to skip the intermediate positions while a motion key is held, short enough to feel immediate.
const SYMBOL_HIGHLIGHT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// Monotonic debounce generation for symbol highlights (see [`ServerState::symbol_highlight_gen`]).
static SYMBOL_HL_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_symbol_hl_epoch() -> u64 {
    SYMBOL_HL_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Highlight the occurrences of the symbol under the cursor. Cursor-relative and fire-and-forget:
/// record a fresh debounce generation and spawn the (debounced) refresh, returning immediately so
/// the request path never blocks on the LSP round-trip. The client fires this as the cursor settles
/// while no search is active; if a search *is* active we drop any stale symbol set and do nothing —
/// the search owns the highlight layer. `active: false` is an explicit clear (the client leaving
/// Normal mode): drop the set and repaint so a stale highlight can't linger in Insert / the prompt.
pub async fn lsp_document_highlight(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspDocumentHighlightParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;
    let key = (client_id, buffer_id);
    if !params.active {
        let pushes = {
            let mut s = state.lock().await;
            // Stop following, invalidate any in-flight refresh, then clear — repainting only if
            // something showed.
            s.symbol_highlight_follow.remove(&key);
            s.symbol_highlight_gen.remove(&key);
            if s.symbol_highlights.remove(&key).is_none() {
                return Ok(());
            }
            collect_viewport_refresh(&s, client_id, buffer_id)
        };
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        return Ok(());
    }
    let epoch = {
        let mut s = state.lock().await;
        if s.searches.contains_key(&key) {
            s.symbol_highlights.remove(&key);
            s.symbol_highlight_gen.remove(&key);
            return Ok(());
        }
        // No language server attached (the common plain-text case) → nothing to highlight.
        if !s.lsp.doc_server.contains_key(&buffer_id) {
            return Ok(());
        }
        // Follow from here on: `set_cursor` re-arms this refresh on every cursor change, so the
        // client never re-sends this request per move.
        s.symbol_highlight_follow.insert(key);
        let epoch = next_symbol_hl_epoch();
        s.symbol_highlight_gen.insert(key, epoch);
        epoch
    };
    let token = state.lock().await.deferred.start();
    spawn_symbol_highlight_refresh(state.clone(), client_id, buffer_id, epoch, token);
    Ok(())
}

/// The debounced body of [`lsp_document_highlight`]: wait out the settle window, then — if this is
/// still the latest request — resolve the cursor, round-trip `documentHighlight`, store the
/// occurrences as the buffer's symbol set, and push the refreshed viewport. Superseded requests
/// bail without touching the current highlights; a not-ready / empty result clears them.
/// Detached/fire-and-forget; mirrors [`spawn_reference_resolve`].
pub fn spawn_symbol_highlight_refresh(
    state: SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
    token: DeferredToken,
) {
    tokio::spawn(async move {
        let _token = token;
        tokio::time::sleep(SYMBOL_HIGHLIGHT_DEBOUNCE).await;
        let key = (client_id, buffer_id);
        // Resolve the request at the settled position — but only if no newer move superseded us.
        let req = {
            let s = state.lock().await;
            if s.symbol_highlight_gen.get(&key) != Some(&epoch) {
                return;
            }
            lsp_cursor_request(&s, client_id, buffer_id).ready()
        };
        let Some(req) = req else {
            // Server vanished / not ready: make sure nothing stale lingers.
            apply_symbol_highlights(&state, client_id, buffer_id, epoch, Vec::new()).await;
            return;
        };
        let params_json = serde_json::json!({
            "textDocument": { "uri": req.uri },
            "position": { "line": req.line, "character": req.character },
        });
        let raw = match req
            .client
            .request("textDocument/documentHighlight", params_json)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "lsp documentHighlight request failed");
                serde_json::Value::Null
            }
        };
        // Parse against the *current* buffer under the lock (and re-check the generation, since the
        // buffer may have changed during the round-trip).
        let ranges = {
            let s = state.lock().await;
            if s.symbol_highlight_gen.get(&key) != Some(&epoch) {
                return;
            }
            match s.try_doc_of(buffer_id) {
                Some(buf) => parse_document_highlights(&raw, buf, req.encoding),
                None => return,
            }
        };
        apply_symbol_highlights(&state, client_id, buffer_id, epoch, ranges).await;
    });
}

/// Store `ranges` as the symbol-highlight set for `(client, buffer)` and push the refreshed
/// viewport — unless a newer request superseded `epoch`, or the set is unchanged (empty staying
/// empty), in which case nothing is pushed.
async fn apply_symbol_highlights(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    epoch: u64,
    ranges: Vec<(LogicalPosition, LogicalPosition)>,
) {
    let mut s = state.lock().await;
    let key = (client_id, buffer_id);
    if s.symbol_highlight_gen.get(&key) != Some(&epoch) {
        return;
    }
    if ranges.is_empty() {
        if s.symbol_highlights.remove(&key).is_none() {
            return; // already empty — no visible change, skip the push
        }
    } else {
        // Moving between occurrences of the same symbol resolves to the identical set; skip the
        // full-window repaint when nothing actually changed.
        if s.symbol_highlights.get(&key).map(|e| &e.matches) == Some(&ranges) {
            return;
        }
        s.symbol_highlights.insert(
            key,
            SearchEntry {
                query: String::new(),
                options: MatchOptions::default(),
                matches: ranges,
                truncated: false,
                last_pushed_index: 0,
            },
        );
    }
    let pushes = collect_viewport_refresh(&s, client_id, buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Jump the cursor to the next/previous diagnostic in the buffer. The server holds the diagnostics,
/// so it resolves the target and moves the cursor authoritatively (mirrors `view/navigate_change`).
pub async fn lsp_navigate_diagnostic(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspNavigateDiagnosticParams,
) -> Result<LspNavigateDiagnosticResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    // The field's bounds, taken before the mutable borrows below. `d` is a *target* motion: it
    // seeks a specific destination, so a diagnostic outside the element the cursor is in is not a
    // destination at all — landing on the element's edge instead would claim you had arrived
    // somewhere you had not.
    let bounds = {
        let scope = s.motion_scope(client_id, params.buffer_id)?;
        (scope.first_line(), scope.last_line())
    };
    let target = navigate_diagnostic_target(
        buffer_diagnostics(&s, params.buffer_id),
        current.position,
        params.direction,
        params.count,
        bounds,
    );
    let Some(target) = target else {
        let response = wrap_for_response(&s, client_id, params.buffer_id, current);
        return Ok(LspNavigateDiagnosticResult {
            cursor: response,
            moved: false,
        });
    };

    let buf = s.doc_of(params.buffer_id);
    let position = motion::clamp_position(buf, target);
    let result = CursorState {
        position,
        // Extend keeps the existing anchor (grow the selection to the diagnostic); otherwise
        // collapse to a point at the diagnostic.
        anchor: if params.extend {
            current.anchor
        } else {
            position
        },
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, result);
    s.record_motion(key, current, result);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, result);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(LspNavigateDiagnosticResult {
        cursor: response,
        moved: true,
    })
}

/// The position of the `count`-th diagnostic strictly beyond `from` in `direction` **and inside
/// `bounds`**, or `None` when there is no such diagnostic — which the caller reports as
/// `moved: false`, and the client turns into "No more diagnostics".
///
/// Diagnostics are compared by their start position (line then byte column), so the jump is
/// *position*-granular: a second diagnostic further along the cursor's own line is a distinct stop,
/// and navigation moves off the exact cursor position rather than off its whole line.
///
/// Two refusals, both deliberate:
///
/// - **Outside the field is not a destination.** `bounds` is the focused element's line extent, so
///   in a composed view a diagnostic in another hunk — or elsewhere in the same file, outside the
///   window this view shows of it — is invisible to `d`. It used to be found and then whole-document
///   clamped, which put the cursor on the element's edge and called that arriving.
/// - **An over-large count refuses rather than clamping.** `5d` with three diagnostics ahead does
///   nothing. The count says "the fifth one"; there isn't one. Note this cannot change the uncounted
///   case: at `count == 1` the old fallback was already unreachable, since `nth(0)` returning `None`
///   means the filter matched nothing for the fallback to find either.
fn navigate_diagnostic_target(
    diags: &[crate::lsp::diagnostics::BufferDiagnostic],
    from: LogicalPosition,
    direction: DiagnosticDirection,
    count: u32,
    bounds: (u32, u32),
) -> Option<LogicalPosition> {
    let key = |p: &LogicalPosition| (p.line, p.col);
    let from_key = key(&from);
    let (first_line, last_line) = bounds;
    let mut anchors: Vec<LogicalPosition> = diags
        .iter()
        .map(|d| d.start)
        .filter(|p| p.line >= first_line && p.line <= last_line)
        .collect();
    anchors.sort_by_key(key);
    anchors.dedup();
    let skip = (count.max(1) - 1) as usize;
    match direction {
        DiagnosticDirection::Next => anchors
            .iter()
            .filter(|p| key(p) > from_key)
            .nth(skip)
            .copied(),
        DiagnosticDirection::Prev => anchors
            .iter()
            .rev()
            .filter(|p| key(p) < from_key)
            .nth(skip)
            .copied(),
    }
}

/// Everything needed to issue a whole-document formatting request: the ready server's client, the
/// document URI + negotiated encoding, the buffer revision at request time (to detect a concurrent
/// edit), and the LSP formatting options derived from the buffer's indent style.
struct LspFormatReq {
    client: crate::lsp::client::LspClient,
    uri: String,
    encoding: crate::lsp::position::PositionEncoding,
    revision: Revision,
    tab_size: u32,
    insert_spaces: bool,
}

/// Outcome of resolving a format request before the round-trip — lets `lsp_format` report a
/// specific reason rather than a catch-all.
enum FormatResolve {
    Ready(LspFormatReq),
    /// A server for this language exists but isn't `Ready` yet.
    NotReady,
    /// The attached server crashed or was stopped.
    Unavailable,
    /// No formatter: no server attached / not file-backed, or the ready server doesn't advertise
    /// `documentFormattingProvider`.
    Unsupported,
}

fn lsp_format_resolve(s: &ServerState, buffer_id: BufferId) -> FormatResolve {
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return FormatResolve::Unsupported;
    };
    let Some(path) = buf.canonical_path.as_deref() else {
        return FormatResolve::Unsupported;
    };
    let Some(key) = s.lsp.doc_server.get(&buffer_id) else {
        return FormatResolve::Unsupported;
    };
    let Some(handle) = s.lsp.servers.get(key) else {
        return FormatResolve::Unsupported;
    };
    match handle.status {
        LspStatus::Ready => {}
        LspStatus::Starting | LspStatus::Initializing | LspStatus::Restarting => {
            return FormatResolve::NotReady
        }
        LspStatus::Crashed { .. } | LspStatus::Stopped => return FormatResolve::Unavailable,
    }
    if !handle.document_formatting {
        return FormatResolve::Unsupported;
    }
    let Some(client) = handle.client.clone() else {
        return FormatResolve::Unsupported;
    };
    let (tab_size, insert_spaces) = match buf.indent_style {
        crate::indent::IndentStyle::Tab => (4, false),
        crate::indent::IndentStyle::Spaces(n) => (n as u32, true),
    };
    FormatResolve::Ready(LspFormatReq {
        client,
        uri: crate::lsp::uri::path_to_uri(path),
        encoding: handle.position_encoding,
        revision: buf.revision,
        tab_size,
        insert_spaces,
    })
}

/// Format the whole buffer via `textDocument/formatting`. Resolves the server and captures the
/// document version under the lock, drops the lock for the round-trip, then re-locks and applies
/// the returned edits as a single whole-document replacement (one undo step), re-pushing the
/// affected viewports — mirrors the undo/redo whole-rope-swap path.
pub async fn lsp_format(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: LspBufferParams,
) -> Result<LspFormatResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Echo the current (possibly soft-wrap-adjusted) cursor with a non-`Applied` status.
    let outcome = |s: &ServerState, status: FormatStatus| -> LspFormatResult {
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        LspFormatResult {
            cursor: wrap_for_response(s, client_id, buffer_id, cursor),
            status,
        }
    };

    let resolved = {
        let s = state.lock().await;
        lsp_format_resolve(&s, buffer_id)
    };
    let req = match resolved {
        FormatResolve::Ready(req) => req,
        FormatResolve::NotReady => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::NotReady));
        }
        FormatResolve::Unavailable => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::Unavailable));
        }
        FormatResolve::Unsupported => {
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::Unsupported));
        }
    };

    let params_json = serde_json::json!({
        "textDocument": { "uri": req.uri },
        "options": { "tabSize": req.tab_size, "insertSpaces": req.insert_spaces },
    });
    let edits = match req
        .client
        .request("textDocument/formatting", params_json)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "lsp format request failed");
            let s = state.lock().await;
            return Ok(outcome(&s, FormatStatus::NoChange));
        }
    };

    let mut s = state.lock().await;
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return Err(RpcError::buffer_not_found(buffer_id));
    };
    // Edits were computed against `req.revision`; if the buffer moved under us, they're stale.
    if buf.revision != req.revision {
        return Ok(outcome(&s, FormatStatus::NoChange));
    }
    let Some(new_text) = apply_lsp_text_edits(&buf.text, &edits, req.encoding) else {
        return Ok(outcome(&s, FormatStatus::NoChange));
    };
    if buf.text == new_text.as_str() {
        return Ok(outcome(&s, FormatStatus::NoChange)); // formatter produced identical text
    }

    // Apply as one whole-document replacement (single undo step), then refresh like undo/redo.
    let was_dirty = buf.dirty;
    let old_len = buf.text.len_chars();
    let cursors_before = document_cursor_snapshot(&s, buffer_id);
    let mut buf_mut = s.editable_doc(buffer_id)?;
    buf_mut.apply_edit(0, old_len, &new_text, EditKindTag::Format, cursors_before);

    // Clamp every cursor on the buffer into the reformatted rope.
    clamp_doc_cursors(&mut s, buffer_id);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
    notify_lsp_change(&mut s, buffer_id);

    let pushes: PendingPushes = collect_doc_lines_changed_pushes(&s, buffer_id);
    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    let result_cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let result_cursor = wrap_for_response(&s, client_id, buffer_id, result_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(LspFormatResult {
        cursor: result_cursor,
        status: FormatStatus::Applied,
    })
}

/// Apply an LSP `TextEdit[]` to `text`, returning the resulting full document, or `None` when the
/// array is empty/absent or an edit is malformed. Edits are non-overlapping per the spec; applied
/// in descending start order so earlier byte offsets stay valid.
fn apply_lsp_text_edits(
    text: &ropey::Rope,
    edits: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<String> {
    let arr = edits.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut byte_edits: Vec<(usize, usize, &str)> = Vec::with_capacity(arr.len());
    for e in arr {
        let range = e.get("range")?;
        let new_text = e
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let sb = lsp_pos_to_byte(text, range.get("start")?, encoding)?;
        let eb = lsp_pos_to_byte(text, range.get("end")?, encoding)?;
        if sb > eb {
            return None;
        }
        byte_edits.push((sb, eb, new_text));
    }
    byte_edits.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut out: String = text.to_string();
    for (sb, eb, new) in byte_edits {
        if eb > out.len() || !out.is_char_boundary(sb) || !out.is_char_boundary(eb) {
            return None;
        }
        out.replace_range(sb..eb, new);
    }
    Some(out)
}

/// Convert an LSP position (line + `character` in `encoding`) to an absolute byte offset in `text`.
/// A line at/past the buffer end clamps to the byte length.
fn lsp_pos_to_byte(
    text: &ropey::Rope,
    pos: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<usize> {
    let line = pos.get("line")?.as_u64()? as usize;
    let character = pos.get("character")?.as_u64()? as u32;
    if line >= text.len_lines() {
        return Some(text.len_bytes());
    }
    let mut line_str: String = text.line(line).chunks().collect();
    while line_str.ends_with('\n') || line_str.ends_with('\r') {
        line_str.pop();
    }
    let byte_in_line = crate::lsp::position::lsp_to_byte(&line_str, character, encoding);
    Some(text.line_to_byte(line) + byte_in_line)
}

/// Flatten an LSP hover `contents` (MarkupContent, MarkedString, or an array of them) to a string
/// plus whether it's Markdown — so the client renders Markdown vs. literal plain text rather than
/// assuming Markdown for everything.
fn parse_hover_contents(v: &serde_json::Value) -> Option<(String, bool)> {
    let (s, markdown) = markup_to_string(v.get("contents")?)?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some((s, markdown))
}

/// Returns `(text, is_markdown)`. Per LSP: a bare string and `MarkedString` are Markdown;
/// `MarkupContent` carries an explicit `kind` (only `"plaintext"` is not Markdown); a
/// `MarkedString { language, value }` is a code block, fenced so it renders as Markdown.
fn markup_to_string(c: &serde_json::Value) -> Option<(String, bool)> {
    match c {
        serde_json::Value::String(s) => Some((s.clone(), true)),
        serde_json::Value::Object(o) => {
            let value = o.get("value")?.as_str()?;
            if let Some(lang) = o.get("language").and_then(|v| v.as_str()) {
                // Legacy MarkedString { language, value }: a code block → fence it as Markdown.
                Some((format!("```{lang}\n{value}\n```"), true))
            } else {
                // MarkupContent { kind, value }: Markdown unless explicitly plaintext.
                let markdown = o.get("kind").and_then(|v| v.as_str()) != Some("plaintext");
                Some((value.to_string(), markdown))
            }
        }
        // MarkedString[] (legacy) — Markdown if any part is; parts joined as paragraphs.
        serde_json::Value::Array(a) => {
            let parts: Vec<(String, bool)> = a.iter().filter_map(markup_to_string).collect();
            if parts.is_empty() {
                return None;
            }
            let markdown = parts.iter().any(|(_, md)| *md);
            let text = parts
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
                .join("\n\n");
            Some((text, markdown))
        }
        _ => None,
    }
}

/// Parse an LSP definition response (`Location`, `Location[]`, `LocationLink[]`, or null) into the
/// first target location, converting its position into the buffer's byte columns.
fn parse_definition(
    v: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Option<LspLocation> {
    let first = match v {
        serde_json::Value::Array(a) => a.first()?,
        serde_json::Value::Object(_) => v,
        _ => return None,
    };
    parse_location_entry(first, encoding, &mut FileLineCache::new())
}

/// Parse a single LSP `Location` / `LocationLink` object into a location in editor coordinates.
/// Shared by `parse_definition` (first entry only) and `parse_references` (every entry — whose
/// `cache` is what keeps a many-references-in-one-file response from re-reading that file per
/// position).
fn parse_location_entry(
    entry: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
    cache: &mut FileLineCache,
) -> Option<LspLocation> {
    let (uri, range) = if let Some(u) = entry.get("uri") {
        (u.as_str()?, entry.get("range")?)
    } else {
        // LocationLink: prefer the precise selection range, fall back to the full target range.
        let u = entry.get("targetUri")?.as_str()?;
        let range = entry
            .get("targetSelectionRange")
            .or_else(|| entry.get("targetRange"))?;
        (u, range)
    };
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;
    let path = crate::lsp::uri::uri_to_path(uri)?;
    let col = cache.byte_col(&path, line, character, encoding);
    let position = LogicalPosition { line, col };
    Some(LspLocation {
        path: path.display().to_string(),
        position,
        end: lsp_range_end_inclusive(range, position, |l, ch| {
            cache.byte_col(&path, l, ch, encoding)
        }),
    })
}

/// The inclusive last position of an LSP `range` (the identifier span), or `start` when the range
/// is empty, multi-line, or malformed. LSP end is exclusive, so step back one LSP char; identifiers
/// are single-line, so this stays on the start line, converted to a byte column by `col_at`
/// (rope-backed for the outline, cached-file-backed for cross-file locations). Shared by
/// `parse_location_entry` (references / goto-definition) and `push_symbol` (the outline), so all
/// three land the identifier selected identically.
fn lsp_range_end_inclusive(
    range: &serde_json::Value,
    start: LogicalPosition,
    mut col_at: impl FnMut(u32, u32) -> u32,
) -> LogicalPosition {
    (|| {
        let s = range.get("start")?;
        let e = range.get("end")?;
        let line = s.get("line")?.as_u64()? as u32;
        let start_char = s.get("character")?.as_u64()? as u32;
        let end_char = e.get("character")?.as_u64()? as u32;
        let same_line = e.get("line")?.as_u64()? as u32 == line;
        (same_line && end_char > start_char).then(|| LogicalPosition {
            line,
            col: col_at(line, end_char - 1),
        })
    })()
    .unwrap_or(start)
}

/// Parse an LSP `textDocument/references` response (`Location[]`, `LocationLink[]`, or null) into
/// every reference location, converting positions into the buffer's byte columns. Entries that
/// fail to parse are skipped.
fn parse_references(
    v: &serde_json::Value,
    encoding: crate::lsp::position::PositionEncoding,
) -> Vec<LspLocation> {
    let mut cache = FileLineCache::new();
    match v {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|e| parse_location_entry(e, encoding, &mut cache))
            .collect(),
        _ => Vec::new(),
    }
}

/// Map an LSP `{line, character}` (in `encoding`) to a buffer byte position, against the *live*
/// buffer text — correct even with unsaved edits (unlike the disk-reading [`target_byte_col`] that
/// the cross-file reference/definition paths need). For `documentHighlight`, whose ranges are
/// always in the current document.
fn lsp_pos_to_logical(
    buf: &Document,
    line: u32,
    character: u32,
    encoding: crate::lsp::position::PositionEncoding,
) -> LogicalPosition {
    let line_text = line_text_no_newline(buf, line);
    let col = crate::lsp::position::lsp_to_byte(&line_text, character, encoding) as u32;
    LogicalPosition { line, col }
}

/// Parse a `textDocument/documentHighlight` response (`DocumentHighlight[]` or null) into the
/// end-exclusive `(start, end)` ranges of each occurrence, in the buffer's byte coordinates. LSP
/// ranges are already end-exclusive, matching the `SearchEntry::matches` convention. Entries
/// without a parseable range, and empty/degenerate ranges, are dropped.
fn parse_document_highlights(
    v: &serde_json::Value,
    buf: &Document,
    encoding: crate::lsp::position::PositionEncoding,
) -> Vec<(LogicalPosition, LogicalPosition)> {
    let serde_json::Value::Array(entries) = v else {
        return Vec::new();
    };
    let parse_pos = |p: &serde_json::Value| -> Option<LogicalPosition> {
        let line = p.get("line")?.as_u64()? as u32;
        let character = p.get("character")?.as_u64()? as u32;
        Some(lsp_pos_to_logical(buf, line, character, encoding))
    };
    entries
        .iter()
        .filter_map(|entry| {
            let range = entry.get("range")?;
            let start = parse_pos(range.get("start")?)?;
            let end = parse_pos(range.get("end")?)?;
            ((start.line, start.col) < (end.line, end.col)).then_some((start, end))
        })
        .collect()
}

/// Parse an LSP `textDocument/documentSymbol` response into a flat, depth-first list of symbol
/// candidates for `abs_path`. The response is one of two shapes (server capability dependent):
///
/// - `DocumentSymbol[]` — hierarchical: each carries `name`, `kind`, an optional `detail`
///   (signature), a `selectionRange` (the name span) and `children`. We recurse, recording the
///   nesting `depth` and jumping to `selectionRange.start`.
/// - `SymbolInformation[]` — flat: each carries `name`, `kind`, a `location` (range) and an
///   optional `containerName` (used as `detail`). All at depth 0.
///
/// `null` / unexpected shapes yield no symbols; entries missing a name or position are skipped.
/// Server order is preserved (it's the natural reading / nesting order). Positions convert to byte
/// columns against `abs_path` (read from disk for non-UTF-8 servers, like `parse_location_entry`).
fn parse_document_symbols(
    v: &serde_json::Value,
    abs_path: &str,
    encoding: crate::lsp::position::PositionEncoding,
    text: &ropey::Rope,
) -> Vec<picker_state::SymbolCandidate> {
    let serde_json::Value::Array(items) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        push_symbol(item, abs_path, encoding, 0, &mut out, text);
    }
    // Flat servers (e.g. vscode-html-language-server) return `SymbolInformation[]` with no
    // `children`, so every symbol lands at depth 0 with the parent merely named in `containerName`.
    // Rebuild the tree from `range` containment so the outline indents. Gated on "nothing nested
    // yet": a hierarchical `DocumentSymbol` response already carries real depths, and the LSP spec
    // warns its `range` needn't reflect the AST — so we trust the explicit `children` tree there and
    // never second-guess it from ranges.
    if out.iter().all(|c| c.depth == 0) {
        assign_depth_by_containment(&mut out);
    }
    out
}

/// Reconstruct nesting depth for a flat symbol list from `range` containment: in document order,
/// a symbol whose range is enclosed by an ancestor's is one level deeper. Sorts into document order
/// first (by start, widest-first on ties) so it's robust to a server that returns symbols
/// out of order, then walks a stack of open ancestors keyed by their end position.
fn assign_depth_by_containment(cands: &mut [picker_state::SymbolCandidate]) {
    cands.sort_by(|a, b| {
        let pos = |p: &LogicalPosition| (p.line, p.col);
        pos(&a.range_start)
            .cmp(&pos(&b.range_start))
            .then(pos(&b.range_end).cmp(&pos(&a.range_end)))
    });
    let mut ancestor_ends: Vec<(u32, u32)> = Vec::new();
    for c in cands.iter_mut() {
        let start = (c.range_start.line, c.range_start.col);
        // Pop ancestors that have already closed at or before this symbol starts.
        while ancestor_ends.last().is_some_and(|&end| start >= end) {
            ancestor_ends.pop();
        }
        c.depth = ancestor_ends.len() as u32;
        ancestor_ends.push((c.range_end.line, c.range_end.col));
    }
}

/// Append one parsed symbol (and, for `DocumentSymbol`, its children) to `out`. Handles both the
/// hierarchical and the flat response shapes by probing for the fields each carries. Positions
/// convert against `text` — the live rope of the (open) document the symbols describe.
fn push_symbol(
    entry: &serde_json::Value,
    abs_path: &str,
    encoding: crate::lsp::position::PositionEncoding,
    depth: u32,
    out: &mut Vec<picker_state::SymbolCandidate>,
    text: &ropey::Rope,
) {
    let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let symbol_kind = entry
        .get("kind")
        .and_then(|v| v.as_u64())
        .map(aether_protocol::picker::SymbolKind::from_lsp)
        .unwrap_or_default();
    let pos_at = |range: &serde_json::Value, edge: &str| -> Option<LogicalPosition> {
        let p = range.get(edge)?;
        let line = p.get("line")?.as_u64()? as u32;
        let character = p.get("character")?.as_u64()? as u32;
        Some(LogicalPosition {
            line,
            col: rope_byte_col(text, line, character, encoding),
        })
    };
    // DocumentSymbol: `selectionRange` is the name, `range` the full extent. SymbolInformation:
    // both live under `location.range`.
    let name_range = entry
        .get("selectionRange")
        .or_else(|| entry.get("range"))
        .or_else(|| entry.get("location").and_then(|l| l.get("range")));
    let full_range = entry
        .get("range")
        .or_else(|| entry.get("location").and_then(|l| l.get("range")))
        .or(name_range);
    let Some(name_pos) = name_range.and_then(|r| pos_at(r, "start")) else {
        return;
    };
    // Inclusive last char of the name span (see `lsp_range_end_inclusive`); a point when there's no
    // distinct span.
    let name_end = name_range
        .map(|r| lsp_range_end_inclusive(r, name_pos, |l, ch| rope_byte_col(text, l, ch, encoding)))
        .unwrap_or(name_pos);
    // The enclosing extent for cursor-containment; fall back to a zero-width span at the name.
    let range_start = full_range
        .and_then(|r| pos_at(r, "start"))
        .unwrap_or(name_pos);
    let range_end = full_range
        .and_then(|r| pos_at(r, "end"))
        .unwrap_or(name_pos);
    // Only `DocumentSymbol.detail` (a signature). We deliberately skip `SymbolInformation`'s
    // `containerName` — it names the enclosing scope, which the reconstructed indentation already
    // shows, so surfacing it here would just duplicate the parent next to every flat-server symbol.
    let detail = entry
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    out.push(picker_state::SymbolCandidate {
        abs_path: abs_path.to_string(),
        start: name_pos,
        end: name_end,
        name: crate::symbols::clean_symbol_name(name, abs_path),
        symbol_kind,
        detail,
        depth,
        range_start,
        range_end,
    });
    if let Some(children) = entry.get("children").and_then(|c| c.as_array()) {
        for child in children {
            push_symbol(child, abs_path, encoding, depth + 1, out, text);
        }
    }
}

/// Per-parse cache of target files' lines, for converting cross-file LSP positions (references /
/// goto-definition — files that may not be open) into byte columns. Each file is read from disk
/// once per parse: the previous per-position `read_to_string` made a many-entry response
/// O(entries × file size), seconds of grinding after the server had already answered.
#[derive(Default)]
struct FileLineCache(HashMap<std::path::PathBuf, Option<Vec<String>>>);

impl FileLineCache {
    fn new() -> Self {
        Self::default()
    }

    /// Convert `character` (in the server's encoding) on `path:line` to a byte column. For UTF-8
    /// the character *is* the byte offset (no read at all); otherwise best-effort against the
    /// cached file — falling back to the raw character when the file/line can't be read.
    fn byte_col(
        &mut self,
        path: &std::path::Path,
        line: u32,
        character: u32,
        encoding: crate::lsp::position::PositionEncoding,
    ) -> u32 {
        if matches!(encoding, crate::lsp::position::PositionEncoding::Utf8) {
            return character;
        }
        let lines = self.0.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(path)
                .ok()
                .map(|c| c.lines().map(str::to_string).collect())
        });
        match lines.as_ref().and_then(|ls| ls.get(line as usize)) {
            Some(l) => crate::lsp::position::lsp_to_byte(l, character, encoding) as u32,
            None => character,
        }
    }
}

/// [`FileLineCache::byte_col`]'s live-buffer sibling: convert against the rope the server's
/// positions were synced from. The right source for the outline — the document is the open
/// buffer, so disk may lag the `didChange` stream the server answered against (unsaved edits),
/// and the rope is already in memory (no I/O at all).
fn rope_byte_col(
    text: &ropey::Rope,
    line: u32,
    character: u32,
    encoding: crate::lsp::position::PositionEncoding,
) -> u32 {
    if matches!(encoding, crate::lsp::position::PositionEncoding::Utf8) {
        return character;
    }
    if line as usize >= text.len_lines() {
        return character;
    }
    let mut s: String = text.line(line as usize).chunks().collect();
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    crate::lsp::position::lsp_to_byte(&s, character, encoding) as u32
}

/// [`recompute_diff_hunks_if_viewed`] without the "is anyone viewing it" guard.
///
/// That guard is an optimisation — no viewport, no gutter to feed — but it leaves the cached
/// hunks stale for a buffer mutated while hidden, and nothing recomputes them on the way back:
/// a subscribe renders from the cache rather than refreshing it. So the moment a viewport
/// *appears* is the other point the guard's assumption has to be re-established, which is the
/// one caller here.
///
/// Cheap in the same way: it re-diffs the **cached** baseline (HEAD hasn't moved, only the
/// buffer), so there's no repo discovery or blob read. Blame needs no invalidation — its cache
/// carries the revision it was computed at and re-derives itself when that no longer matches.
pub fn rediff_git_for_buffer(s: &mut ServerState, buffer_id: BufferId) {
    recompute_git_hunks(s, buffer_id);
}

/// Re-resolve a buffer's Git baseline from disk (HEAD changed externally — commit / checkout /
/// stage), recompute its hunks, invalidate cached blame, and build `viewport/lines_changed`
/// pushes for every viewport on the buffer so the gutter / inline diff refresh live. Called by
/// the file watcher when something under the repo's `.git` changes. Returns the pushes to send
/// after the state lock is released. No-op (empty) for a scratch buffer or a missing buffer.
pub(crate) fn refresh_git_for_buffer(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return Vec::new();
    };
    let Some(path) = buf.canonical_path.clone() else {
        // A `git/show` view has no file, but its repo-level status can still go stale — a checkout
        // in a terminal moves the branch its status bar is showing. Only the branch is refreshed
        // here: a revision's content really is a snapshot, and the working tree's is rebuilt by
        // [`refresh_working_changes_views`], which needs a `git diff` and so can't run under this
        // lock.
        if let Some(workdir) = buf
            .virtual_source
            .as_ref()
            .and_then(|v| v.target.repo_id().map(std::path::PathBuf::from))
        {
            if let Some(mut status) = crate::git::repo_status(&workdir) {
                // A pathless buffer has no `GitBaseline` to carry the baseline token, so it is
                // read straight from the repo's choice — the same one the view's content is
                // generated against.
                status.baseline = s.git_baseline_choices.get(&workdir).cloned();
                s.virtual_git_status.insert(buffer_id, status);
                return collect_doc_lines_changed_pushes(s, buffer_id);
            }
        }
        return Vec::new();
    };
    // Re-read the committed baseline (the expensive part), then attach it — re-diffing the live
    // buffer against both the HEAD and index blobs (the latter also picks up staging done outside
    // the editor).
    let baseline = crate::git::load_baseline(&path, &s.git_baseline_choices);
    attach_git_baseline(s, buffer_id, baseline)
}

/// Install a freshly-loaded Git baseline for `buffer_id`: diff the live buffer against it, replace
/// the cached hunks, invalidate cached blame, and build `viewport/lines_changed` pushes for every
/// viewport on the buffer so the gutter / inline diff refresh live. The diff runs against the
/// buffer's *current* text, so a caller that loaded the baseline off the lock ([`finish_git_baseline`])
/// needs no revision bookkeeping. No-op (empty) for a missing buffer.
pub fn attach_git_baseline(
    s: &mut ServerState,
    buffer_id: BufferId,
    baseline: crate::git::GitBaseline,
) -> PendingPushes {
    if s.try_doc_of(buffer_id).is_none() {
        return Vec::new();
    }
    // The baseline goes in first: it's what says whether this file is conflicted, which decides
    // both what the hunks are diffed against and which of them are masked out.
    s.git_baseline.insert(buffer_id, baseline);
    recompute_git_hunks(s, buffer_id);
    s.git_blame.remove(&buffer_id); // committed history may have changed → recompute on request
                                    // The blame label's push dedupe keys on (line, revision) — both unchanged by an external
                                    // commit — so forget the last pushes and re-arm every follower, or the label would go stale.
    let followers: Vec<_> = s
        .blame_follow
        .iter()
        .filter(|(_, b)| *b == buffer_id)
        .copied()
        .collect();
    for key in followers {
        s.blame_last_pushed.remove(&key);
        if let Some(tx) = &s.cursor_moved_tx {
            let token = s.deferred.start();
            let _ = tx.send((key.0, key.1, token));
        }
    }
    collect_buffer_refresh_pushes(s, buffer_id)
}

#[cfg(test)]
mod document_highlight_tests {
    use super::*;
    use crate::lsp::position::PositionEncoding;
    use aether_protocol::LogicalPosition;

    fn buf_with(text: &str) -> Document {
        let mut doc = Document::scratch(DocumentId(1), None);
        doc.text = ropey::Rope::from_str(text);
        doc
    }

    fn pos(line: u32, col: u32) -> LogicalPosition {
        LogicalPosition { line, col }
    }

    #[test]
    fn parses_ranges_end_exclusive_and_drops_degenerate() {
        // `foo` twice on line 0; LSP ranges are end-exclusive, matching SearchEntry::matches.
        let buf = buf_with("foo = foo\n");
        let v = serde_json::json!([
            { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3} } },
            { "range": { "start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 9} }, "kind": 2 },
            // Empty range → dropped.
            { "range": { "start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 4} } },
        ]);
        assert_eq!(
            parse_document_highlights(&v, &buf, PositionEncoding::Utf8),
            vec![(pos(0, 0), pos(0, 3)), (pos(0, 6), pos(0, 9))]
        );
    }

    #[test]
    fn non_array_response_yields_nothing() {
        let buf = buf_with("foo\n");
        assert!(
            parse_document_highlights(&serde_json::Value::Null, &buf, PositionEncoding::Utf8)
                .is_empty()
        );
    }

    #[test]
    fn utf16_characters_map_to_byte_columns() {
        // 'é' is one UTF-16 unit but two bytes, so byte columns sit past the LSP characters.
        let buf = buf_with("é foo\n");
        let v = serde_json::json!([
            { "range": { "start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 5} } },
        ]);
        // 'é'(2 bytes) + ' '(1) → "foo" spans bytes 3..6.
        assert_eq!(
            parse_document_highlights(&v, &buf, PositionEncoding::Utf16),
            vec![(pos(0, 3), pos(0, 6))]
        );
    }

    #[test]
    fn render_matches_prefers_search_then_symbol_then_none() {
        let mut st = ServerState::new();
        let client = uuid::Uuid::nil();
        let buffer = 1u64;
        let entry = |q: &str, n: u32| SearchEntry {
            query: q.to_string(),
            options: MatchOptions::default(),
            matches: (0..n).map(|i| (pos(0, i), pos(0, i + 1))).collect(),
            truncated: false,
            last_pushed_index: 0,
        };
        // Nothing stored → no highlights.
        assert!(render_matches(&st, client, buffer).is_none());
        // Symbol set only → it renders.
        st.symbol_highlights.insert((client, buffer), entry("", 2));
        assert_eq!(
            render_matches(&st, client, buffer).map(|e| e.matches.len()),
            Some(2)
        );
        // A real search always wins, enforcing "symbol highlights only when no search is active".
        st.searches.insert((client, buffer), entry("needle", 5));
        assert_eq!(
            render_matches(&st, client, buffer).map(|e| e.query.as_str()),
            Some("needle")
        );
    }
}

#[cfg(test)]
mod diagnostic_span_tests {
    use super::{diagnostic_counts, diagnostic_spans_on_line, navigate_diagnostic_target};
    use crate::lsp::diagnostics::BufferDiagnostic;
    use aether_protocol::lsp::DiagnosticDirection;
    use aether_protocol::viewport::DiagnosticSeverity;
    use aether_protocol::LogicalPosition;

    #[test]
    fn diagnostic_counts_tally_by_severity() {
        let mk = |severity| BufferDiagnostic {
            start: LogicalPosition { line: 0, col: 0 },
            end: LogicalPosition { line: 0, col: 1 },
            severity,
            message: "m".into(),
        };
        let diags = vec![
            mk(DiagnosticSeverity::Error),
            mk(DiagnosticSeverity::Error),
            mk(DiagnosticSeverity::Warning),
            mk(DiagnosticSeverity::Hint),
        ];
        let c = diagnostic_counts(&diags);
        assert_eq!((c.errors, c.warnings, c.infos, c.hints), (2, 1, 0, 1));
        assert!(diagnostic_counts(&[]).is_empty());
    }

    fn diag(l0: u32, c0: u32, l1: u32, c1: u32) -> BufferDiagnostic {
        BufferDiagnostic {
            start: LogicalPosition { line: l0, col: c0 },
            end: LogicalPosition { line: l1, col: c1 },
            severity: DiagnosticSeverity::Error,
            message: "m".into(),
        }
    }

    #[test]
    fn single_line_span_is_clipped_to_its_range() {
        let diags = [diag(2, 3, 2, 7)];
        assert!(diagnostic_spans_on_line(&diags, 1, 80).is_empty());
        let on = diagnostic_spans_on_line(&diags, 2, 80);
        assert_eq!(on.len(), 1);
        assert_eq!((on[0].start, on[0].end), (3, 7));
        assert!(diagnostic_spans_on_line(&diags, 3, 80).is_empty());
    }

    #[test]
    fn multi_line_span_covers_each_line() {
        // Lines 1..=3; line lengths 10/20/30.
        let diags = [diag(1, 4, 3, 6)];
        let start = diagnostic_spans_on_line(&diags, 1, 10);
        assert_eq!((start[0].start, start[0].end), (4, 10)); // from col to EOL
        let mid = diagnostic_spans_on_line(&diags, 2, 20);
        assert_eq!((mid[0].start, mid[0].end), (0, 20)); // whole line
        let end = diagnostic_spans_on_line(&diags, 3, 30);
        assert_eq!((end[0].start, end[0].end), (0, 6)); // up to col
    }

    #[test]
    fn columns_clamp_to_line_length() {
        let diags = [diag(0, 50, 0, 99)];
        let on = diagnostic_spans_on_line(&diags, 0, 5);
        assert_eq!((on[0].start, on[0].end), (5, 5)); // both clamped to EOL
    }

    #[test]
    fn zero_width_diagnostic_is_kept() {
        let diags = [diag(0, 2, 0, 2)];
        let on = diagnostic_spans_on_line(&diags, 0, 10);
        assert_eq!(on.len(), 1);
        assert_eq!((on[0].start, on[0].end), (2, 2));
    }

    /// Bounds covering every line — the ordinary editor case, where the field *is* the buffer.
    /// Scoping is exercised separately by `navigate_diagnostic_ignores_diagnostics_outside_the_field`.
    const WHOLE_DOC: (u32, u32) = (0, u32::MAX);

    #[test]
    fn navigate_diagnostic_finds_next_and_prev() {
        use DiagnosticDirection::{Next, Prev};
        // Diagnostics on lines 2 (col 4), 5, 9 — deliberately out of order to exercise the sort.
        let pos = |line, col| LogicalPosition { line, col };
        let diags = [diag(5, 0, 5, 1), diag(2, 4, 2, 6), diag(9, 0, 9, 3)];
        // From (3, 0): next is line 5, prev is line 2 (at its column).
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(3, 0), Next, 1, WHOLE_DOC),
            Some(pos(5, 0))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(3, 0), Prev, 1, WHOLE_DOC),
            Some(pos(2, 4))
        );
        // Strictly beyond the cursor *position*: standing exactly on a diagnostic skips it.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 0), Next, 1, WHOLE_DOC),
            Some(pos(9, 0))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 0), Prev, 1, WHOLE_DOC),
            Some(pos(2, 4))
        );
        // Count walks the list: from (3, 0), count 2 forward skips line 5 to land on line 9.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(3, 0), Next, 2, WHOLE_DOC),
            Some(pos(9, 0))
        );
        // An over-large count REFUSES rather than clamping to the furthest diagnostic: the count
        // names "the ninth one", and there is no ninth one. Landing on the third and calling it
        // done is the clamp this design replaces.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(3, 0), Next, 9, WHOLE_DOC),
            None
        );
        // count 0 behaves as 1.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(3, 0), Next, 0, WHOLE_DOC),
            Some(pos(5, 0))
        );
    }

    /// `d` is a target motion: a diagnostic outside the focused field is not a destination, so the
    /// motion refuses rather than finding it and clamping onto the field's edge.
    ///
    /// This is the case Joe described — "if the diagnostic isn't visible, we wouldn't move the
    /// cursor, rather than sometimes moving to the end of the editor block if there happens to be a
    /// diagnostic later in the buffer". `None` here is what becomes `moved: false`, which the client
    /// turns into a grouped "No more diagnostics" toast.
    #[test]
    fn navigate_diagnostic_ignores_diagnostics_outside_the_field() {
        use DiagnosticDirection::{Next, Prev};
        let pos = |line, col| LogicalPosition { line, col };
        // A hunk covering lines 10..=14, with diagnostics above it, inside it, and below it.
        let diags = [diag(2, 0, 2, 1), diag(12, 3, 12, 5), diag(40, 0, 40, 1)];
        let field = (10u32, 14u32);

        // Inside the field, the in-field diagnostic is reachable in both directions.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(10, 0), Next, 1, field),
            Some(pos(12, 3))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(14, 0), Prev, 1, field),
            Some(pos(12, 3))
        );
        // Past it, the one 26 lines below is invisible — no move, rather than a jump or an edge.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(12, 3), Next, 1, field),
            None
        );
        // And the one above the field is equally out of reach going backwards.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(12, 3), Prev, 1, field),
            None
        );
        // Non-vacuity: the very same diagnostics, unscoped, DO find both.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(12, 3), Next, 1, WHOLE_DOC),
            Some(pos(40, 0))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(12, 3), Prev, 1, WHOLE_DOC),
            Some(pos(2, 0))
        );
    }

    #[test]
    fn navigate_diagnostic_distinguishes_diagnostics_on_one_line() {
        use DiagnosticDirection::{Next, Prev};
        let pos = |line, col| LogicalPosition { line, col };
        // Three diagnostics on line 5 (cols 2, 8, 14) plus a neighbour on line 9.
        let diags = [
            diag(5, 8, 5, 9),
            diag(5, 2, 5, 4),
            diag(9, 0, 9, 1),
            diag(5, 14, 5, 16),
        ];
        // From the line start, Next steps through each same-line diagnostic by column…
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 0), Next, 1, WHOLE_DOC),
            Some(pos(5, 2))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 2), Next, 1, WHOLE_DOC),
            Some(pos(5, 8))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 8), Next, 1, WHOLE_DOC),
            Some(pos(5, 14))
        );
        // …then crosses to the next line once the line's diagnostics are exhausted.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 14), Next, 1, WHOLE_DOC),
            Some(pos(9, 0))
        );
        // A count jumps multiple same-line diagnostics at once.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 0), Next, 3, WHOLE_DOC),
            Some(pos(5, 14))
        );
        // Prev is the column-aware mirror.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 14), Prev, 1, WHOLE_DOC),
            Some(pos(5, 8))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 8), Prev, 1, WHOLE_DOC),
            Some(pos(5, 2))
        );
        // A cursor mid-span (col 10, between the col-8 and col-14 diagnostics) resolves by position.
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 10), Next, 1, WHOLE_DOC),
            Some(pos(5, 14))
        );
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(5, 10), Prev, 1, WHOLE_DOC),
            Some(pos(5, 8))
        );
    }

    #[test]
    fn navigate_diagnostic_returns_none_at_the_ends() {
        use DiagnosticDirection::{Next, Prev};
        let pos = |line, col| LogicalPosition { line, col };
        let diags = [diag(2, 0, 2, 1), diag(7, 0, 7, 1)];
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(7, 0), Next, 1, WHOLE_DOC),
            None
        ); // past the last
        assert_eq!(
            navigate_diagnostic_target(&diags, pos(2, 0), Prev, 1, WHOLE_DOC),
            None
        ); // before the first
        assert_eq!(
            navigate_diagnostic_target(&[], pos(0, 0), Next, 1, WHOLE_DOC),
            None
        ); // none at all
    }
}

#[cfg(test)]
mod lsp_parse_tests {
    use super::*;
    use crate::lsp::position::PositionEncoding;
    use serde_json::json;

    #[test]
    fn hover_markup_content_string_and_array() {
        // MarkupContent markdown → text + markdown=true.
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"kind": "markdown", "value": "fn foo()"}})),
            Some(("fn foo()".into(), true))
        );
        // MarkupContent plaintext → markdown=false (render literally).
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"kind": "plaintext", "value": "a*b_c"}})),
            Some(("a*b_c".into(), false))
        );
        // A bare string is a MarkedString → markdown.
        assert_eq!(
            parse_hover_contents(&json!({"contents": "plain"})),
            Some(("plain".into(), true))
        );
        // Legacy MarkedString { language, value } → fenced as a markdown code block.
        assert_eq!(
            parse_hover_contents(&json!({"contents": {"language": "rust", "value": "let x = 1;"}})),
            Some(("```rust\nlet x = 1;\n```".into(), true))
        );
        // Array (MarkedString[]) → joined, markdown if any part is.
        assert_eq!(
            parse_hover_contents(&json!({"contents": [{"language": "rust", "value": "a"}, "b"]})),
            Some(("```rust\na\n```\n\nb".into(), true))
        );
    }

    #[test]
    fn hover_empty_or_absent_is_none() {
        assert!(parse_hover_contents(&json!({"contents": null})).is_none());
        assert!(parse_hover_contents(&json!({"contents": "   "})).is_none());
        assert!(parse_hover_contents(&json!({})).is_none());
    }

    #[test]
    fn definition_location_array_and_link() {
        // Bare Location.
        let v = json!({"uri": "file:///p/a.rs", "range": {"start": {"line": 3, "character": 5}, "end": {"line": 3, "character": 8}}});
        let loc = parse_definition(&v, PositionEncoding::Utf8).unwrap();
        assert_eq!(loc.path, "/p/a.rs");
        assert_eq!(loc.position, LogicalPosition { line: 3, col: 5 });
        // Array → first.
        let v = json!([{"uri": "file:///p/a.rs", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}}]);
        assert_eq!(
            parse_definition(&v, PositionEncoding::Utf8)
                .unwrap()
                .position
                .line,
            1
        );
        // LocationLink → targetSelectionRange preferred over targetRange.
        let v = json!([{
            "targetUri": "file:///p/b.rs",
            "targetSelectionRange": {"start": {"line": 7, "character": 2}, "end": {"line": 7, "character": 9}},
            "targetRange": {"start": {"line": 6, "character": 0}, "end": {"line": 8, "character": 0}}
        }]);
        let loc = parse_definition(&v, PositionEncoding::Utf8).unwrap();
        assert_eq!(loc.path, "/p/b.rs");
        assert_eq!(loc.position, LogicalPosition { line: 7, col: 2 });
    }

    #[test]
    fn definition_null_and_empty_is_none() {
        assert!(parse_definition(&json!(null), PositionEncoding::Utf8).is_none());
        assert!(parse_definition(&json!([]), PositionEncoding::Utf8).is_none());
    }

    #[test]
    fn references_parses_every_location() {
        // A `Location[]` with entries in two files — all are kept, in response order.
        let v = json!([
            {"uri": "file:///p/a.rs", "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9}}},
            {"uri": "file:///p/b.rs", "range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 14}}},
        ]);
        let refs = parse_references(&v, PositionEncoding::Utf8);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "/p/a.rs");
        assert_eq!(refs[0].position, LogicalPosition { line: 0, col: 3 });
        // The identifier span's inclusive last char (LSP end is exclusive at char 9 → col 8), for
        // landing the reference selected.
        assert_eq!(refs[0].end, LogicalPosition { line: 0, col: 8 });
        assert_eq!(refs[1].path, "/p/b.rs");
        assert_eq!(refs[1].position, LogicalPosition { line: 4, col: 8 });
        assert_eq!(refs[1].end, LogicalPosition { line: 4, col: 13 });
    }

    #[test]
    fn references_null_and_non_array_is_empty() {
        // `textDocument/references` returns `Location[] | null`; both null and a stray object
        // yield no references rather than erroring.
        assert!(parse_references(&json!(null), PositionEncoding::Utf8).is_empty());
        assert!(
            parse_references(&json!({"uri": "file:///p/a.rs"}), PositionEncoding::Utf8).is_empty()
        );
        // Unparseable entries are skipped, not fatal.
        let v = json!([
            {"uri": "file:///p/a.rs", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}}},
            {"garbage": true},
        ]);
        assert_eq!(parse_references(&v, PositionEncoding::Utf8).len(), 1);
    }

    #[test]
    fn document_symbols_flattens_hierarchy_with_depth() {
        // DocumentSymbol[]: a struct with two nested members. selectionRange drives the position;
        // children are flattened depth-first with incrementing depth.
        let v = json!([
            {
                "name": "Parser", "kind": 23, "detail": "struct Parser",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 1}},
                "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}},
                "children": [
                    {
                        "name": "new", "kind": 6, "detail": "fn() -> Parser",
                        "range": {"start": {"line": 1, "character": 4}, "end": {"line": 3, "character": 5}},
                        "selectionRange": {"start": {"line": 1, "character": 11}, "end": {"line": 1, "character": 14}}
                    }
                ]
            }
        ]);
        let syms =
            parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8, &ropey::Rope::new());
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Parser");
        assert_eq!(
            syms[0].symbol_kind,
            aether_protocol::picker::SymbolKind::Struct
        );
        assert_eq!(syms[0].depth, 0);
        // The name span is `selectionRange`, not `range`: start (0,7), inclusive last char (0,12).
        assert_eq!(syms[0].start, LogicalPosition { line: 0, col: 7 });
        assert_eq!(syms[0].end, LogicalPosition { line: 0, col: 12 });
        assert_eq!(syms[0].detail, "struct Parser");
        assert_eq!(syms[1].name, "new");
        assert_eq!(
            syms[1].symbol_kind,
            aether_protocol::picker::SymbolKind::Method
        );
        assert_eq!(syms[1].depth, 1);
        assert_eq!(syms[1].start, LogicalPosition { line: 1, col: 11 });
        assert_eq!(syms[1].end, LogicalPosition { line: 1, col: 13 });
        // The full `range` (not selectionRange) is captured for cursor containment: the struct
        // spans lines 0..9, so a cursor on line 5 falls inside it.
        assert_eq!(syms[0].range_start, LogicalPosition { line: 0, col: 0 });
        assert_eq!(syms[0].range_end, LogicalPosition { line: 9, col: 1 });
        assert!(syms[0].contains(LogicalPosition { line: 5, col: 0 }));
    }

    #[test]
    fn document_symbols_parses_flat_symbol_information() {
        // SymbolInformation[]: no selectionRange/children; position under location.range.
        // `containerName` is deliberately *not* used as detail (the tree indentation shows the
        // parent), so detail stays empty here.
        let v = json!([
            {
                "name": "helper", "kind": 12, "containerName": "mymod",
                "location": {"uri": "file:///p/a.rs", "range": {"start": {"line": 5, "character": 3}, "end": {"line": 5, "character": 9}}}
            }
        ]);
        let syms =
            parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8, &ropey::Rope::new());
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "helper");
        assert_eq!(
            syms[0].symbol_kind,
            aether_protocol::picker::SymbolKind::Function
        );
        assert_eq!(
            syms[0].detail, "",
            "containerName is not surfaced as detail"
        );
        assert_eq!(syms[0].depth, 0);
        assert_eq!(syms[0].start, LogicalPosition { line: 5, col: 3 });
        // Flat servers have no distinct name range → `end` falls back to the location range's
        // inclusive last char (5,8).
        assert_eq!(syms[0].end, LogicalPosition { line: 5, col: 8 });
    }

    #[test]
    fn document_symbols_convert_utf16_columns_against_the_rope() {
        // A UTF-16 server's character offsets land on multi-byte text: conversion runs against
        // the live rope (the synced buffer content the server's positions describe) — no disk
        // I/O, and correct for unsaved edits the on-disk file doesn't have yet.
        let text = ropey::Rope::from_str("λλ fn naïve() {}\n");
        let v = json!([
            {"name": "naïve", "kind": 12,
             "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 16}},
             "selectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}}}
        ]);
        let syms = parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf16, &text);
        assert_eq!(syms.len(), 1);
        // "λλ fn " is 6 UTF-16 units but 8 bytes; the name's inclusive last char ('e', unit 10)
        // sits at byte 13 ('ï' is two bytes).
        assert_eq!(syms[0].start, LogicalPosition { line: 0, col: 8 });
        assert_eq!(syms[0].end, LogicalPosition { line: 0, col: 13 });
    }

    #[test]
    fn document_symbols_flat_reconstructs_depth_from_ranges() {
        // A flat SymbolInformation[] (like vscode-html) with nested ranges — html > head > meta —
        // gets its tree rebuilt from `range` containment so the outline indents.
        let loc = |s: (u64, u64), e: (u64, u64)| json!({"range": {"start": {"line": s.0, "character": s.1}, "end": {"line": e.0, "character": e.1}}});
        let v = json!([
            {"name": "html", "kind": 8, "location": loc((1, 0), (24, 7))},
            {"name": "head", "kind": 8, "location": loc((2, 2), (6, 9))},
            {"name": "meta", "kind": 8, "location": loc((3, 4), (3, 28))},
            {"name": "title", "kind": 8, "location": loc((4, 4), (4, 33))},
            {"name": "body", "kind": 8, "location": loc((7, 2), (23, 9))},
            {"name": "h1", "kind": 8, "location": loc((8, 4), (8, 20))},
        ]);
        let syms =
            parse_document_symbols(&v, "/p/a.html", PositionEncoding::Utf8, &ropey::Rope::new());
        let depth = |name: &str| syms.iter().find(|c| c.name == name).unwrap().depth;
        assert_eq!(depth("html"), 0);
        assert_eq!(depth("head"), 1);
        assert_eq!(depth("body"), 1);
        assert_eq!(depth("meta"), 2);
        assert_eq!(depth("title"), 2);
        assert_eq!(depth("h1"), 2); // h1 under body
    }

    #[test]
    fn document_symbols_null_and_bad_entries_skipped() {
        // null / non-array → empty; entries missing name or position are skipped, not fatal.
        assert!(parse_document_symbols(
            &json!(null),
            "/p/a.rs",
            PositionEncoding::Utf8,
            &ropey::Rope::new()
        )
        .is_empty());
        let v = json!([
            {"name": "ok", "kind": 13, "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
            {"kind": 13, "selectionRange": {"start": {"line": 1, "character": 0}}},
            {"name": "no_pos", "kind": 13},
        ]);
        let syms =
            parse_document_symbols(&v, "/p/a.rs", PositionEncoding::Utf8, &ropey::Rope::new());
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ok");
    }

    #[test]
    fn apply_text_edits_single_and_multi() {
        use ropey::Rope;
        let text = Rope::from_str("foo\nbar\n");
        // Replace "bar" (line 1, cols 0..3) with "BAZ".
        let edits = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "BAZ"}]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "foo\nBAZ\n"
        );
        // Two edits given out of order — descending-start application keeps offsets valid.
        let edits = json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "X"},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "Q"},
        ]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "Xfoo\nQ\n"
        );
    }

    #[test]
    fn apply_text_edits_whole_document_and_empty() {
        use ropey::Rope;
        let text = Rope::from_str("a\nb\n");
        // Whole-document replace: end one past the last line clamps to the buffer end.
        let edits = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": "z\n"}]);
        assert_eq!(
            apply_lsp_text_edits(&text, &edits, PositionEncoding::Utf8).unwrap(),
            "z\n"
        );
        // Empty / absent edit lists are a no-op (None), not an empty document.
        assert!(apply_lsp_text_edits(&text, &json!([]), PositionEncoding::Utf8).is_none());
        assert!(apply_lsp_text_edits(&text, &json!(null), PositionEncoding::Utf8).is_none());
    }
}

#[cfg(test)]
mod seed_reference_center_tests {
    use super::*;

    fn at(line: u32, col: u32) -> LogicalPosition {
        LogicalPosition { line, col }
    }

    /// `name` spans `[start_col, end_col]` inclusive on a single line, in the given file.
    fn rf(path: &str, line: u32, start_col: u32, end_col: u32) -> picker_state::ReferenceCandidate {
        picker_state::ReferenceCandidate {
            abs_path: path.into(),
            display_path: path.into(),
            line,
            col: start_col,
            end_line: line,
            end_col,
            preview: String::new(),
            is_definition: false,
        }
    }

    #[test]
    fn cursor_inside_span_seeds_that_occurrence_not_the_next() {
        // Two uses of `helper` (cols 4..=9) in the active file, on lines 4 and 8. The cursor rests
        // mid-identifier on line 4 (col 6) — a start-only "at-or-after" test would reject line 4
        // (its start col 4 < 6) and skip to line 8. Containment must keep us on line 4.
        let refs = vec![rf("/a.rs", 4, 4, 9), rf("/a.rs", 8, 4, 9)];
        let ranked: Vec<u32> = vec![0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(4, 6));
        assert_eq!(seed, Some((0, 0)), "cursor mid-span on line 4 seeds line 4");
    }

    #[test]
    fn cursor_at_span_start_seeds_that_occurrence() {
        // The post-jump "identifier selected" case: leading edge == span start (col 4).
        let refs = vec![rf("/a.rs", 4, 4, 9), rf("/a.rs", 8, 4, 9)];
        let ranked: Vec<u32> = vec![0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(4, 4));
        assert_eq!(seed, Some((0, 0)));
    }

    #[test]
    fn cursor_between_occurrences_takes_nearest_after() {
        // Cursor on line 6, contained by neither span → fall back to nearest at-or-after (line 8).
        let refs = vec![rf("/a.rs", 4, 4, 9), rf("/a.rs", 8, 4, 9)];
        let ranked: Vec<u32> = vec![0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(6, 0));
        assert_eq!(seed, Some((1, 1)));
    }

    #[test]
    fn cursor_past_all_occurrences_wraps_to_first() {
        // Nothing at-or-after the cursor → wrap to the file's first reference.
        let refs = vec![rf("/a.rs", 4, 4, 9), rf("/a.rs", 8, 4, 9)];
        let ranked: Vec<u32> = vec![0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(20, 0));
        assert_eq!(seed, Some((0, 0)));
    }

    #[test]
    fn only_active_file_references_are_considered() {
        // Candidate 0 is in another file; the cursor's containing ref (candidate 1) is in /a.rs.
        let refs = vec![rf("/b.rs", 4, 4, 9), rf("/a.rs", 4, 4, 9)];
        let ranked: Vec<u32> = vec![0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(4, 6));
        assert_eq!(
            seed,
            Some((1, 1)),
            "rank/index point at the /a.rs occurrence"
        );
    }

    #[test]
    fn rank_reflects_position_in_ranked_not_candidate_index() {
        // `ranked` is reordered (e.g. definition-first): candidate 2 sits at rank 0.
        let refs = vec![
            rf("/a.rs", 8, 4, 9),
            rf("/a.rs", 12, 4, 9),
            rf("/a.rs", 4, 4, 9),
        ];
        let ranked: Vec<u32> = vec![2, 0, 1];
        let seed = seed_reference_center(&refs, &ranked, Some("/a.rs"), at(4, 6));
        assert_eq!(seed, Some((0, 2)), "candidate 2 is at rank 0");
    }
}
