//! RPC method handlers. One function per protocol method.

use crate::cursor as motion;
use crate::error::RpcError;
use crate::state::{Buffer, ClientSession, EditKindTag, ServerState, SharedState, Viewport};
use crate::wrap;
use std::collections::HashMap;
use aether_protocol::buffer::{
    BufferCopyParams, BufferCopyResult, BufferCutResult, BufferOpenParams, BufferOpenResult,
    BufferSaveParams, BufferSaveResult, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorMoveParams, CursorSelectLineParams, CursorSetParams, CursorState, CursorSwapAnchorParams,
    Direction, Motion,
};
use aether_protocol::LogicalPosition;
use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::handshake::{ClientHelloParams, ClientHelloResult, ProjectInfo};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDeleteParams, InputTextParams, UndoResult,
};
use aether_protocol::viewport::{
    LogicalLineRange, LogicalLineRender, ViewportLinesChanged, ViewportLinesChangedParams,
    ViewportResizeParams, ViewportScrollParams, ViewportSubscribeParams, ViewportSubscribeResult,
    ViewportUnsubscribeParams, ViewportWindowResult, Window,
};
use aether_protocol::{BufferId, ClientId, Revision};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Per-connection context handed to handlers. Mutable bits live here; the durable state is in
/// [`SharedState`].
pub struct ConnectionCtx {
    /// `Some` once `client/hello` has succeeded.
    pub client_id: Option<ClientId>,
    /// Cloned into [`ClientSession::outbound`] so other tasks can push to this connection.
    pub outbound_tx: mpsc::Sender<Notification>,
}

impl ConnectionCtx {
    pub fn require_hello(&self) -> Result<ClientId, RpcError> {
        self.client_id.ok_or_else(|| {
            RpcError::new(ErrorCode::INVALID_REQUEST, "client/hello must be sent first")
        })
    }
}

// ---- handshake ---------------------------------------------------------------------------------

pub async fn client_hello(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ClientHelloParams,
) -> Result<ClientHelloResult, RpcError> {
    let (project_name, project_paths, server_token) = {
        let s = state.lock().await;
        (s.project_name.clone(), s.project_paths.clone(), s.token.clone())
    };
    if params.token != server_token {
        return Err(RpcError::invalid_token());
    }
    if ctx.client_id.is_some() {
        return Err(RpcError::new(
            ErrorCode::INVALID_REQUEST,
            "client/hello already sent for this connection",
        ));
    }
    let client_id = Uuid::new_v4();
    ctx.client_id = Some(client_id);

    let session = ClientSession { client_id, outbound: ctx.outbound_tx.clone() };
    state.lock().await.clients.insert(client_id, session);
    tracing::info!(%client_id, client_version = %params.client_version, "client registered");

    Ok(ClientHelloResult {
        client_id,
        server_version: env!("CARGO_PKG_VERSION").into(),
        project: ProjectInfo {
            name: project_name,
            paths: project_paths.iter().map(|p| p.display().to_string()).collect(),
        },
    })
}

// ---- buffer/open --------------------------------------------------------------------------------

pub async fn buffer_open(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: BufferOpenParams,
) -> Result<BufferOpenResult, RpcError> {
    let canonical = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let mut s = state.lock().await;
            let id = s.allocate_buffer_id();
            let buf = Buffer::scratch(id, params.language.clone());
            let result = BufferOpenResult {
                buffer_id: id,
                language: buf.language.clone(),
                line_count: buf.line_count(),
                byte_count: buf.byte_count(),
                revision: 0,
                dirty: false,
            };
            s.buffers.insert(id, buf);
            return Ok(result);
        }
        (Some(idx), rel) => {
            let s = state.lock().await;
            let base = s
                .project_paths
                .get(idx as usize)
                .ok_or_else(|| RpcError::invalid_path(format!("path_index {idx} out of range")))?
                .clone();
            drop(s);
            let base_is_file = base.is_file();
            let candidate = match rel {
                None | Some("") => base.clone(),
                Some(r) if base_is_file => {
                    return Err(RpcError::invalid_path(format!(
                        "path_index {idx} is a single-file entry; relative_path must be empty (got {r:?})"
                    )));
                }
                Some(r) => base.join(r),
            };
            std::fs::canonicalize(&candidate)
                .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", candidate.display())))?
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params(
                "relative_path provided without path_index",
            ));
        }
    };

    {
        let s = state.lock().await;
        if !s.path_is_in_project(&canonical) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the project's access boundary",
                canonical.display()
            )));
        }
        if let Some(existing) = s.buffer_for_path(&canonical) {
            let buf = &s.buffers[&existing];
            return Ok(BufferOpenResult {
                buffer_id: existing,
                language: buf.language.clone(),
                line_count: buf.line_count(),
                byte_count: buf.byte_count(),
                revision: buf.revision,
                dirty: buf.dirty,
            });
        }
    }

    let mut s = state.lock().await;
    let id = s.allocate_buffer_id();
    let buf = Buffer::load_from_file(id, canonical.clone()).map_err(RpcError::file_io)?;
    let result = BufferOpenResult {
        buffer_id: id,
        language: buf.language.clone(),
        line_count: buf.line_count(),
        byte_count: buf.byte_count(),
        revision: buf.revision,
        dirty: buf.dirty,
    };
    s.buffers.insert(id, buf);
    tracing::info!(buffer_id = id, path = %canonical.display(), "buffer opened");
    Ok(result)
}

// ---- buffer/save --------------------------------------------------------------------------------

pub async fn buffer_copy(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCopyResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let cursor = s.cursors.get(&(client_id, params.buffer_id)).copied().unwrap_or_default();
    let (start, end) = scope_range(buf, &cursor, params.scope);
    let text = buf.text.slice(start..end).to_string();
    Ok(BufferCopyResult { text })
}

pub async fn buffer_cut(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCutResult, RpcError> {
    let client_id = ctx.require_hello()?;

    // Extract the text and compute the range while holding the lock; then apply the deletion via
    // `Buffer::apply_edit` (which handles the undo entry and tree update) and broadcast.
    let mut s = state.lock().await;
    let cursor = s.cursors.get(&(client_id, params.buffer_id)).copied().unwrap_or_default();
    let buf_ref = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let (start_char, end_char) = scope_range(buf_ref, &cursor, params.scope);
    let text = buf_ref.text.slice(start_char..end_char).to_string();
    let start_pos = motion::char_to_pos(buf_ref, start_char);
    let end_pos_exclusive = motion::char_to_pos(buf_ref, end_char);
    let old_first_line = start_pos.line;
    let old_last_line_excl = end_pos_exclusive.line.saturating_add(1);

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| if *b == params.buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    let buf_mut = s.buffers.get_mut(&params.buffer_id).expect("just checked");
    let revision = buf_mut.apply_edit(start_char, end_char, "", EditKindTag::Delete, cursors_before);
    let new_cursor = CursorState { position: motion::char_to_pos(buf_mut, start_char), anchor: None };
    s.cursors.insert((client_id, params.buffer_id), new_cursor);

    let dirty = s.buffers[&params.buffer_id].dirty;
    let buf_ref = &s.buffers[&params.buffer_id];

    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != params.buffer_id {
            continue;
        }
        if !ranges_overlap(vp.first_logical_line, vp.last_logical_line_exclusive, old_first_line, old_last_line_excl) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision)));
    }
    let new_line_count = buf_ref.line_count();
    for vp in s.viewports.values_mut() {
        if vp.buffer_id != params.buffer_id {
            continue;
        }
        vp.first_logical_line = vp.first_logical_line.min(new_line_count);
        vp.last_logical_line_exclusive = vp.last_logical_line_exclusive.min(new_line_count);
    }

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(BufferCutResult { text, revision, cursor: new_cursor, dirty })
}

/// Compute the `[start_char, end_char)` range for a copy/cut scope.
fn scope_range(buf: &Buffer, cursor: &CursorState, scope: CopyScope) -> (usize, usize) {
    match scope {
        CopyScope::Selection => {
            if let Some(anchor) = cursor.anchor {
                let (start_pos, end_pos) = motion::ordered(cursor.position, anchor);
                let start = motion::pos_to_char(buf, start_pos);
                let end = motion::pos_to_char(buf, end_pos);
                (start, (end + 1).min(buf.text.len_chars()))
            } else {
                let start = motion::pos_to_char(buf, cursor.position);
                (start, (start + 1).min(buf.text.len_chars()))
            }
        }
        CopyScope::Line => {
            let line = cursor.position.line as usize;
            let start = buf.text.line_to_char(line);
            let end = if line + 1 < buf.text.len_lines() {
                buf.text.line_to_char(line + 1)
            } else {
                buf.text.len_chars()
            };
            (start, end)
        }
    }
}

pub async fn buffer_save(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferSaveParams,
) -> Result<BufferSaveResult, RpcError> {
    let _client_id = ctx.require_hello()?;

    // Resolve the target absolute path.
    let target: std::path::PathBuf = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let s = state.lock().await;
            let buf = s
                .buffers
                .get(&params.buffer_id)
                .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
            buf.canonical_path
                .clone()
                .ok_or_else(RpcError::buffer_has_no_path)?
        }
        (Some(idx), rel) => {
            let s = state.lock().await;
            let base = s
                .project_paths
                .get(idx as usize)
                .ok_or_else(|| RpcError::invalid_path(format!("path_index {idx} out of range")))?
                .clone();
            drop(s);

            let target = match rel {
                None | Some("") => base,
                Some(r) => base.join(r),
            };

            // The target file may not exist yet (creating); canonicalize the parent and join
            // the file name so the access-boundary check is meaningful.
            let parent = target.parent().ok_or_else(|| {
                RpcError::invalid_path(format!("{} has no parent directory", target.display()))
            })?;
            let parent_canonical = std::fs::canonicalize(parent).map_err(|e| {
                RpcError::invalid_path(format!("canonicalizing {}: {e}", parent.display()))
            })?;
            let file_name = target
                .file_name()
                .ok_or_else(|| RpcError::invalid_path("save target has no file name"))?;
            let resolved = parent_canonical.join(file_name);

            let s = state.lock().await;
            if !s.path_is_in_project(&resolved) {
                return Err(RpcError::invalid_path(format!(
                    "{} is outside the project's access boundary",
                    resolved.display()
                )));
            }
            resolved
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params("relative_path provided without path_index"));
        }
    };

    // Perform the write. I/O happens under the lock; in v1 that's acceptable (single client).
    // For multi-client we'd clone the rope, drop the lock, write, then re-lock to update state.
    let (saved_at_unix_ms, revision) = {
        let mut s = state.lock().await;
        let buf = s
            .buffers
            .get_mut(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let saved_at = buf.save_to_disk(target).map_err(RpcError::file_io)?;
        (saved_at, buf.revision)
    };

    // Broadcast buffer/state to all clients with viewports on this buffer.
    let pushes: Vec<(mpsc::Sender<Notification>, Notification)> = {
        let s = state.lock().await;
        let mut clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
        for vp in s.viewports.values() {
            if vp.buffer_id == params.buffer_id {
                clients.insert(vp.client_id);
            }
        }
        clients
            .into_iter()
            .filter_map(|cid| {
                let session = s.clients.get(&cid)?;
                let state_params = BufferStateParams {
                    buffer_id: params.buffer_id,
                    dirty: false,
                    revision,
                    saved_at_unix_ms: Some(saved_at_unix_ms),
                };
                let notif = Notification {
                    jsonrpc: JsonRpc,
                    method: BufferState::NAME.into(),
                    params: serde_json::to_value(state_params).expect("infallible"),
                };
                Some((session.outbound.clone(), notif))
            })
            .collect()
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(BufferSaveResult { saved_at_unix_ms, revision })
}

// ---- viewport handlers -------------------------------------------------------------------------

pub async fn viewport_subscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSubscribeParams,
) -> Result<ViewportSubscribeResult, RpcError> {
    let client_id = ctx.require_hello()?;

    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let line_count = buf.line_count();
    let buffer_id = buf.id;

    let (first, last_excl) = pushed_range(params.scroll.logical_line, params.rows, params.overscan_rows, line_count);
    let window = render_window(buf, first, last_excl, params.cols, params.wrap);

    let viewport_id = s.allocate_viewport_id();
    let viewport = Viewport {
        id: viewport_id,
        buffer_id,
        client_id,
        cols: params.cols,
        rows: params.rows,
        overscan_rows: params.overscan_rows,
        scroll_logical_line: params.scroll.logical_line,
        scroll_sub_row: params.scroll.sub_row,
        wrap: params.wrap,
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
    };
    s.viewports.insert(viewport_id, viewport);
    tracing::debug!(%client_id, viewport_id, buffer_id, first, last_excl, "viewport subscribed");

    Ok(ViewportSubscribeResult { viewport_id, window })
}

pub async fn viewport_resize(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportResizeParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.cols = params.cols;
    vp.rows = params.rows;
    let (cols, rows, overscan, wrap, buffer_id, scroll_line) =
        (vp.cols, vp.rows, vp.overscan_rows, vp.wrap, vp.buffer_id, vp.scroll_logical_line);

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let window = render_window(buf, first, last_excl, cols, wrap);

    let vp = s.viewports.get_mut(&params.viewport_id).expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_scroll(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportScrollParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.scroll_logical_line = params.scroll.logical_line;
    vp.scroll_sub_row = params.scroll.sub_row;
    let (cols, rows, overscan, wrap, buffer_id, scroll_line) =
        (vp.cols, vp.rows, vp.overscan_rows, vp.wrap, vp.buffer_id, vp.scroll_logical_line);

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let window = render_window(buf, first, last_excl, cols, wrap);

    let vp = s.viewports.get_mut(&params.viewport_id).expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_unsubscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportUnsubscribeParams,
) -> Result<(), RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let vp = s
        .viewports
        .get(&params.viewport_id)
        .ok_or_else(|| RpcError::new(ErrorCode::VIEWPORT_NOT_FOUND, format!("unknown viewport_id: {}", params.viewport_id)))?;
    if vp.client_id != client_id {
        return Err(RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            "viewport is not owned by this client",
        ));
    }
    s.viewports.remove(&params.viewport_id);
    Ok(())
}

// ---- helpers -----------------------------------------------------------------------------------

fn require_viewport_mut<'a>(
    state: &'a mut ServerState,
    viewport_id: aether_protocol::ViewportId,
    client_id: ClientId,
) -> Result<&'a mut Viewport, RpcError> {
    let vp = state
        .viewports
        .get_mut(&viewport_id)
        .ok_or_else(|| RpcError::new(ErrorCode::VIEWPORT_NOT_FOUND, format!("unknown viewport_id: {viewport_id}")))?;
    if vp.client_id != client_id {
        return Err(RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            "viewport is not owned by this client",
        ));
    }
    Ok(vp)
}

/// Compute the logical-line range to push for a viewport. Each logical line wraps to >= 1 visual
/// row, so sending `rows + 2*overscan_rows` logical lines is a safe over-approximation of the
/// visible + overscan area.
fn pushed_range(scroll_line: u32, rows: u32, overscan: u32, line_count: u32) -> (u32, u32) {
    let first = scroll_line.saturating_sub(overscan);
    let last_excl = scroll_line
        .saturating_add(rows)
        .saturating_add(overscan)
        .min(line_count);
    (first, last_excl.max(first))
}

fn render_window(
    buf: &Buffer,
    first: u32,
    last_excl: u32,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
) -> Window {
    let mut lines: Vec<LogicalLineRender> = Vec::with_capacity((last_excl - first) as usize);

    // For highlighting we need the whole source as bytes. Computed once per render rather than
    // per line. Skipped entirely when no syntax is attached.
    let source: Option<String> =
        buf.syntax.as_ref().map(|_| buf.text.chunks().collect::<String>());

    for i in first..last_excl {
        let line_slice = buf.text.line(i as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }

        let highlights = match (&buf.syntax, source.as_deref()) {
            (Some(syntax), Some(source)) => {
                let line_char_start = buf.text.line_to_char(i as usize);
                let line_byte_start = buf.text.char_to_byte(line_char_start);
                let line_byte_end = line_byte_start + text.len();
                crate::syntax::highlights_for_range(
                    syntax.config,
                    &syntax.tree,
                    source,
                    line_byte_start,
                    line_byte_end,
                )
            }
            _ => Vec::new(),
        };

        lines.push(wrap::render_line(&text, i, cols, wrap, highlights));
    }
    Window { first_logical_line: first, last_logical_line_exclusive: last_excl, lines }
}

// ---- cursor handlers ---------------------------------------------------------------------------

pub async fn cursor_move(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorMoveParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let new_pos = motion::resolve_motion(buf, current.position, &params.motion);
    let new_anchor = if params.extend_selection {
        current.anchor.or(Some(current.position))
    } else {
        None
    };
    // Collapse zero-width selections.
    let new_anchor = match new_anchor {
        Some(a) if a == new_pos => None,
        x => x,
    };

    let new_state = CursorState { position: new_pos, anchor: new_anchor };
    s.cursors.insert(key, new_state);
    Ok(new_state)
}

/// Whole-line selection in either direction. The result is always whole lines (anchor at col 0
/// of one line, cursor at the end byte of another); orientation (forward / backward) is whatever
/// the input was.
///
/// Forward always grows the *bottom-most* edge of the selection downward; backward always grows
/// the *top-most* edge upward. This means the operation looks the same to the user regardless of
/// which end the cursor sits on — useful after `cursor/swap_anchor`. The cursor stays at the end
/// it was already on; the anchor occupies the other end.
pub async fn cursor_select_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSelectLineParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let cur = current.position;

    // Top / bottom edges of the current selection, normalized so we can reason about "extend the
    // bottom down" independent of which end the cursor sits on. Without an anchor, both are at
    // the cursor.
    let (top_edge, bottom_edge) = match current.anchor {
        Some(a) if (a.line, a.col) < (cur.line, cur.col) => (a, cur),
        Some(a) => (cur, a),
        None => (cur, cur),
    };
    let cursor_was_at_top = current.anchor.is_some() && cur == top_edge;

    let (top_line, bottom_line) = match (params.direction, params.extend, current.anchor) {
        // No prior selection: the line is picked by the cursor's position relative to its
        // line's end. End-of-line is the trigger because that's where the cursor naturally
        // lands after typing — forward advances past it, backward stays on the current line.
        (Direction::Forward, _, None) => {
            let len = motion::line_byte_len_excl_newline(buf, cur.line);
            let at_end = cur.col >= len;
            let line = if at_end { cur.line.saturating_add(1) } else { cur.line };
            (line, line)
        }
        (Direction::Backward, _, None) => {
            let len = motion::line_byte_len_excl_newline(buf, cur.line);
            let at_end = cur.col >= len;
            let line = if at_end { cur.line } else { cur.line.saturating_sub(1) };
            (line, line)
        }
        // With an existing selection: walk the relevant edge. If it's already at its line
        // boundary (end for forward, col 0 for backward) advance to the next line; otherwise
        // snap to that boundary first. For non-extend, both edges collapse onto the moved one.
        (Direction::Forward, extend, Some(_)) => {
            let len = motion::line_byte_len_excl_newline(buf, bottom_edge.line);
            let at_end = bottom_edge.col >= len;
            let new_bottom = if at_end {
                bottom_edge.line.saturating_add(1)
            } else {
                bottom_edge.line
            };
            if extend {
                (top_edge.line, new_bottom)
            } else {
                (new_bottom, new_bottom)
            }
        }
        (Direction::Backward, extend, Some(_)) => {
            let new_top = if top_edge.col == 0 {
                top_edge.line.saturating_sub(1)
            } else {
                top_edge.line
            };
            if extend {
                (new_top, bottom_edge.line)
            } else {
                (new_top, new_top)
            }
        }
    };

    let last_line = (buf.text.len_lines() as u32).saturating_sub(1);
    let top_line = top_line.min(last_line);
    let bottom_line = bottom_line.min(last_line);
    let top_pos = LogicalPosition { line: top_line, col: 0 };
    let bottom_pos = LogicalPosition {
        line: bottom_line,
        col: motion::line_byte_len_excl_newline(buf, bottom_line),
    };
    // Cursor stays at the end it occupied (top or bottom). Default to bottom for a fresh
    // selection so the result is forward-oriented.
    let (cursor_pos, anchor_pos) = if cursor_was_at_top {
        (top_pos, bottom_pos)
    } else {
        (bottom_pos, top_pos)
    };
    let anchor = if anchor_pos == cursor_pos { None } else { Some(anchor_pos) };
    let new_state = CursorState { position: cursor_pos, anchor };
    s.cursors.insert(key, new_state);
    Ok(new_state)
}

pub async fn cursor_swap_anchor(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSwapAnchorParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let new_state = match current.anchor {
        Some(a) => CursorState { position: a, anchor: Some(current.position) },
        None => current,
    };
    s.cursors.insert(key, new_state);
    Ok(new_state)
}

pub async fn cursor_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSetParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let position = motion::clamp_position(buf, params.position);
    let anchor = params.anchor.map(|a| motion::clamp_position(buf, a));
    let anchor = match anchor {
        Some(a) if a == position => None,
        x => x,
    };
    let result = CursorState { position, anchor };
    s.cursors.insert((client_id, params.buffer_id), result);
    Ok(result)
}

// ---- input handlers ----------------------------------------------------------------------------

pub async fn input_text(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputTextParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith { text: params.text, select_pasted: params.select_pasted },
    )
    .await
}

pub async fn input_delete(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputDeleteParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    apply_edit(state, client_id, params.buffer_id, EditKind::DeleteMotion(params.motion)).await
}

pub async fn input_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<UndoResult, RpcError> {
    apply_undo_or_redo(state, ctx, params.buffer_id, UndoDirection::Undo).await
}

pub async fn input_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<UndoResult, RpcError> {
    apply_undo_or_redo(state, ctx, params.buffer_id, UndoDirection::Redo).await
}

pub async fn input_join_lines(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let buffer_id = params.buffer_id;

    // Figure out which line(s) we're joining. If the cursor has a selection that spans multiple
    // lines, join all of them. Otherwise, join the cursor's line with the one below.
    let (first_line, last_line) = {
        let s = state.lock().await;
        let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();
        let (a, b) = match cursor.anchor {
            Some(anchor) => motion::ordered(cursor.position, anchor),
            None => (cursor.position, cursor.position),
        };
        let buf = s
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        let line_count = buf.line_count();
        let first = a.line;
        // If single line, join with the line below it. If multi-line selection, join through
        // last selected line.
        let last = if a.line == b.line { a.line.saturating_add(1) } else { b.line };
        let last = last.min(line_count.saturating_sub(1));
        (first, last)
    };

    if first_line >= last_line {
        // Nothing to join (we're on the last line).
        let s = state.lock().await;
        let buf = &s.buffers[&buffer_id];
        return Ok(EditResult {
            revision: buf.revision,
            cursor: s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default(),
            dirty: buf.dirty,
        });
    }

    // Compute the joined range, in char offsets. For each pair of consecutive lines, the range
    // to replace is `[end_of_trailing_ws_on_line_i, first_non_ws_on_line_i+1)` — replaced with
    // a single space. We do them in a single sweep on the rope.
    let s = state.lock().await;
    let buf = &s.buffers[&buffer_id];

    // Build the full replacement: walk the lines from `first_line` to `last_line`, concatenating
    // each line's content (stripped of trailing whitespace) plus a single space between.
    let mut joined = String::new();
    for line_idx in first_line..=last_line {
        let line_slice = buf.text.line(line_idx as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        if line_idx == first_line {
            // Keep first line's content, drop trailing whitespace.
            joined.push_str(text.trim_end());
        } else {
            joined.push(' ');
            // Drop leading whitespace on continuation lines; keep trailing untouched until
            // the next loop iteration trims it.
            let trimmed_start = text.trim_start();
            // For the last line, keep trailing whitespace as it normally appears.
            if line_idx == last_line {
                joined.push_str(trimmed_start);
            } else {
                joined.push_str(trimmed_start.trim_end());
            }
        }
    }

    // Determine the range to replace (full first..=last lines).
    let first_char = buf.text.line_to_char(first_line as usize);
    let last_line_end_char = if (last_line as usize + 1) < buf.text.len_lines() {
        // Up to (but not including) the \n at the end of `last_line`.
        let next_start = buf.text.line_to_char(last_line as usize + 1);
        next_start - 1
    } else {
        buf.text.len_chars()
    };
    drop(s);

    let cursors_before: HashMap<ClientId, CursorState> = {
        let s = state.lock().await;
        s.cursors
            .iter()
            .filter_map(|((c, b), cs)| if *b == buffer_id { Some((*c, *cs)) } else { None })
            .collect()
    };

    let (revision, dirty, new_cursor) = {
        let mut s = state.lock().await;
        let buf = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf.apply_edit(
            first_char,
            last_line_end_char,
            &joined,
            EditKindTag::Text,
            cursors_before,
        );
        let new_cursor_char = first_char + joined.chars().count();
        let new_cursor = CursorState {
            position: motion::char_to_pos(buf, new_cursor_char),
            anchor: None,
        };
        let dirty = buf.dirty;
        s.cursors.insert((client_id, buffer_id), new_cursor);
        (revision, dirty, new_cursor)
    };

    // Push viewport/lines_changed for affected viewports (we changed multiple lines).
    let pushes: Vec<(mpsc::Sender<Notification>, Notification)> = {
        let s = state.lock().await;
        let buf = &s.buffers[&buffer_id];
        let mut pushes = Vec::new();
        let new_line_count = buf.line_count();
        for vp in s.viewports.values() {
            if vp.buffer_id != buffer_id {
                continue;
            }
            let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
                continue;
            };
            pushes.push((sender, build_lines_changed_notif(buf, vp, revision)));
            let _ = new_line_count; // viewport range clamp not needed here; render handles it
        }
        pushes
    };
    // Clamp viewport ranges to new line count.
    {
        let mut s = state.lock().await;
        let new_line_count = s.buffers[&buffer_id].line_count();
        for vp in s.viewports.values_mut() {
            if vp.buffer_id != buffer_id {
                continue;
            }
            vp.first_logical_line = vp.first_logical_line.min(new_line_count);
            vp.last_logical_line_exclusive = vp.last_logical_line_exclusive.min(new_line_count);
        }
    }

    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult { revision, cursor: new_cursor, dirty })
}

enum UndoDirection {
    Undo,
    Redo,
}

async fn apply_undo_or_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: UndoDirection,
) -> Result<UndoResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;

    // Snapshot current cursors so the *other* direction's stack can restore them later.
    let current_cursors: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| if *b == buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    let outcome = {
        let buf = s
            .buffers
            .get_mut(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        match direction {
            UndoDirection::Undo => buf.undo(current_cursors),
            UndoDirection::Redo => buf.redo(current_cursors),
        }
    };

    let Some(outcome) = outcome else {
        // Nothing to undo/redo. Echo current cursor and revision back.
        let buf = s.buffers.get(&buffer_id).expect("just checked");
        let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();
        return Ok(UndoResult {
            revision: buf.revision,
            applied: false,
            cursor,
            dirty: buf.dirty,
        });
    };

    let buf = s.buffers.get(&buffer_id).expect("just modified");
    let revision = buf.revision;
    let dirty = buf.dirty;

    // Restore cursors from the snapshot, clamped to valid positions in the restored rope.
    let mut new_cursors: HashMap<ClientId, CursorState> = HashMap::new();
    for (cid, cursor) in &outcome.restored_cursors {
        new_cursors.insert(*cid, clamp_cursor(buf, *cursor));
    }
    // Clients with cursors on this buffer that weren't in the snapshot: just clamp their current
    // cursor to the new buffer bounds.
    let existing_cursor_ids: Vec<ClientId> = s
        .cursors
        .keys()
        .filter_map(|(c, b)| if *b == buffer_id { Some(*c) } else { None })
        .collect();
    for cid in existing_cursor_ids {
        if !new_cursors.contains_key(&cid) {
            if let Some(cursor) = s.cursors.get(&(cid, buffer_id)).copied() {
                new_cursors.insert(cid, clamp_cursor(buf, cursor));
            }
        }
    }
    for (cid, cursor) in &new_cursors {
        s.cursors.insert((*cid, buffer_id), *cursor);
    }
    let undoing_cursor =
        new_cursors.get(&client_id).copied().unwrap_or_else(CursorState::default);

    // Push the full visible window to every viewport on this buffer — the rope was swapped
    // wholesale, so we can't be surgical about it.
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    let new_line_count = buf_ref.line_count();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision)));
    }
    // Clamp viewport pushed ranges to the new line count.
    for vp in s.viewports.values_mut() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        vp.first_logical_line = vp.first_logical_line.min(new_line_count);
        vp.last_logical_line_exclusive = vp.last_logical_line_exclusive.min(new_line_count);
    }

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(UndoResult { revision, applied: true, cursor: undoing_cursor, dirty })
}

fn clamp_cursor(buf: &Buffer, cursor: CursorState) -> CursorState {
    let position = motion::clamp_position(buf, cursor.position);
    let anchor = cursor.anchor.map(|a| motion::clamp_position(buf, a));
    let anchor = match anchor {
        Some(a) if a == position => None,
        x => x,
    };
    CursorState { position, anchor }
}

enum EditKind {
    /// Replace the selection with `text` (insert at cursor if no selection). If `select_pasted`
    /// is true, the post-edit cursor selects the inserted text instead of collapsing past it.
    ReplaceWith { text: String, select_pasted: bool },
    /// Delete from cursor through the motion's endpoint, or the selection if any.
    DeleteMotion(Motion),
}

async fn apply_edit(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    edit: EditKind,
) -> Result<EditResult, RpcError> {
    // Phase 1: hold the lock for the whole edit; gather notification senders before dropping it.
    let mut s = state.lock().await;

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();

    // Determine the byte/char range to replace, and the text to insert.
    let (start_pos, end_pos) = if let Some(anchor) = cursor.anchor {
        motion::ordered(cursor.position, anchor)
    } else {
        match &edit {
            EditKind::ReplaceWith { .. } => (cursor.position, cursor.position),
            EditKind::DeleteMotion(m) => {
                let target = motion::resolve_motion(buf, cursor.position, m);
                motion::ordered(cursor.position, target)
            }
        }
    };
    let (insert_text, select_pasted): (&str, bool) = match &edit {
        EditKind::ReplaceWith { text, select_pasted } => (text.as_str(), *select_pasted),
        EditKind::DeleteMotion(_) => ("", false),
    };

    let start_char = motion::pos_to_char(buf, start_pos);
    let end_char_base = motion::pos_to_char(buf, end_pos);
    // When an anchor exists, the selection conceptually includes the position char (the one
    // under the block cursor). Operationally extend the half-open range by one char so the
    // visible block cursor's char is part of the affected range.
    let end_char = if cursor.anchor.is_some() {
        end_char_base.saturating_add(1).min(buf.text.len_chars())
    } else {
        end_char_base
    };
    let old_first_line = start_pos.line;
    let old_last_line = end_pos.line;
    let kind_tag = match &edit {
        EditKind::ReplaceWith { .. } => EditKindTag::Text,
        EditKind::DeleteMotion(_) => EditKindTag::Delete,
    };

    // Snapshot all per-client cursors on this buffer so the undo entry can restore them.
    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, b), cs)| if *b == buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    // Mutate the buffer (rope edit + incremental reparse + undo-group bookkeeping).
    let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
    let revision = buf_mut.apply_edit(start_char, end_char, insert_text, kind_tag, cursors_before);

    // Compute the cursor's new position.
    let inserted_char_count = insert_text.chars().count();
    let new_cursor_state = if select_pasted && inserted_char_count > 0 {
        // After pasting, select the inserted text. Block cursor on the last inserted char.
        let last_char = start_char + inserted_char_count - 1;
        let anchor_pos = motion::char_to_pos(buf_mut, start_char);
        let position_pos = motion::char_to_pos(buf_mut, last_char);
        if anchor_pos == position_pos {
            CursorState { position: position_pos, anchor: None }
        } else {
            CursorState { position: position_pos, anchor: Some(anchor_pos) }
        }
    } else {
        CursorState {
            position: motion::char_to_pos(buf_mut, start_char + inserted_char_count),
            anchor: None,
        }
    };
    s.cursors.insert((client_id, buffer_id), new_cursor_state);

    // Collect notifications for all viewports whose pushed range intersects the edit.
    let edit_first = old_first_line;
    let edit_last_excl = old_last_line.saturating_add(1);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !ranges_overlap(vp.first_logical_line, vp.last_logical_line_exclusive, edit_first, edit_last_excl) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        let notif = build_lines_changed_notif(buf_ref, vp, revision);
        pushes.push((sender, notif));
    }

    // Also: clamp viewports' pushed ranges in case the buffer shrank. (We're re-using values from
    // before the mutation; refresh from current line count.)
    let new_line_count = s.buffers[&buffer_id].line_count();
    for vp in s.viewports.values_mut() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        vp.first_logical_line = vp.first_logical_line.min(new_line_count);
        vp.last_logical_line_exclusive = vp.last_logical_line_exclusive.min(new_line_count);
    }

    let dirty = s.buffers[&buffer_id].dirty;
    drop(s);

    for (sender, notif) in pushes {
        // If the receiver's gone, the client's connection has dropped; not our problem.
        let _ = sender.send(notif).await;
    }

    Ok(EditResult { revision, cursor: new_cursor_state, dirty })
}

fn ranges_overlap(a_start: u32, a_end_excl: u32, b_start: u32, b_end_excl: u32) -> bool {
    a_start < b_end_excl && b_start < a_end_excl
}

fn build_lines_changed_notif(buffer: &Buffer, vp: &Viewport, revision: Revision) -> Notification {
    let line_count = buffer.line_count();
    let new_first = vp.first_logical_line.min(line_count);
    let new_last_excl = vp.last_logical_line_exclusive.min(line_count).max(new_first);
    let window = render_window(buffer, new_first, new_last_excl, vp.cols, vp.wrap);
    let params = ViewportLinesChangedParams {
        viewport_id: vp.id,
        revision,
        range: LogicalLineRange {
            start_logical_line: vp.first_logical_line,
            end_logical_line_exclusive: vp.last_logical_line_exclusive,
        },
        replacement_lines: window.lines,
    };
    Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: serde_json::to_value(params).expect("infallible"),
    }
}

