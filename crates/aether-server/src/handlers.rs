//! RPC method handlers. One function per protocol method.

use crate::cursor as motion;
use crate::error::RpcError;
use crate::picker as picker_state;
use crate::state::{Buffer, ClientSession, EditKindTag, SearchEntry, ServerState, SharedState, Viewport};
use crate::wrap;
use std::collections::HashMap;
use std::sync::Arc;
use aether_protocol::buffer::{
    BufferCopyParams, BufferCopyResult, BufferCutResult, BufferOpenParams, BufferOpenResult,
    BufferSaveParams, BufferSaveResult, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::directory::{
    DirectoryCreateParams, DirectoryCreateResult, DirectoryListParams, DirectoryListResult,
    DirEntry,
};
use aether_protocol::picker::{
    PickerHideParams, PickerItem, PickerQueryParams, PickerSelectParams, PickerSelectResult,
    PickerUpdate, PickerViewParams, PickerViewResult,
};
use aether_protocol::search::{
    SearchClearParams, SearchMatchRange, SearchNavParams, SearchNavResult, SearchSetParams,
    SearchSetResult, SearchStateChanged, SearchSummary,
};
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorMoveParams, CursorSelectLineParams, CursorSetParams,
    CursorState, CursorSwapAnchorParams, CursorUndoParams, CursorUndoResult, Direction, Motion,
    VerticalDirection,
};
use crate::state::MOTION_HISTORY_CAP;
use aether_protocol::LogicalPosition;
use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::handshake::{ClientHelloParams, ClientHelloResult, ProjectInfo};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDeleteParams, InputMoveLinesParams, InputTextParams,
    UndoResult,
};
use aether_protocol::viewport::{
    LogicalLineRange, LogicalLineRender, ViewportLinesChanged, ViewportLinesChangedParams,
    ViewportResizeParams, ViewportScrollParams, ViewportSetWrapParams, ViewportSubscribeParams,
    ViewportSubscribeResult, ViewportUnsubscribeParams, ViewportWindowResult, Window,
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
    ctx: &mut ConnectionCtx,
    params: BufferOpenParams,
) -> Result<BufferOpenResult, RpcError> {
    // Used to surface this client's prior cursor + scroll for the buffer on reopen. `None` when
    // the client hasn't sent `client/hello` yet — in that case there's no per-client state to
    // restore, and the result's `cursor` / `scroll` fields fall back to defaults.
    let client_id = ctx.client_id;
    let canonical = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let mut s = state.lock().await;
            let id = s.allocate_buffer_id();
            let buf = Buffer::scratch(id, params.language.clone());
            let cursor = client_id
                .and_then(|c| s.cursors.get(&(c, id)).copied())
                .unwrap_or_default();
            let scroll = client_id.and_then(|c| s.last_scroll.get(&(c, id)).copied());
            let result = BufferOpenResult {
                buffer_id: id,
                language: buf.language.clone(),
                line_count: buf.line_count(),
                byte_count: buf.byte_count(),
                revision: 0,
                saved_revision: buf.saved_revision(),
                path: None,
                cursor,
                scroll,
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
            // Canonicalize the parent (which must exist) and then re-attach the file name. This
            // lets us resolve the absolute path even when `create_if_missing` is set and the
            // file itself doesn't exist yet.
            match std::fs::canonicalize(&candidate) {
                Ok(p) => p,
                Err(_) if params.create_if_missing => {
                    let parent = candidate.parent().ok_or_else(|| {
                        RpcError::invalid_path(format!("no parent for {}", candidate.display()))
                    })?;
                    let parent_canonical = std::fs::canonicalize(parent).map_err(|e| {
                        RpcError::invalid_path(format!("canonicalizing {}: {e}", parent.display()))
                    })?;
                    let file_name = candidate.file_name().ok_or_else(|| {
                        RpcError::invalid_path(format!("no file name in {}", candidate.display()))
                    })?;
                    parent_canonical.join(file_name)
                }
                Err(e) => {
                    return Err(RpcError::invalid_path(format!(
                        "canonicalizing {}: {e}",
                        candidate.display()
                    )));
                }
            }
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
            let cursor = client_id
                .and_then(|c| s.cursors.get(&(c, existing)).copied())
                .unwrap_or_default();
            let scroll = client_id.and_then(|c| s.last_scroll.get(&(c, existing)).copied());
            return Ok(BufferOpenResult {
                buffer_id: existing,
                language: buf.language.clone(),
                line_count: buf.line_count(),
                byte_count: buf.byte_count(),
                revision: buf.revision,
                saved_revision: buf.saved_revision(),
                path: Some(canonical.display().to_string()),
                cursor,
                scroll,
            });
        }
    }

    let mut s = state.lock().await;
    let id = s.allocate_buffer_id();
    let buf = if params.create_if_missing && !canonical.exists() {
        // New file: empty buffer with the target path attached. Save will write to disk.
        Buffer::new_at_path(id, canonical.clone(), params.language.clone())
    } else {
        Buffer::load_from_file(id, canonical.clone()).map_err(RpcError::file_io)?
    };
    // First-time open of this buffer: no prior cursor or scroll to surface — but the client could
    // already have one if a previous server-side session allocated state. Look it up anyway for
    // consistency with the reopen path.
    let cursor = client_id
        .and_then(|c| s.cursors.get(&(c, id)).copied())
        .unwrap_or_default();
    let scroll = client_id.and_then(|c| s.last_scroll.get(&(c, id)).copied());
    let result = BufferOpenResult {
        buffer_id: id,
        language: buf.language.clone(),
        line_count: buf.line_count(),
        byte_count: buf.byte_count(),
        revision: buf.revision,
        saved_revision: buf.saved_revision(),
        path: Some(canonical.display().to_string()),
        cursor,
        scroll,
    };
    s.buffers.insert(id, buf);
    tracing::info!(buffer_id = id, path = %canonical.display(), "buffer opened");
    Ok(result)
}

// ---- buffer/search ------------------------------------------------------------------------------

/// Stateless regex search. Returns up to `MAX_MATCHES` matches; the client is responsible for
/// stashing them and re-issuing the RPC after edits. Smartcase: case-insensitive unless the
/// query has any uppercase character. An empty query returns an empty list.
// ---- directory/* -------------------------------------------------------------------------------

pub async fn directory_list(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: DirectoryListParams,
) -> Result<DirectoryListResult, RpcError> {
    let _ = ctx.require_hello()?;
    let s = state.lock().await;

    // Resolve the requested path. `None` means "first project path".
    let raw_path = match params.path.as_deref() {
        Some(p) => std::path::PathBuf::from(p),
        None => s
            .project_paths
            .first()
            .ok_or_else(|| RpcError::invalid_path("no project paths configured"))?
            .clone(),
    };
    let canonical = std::fs::canonicalize(&raw_path)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", raw_path.display())))?;
    if !s.path_is_in_project(&canonical) {
        return Err(RpcError::invalid_path(format!(
            "{} is outside the project's access boundary",
            canonical.display()
        )));
    }
    let metadata = std::fs::metadata(&canonical).map_err(RpcError::file_io)?;
    if !metadata.is_dir() {
        return Err(RpcError::invalid_path(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }

    // The parent is allowed only if it's still inside the project.
    let parent = canonical
        .parent()
        .and_then(|p| {
            let p = p.to_path_buf();
            if s.path_is_in_project(&p) { Some(p.display().to_string()) } else { None }
        });

    let mut entries: Vec<DirEntry> = Vec::new();
    let read = std::fs::read_dir(&canonical).map_err(RpcError::file_io)?;
    for ent in read {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match ent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue, // non-UTF8 filename — skip
        };
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(DirEntry { name, is_dir });
    }
    // Directories first, then files, each alphabetical.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });

    Ok(DirectoryListResult {
        path: canonical.display().to_string(),
        parent,
        entries,
    })
}

pub async fn directory_create(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: DirectoryCreateParams,
) -> Result<DirectoryCreateResult, RpcError> {
    let _ = ctx.require_hello()?;
    let raw = std::path::PathBuf::from(&params.path);

    // The target itself may not exist yet; canonicalize the nearest existing ancestor so we can
    // validate the path is in the project even before creation.
    let mut anchor = raw.clone();
    loop {
        if anchor.exists() {
            break;
        }
        match anchor.parent() {
            Some(p) if !p.as_os_str().is_empty() => anchor = p.to_path_buf(),
            _ => return Err(RpcError::invalid_path(format!("no existing ancestor for {}", raw.display()))),
        }
    }
    let anchor_canonical = std::fs::canonicalize(&anchor)
        .map_err(|e| RpcError::invalid_path(format!("canonicalizing {}: {e}", anchor.display())))?;
    {
        let s = state.lock().await;
        if !s.path_is_in_project(&anchor_canonical) {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the project's access boundary",
                anchor_canonical.display()
            )));
        }
    }

    // Build the final target by suffixing the relative remainder onto the canonical anchor.
    let suffix = raw
        .strip_prefix(&anchor)
        .unwrap_or_else(|_| std::path::Path::new(""))
        .to_path_buf();
    let target = anchor_canonical.join(&suffix);

    if target.exists() && !target.is_dir() {
        return Err(RpcError::invalid_path(format!(
            "{} exists and is not a directory",
            target.display()
        )));
    }
    std::fs::create_dir_all(&target).map_err(RpcError::file_io)?;
    let canonical = std::fs::canonicalize(&target).map_err(RpcError::file_io)?;
    Ok(DirectoryCreateResult { path: canonical.display().to_string() })
}

// ---- search/* ----------------------------------------------------------------------------------

pub const SEARCH_MAX_MATCHES: usize = 10_000;

/// Run `query` against the buffer and produce a fresh `SearchEntry`. Smartcase (case-insensitive
/// unless the query has any uppercase) and `multi_line: true`. Zero-width matches are skipped so
/// patterns like `^` don't pin the cursor.
pub fn compute_search_entry(buf: &Buffer, query: &str) -> Result<SearchEntry, RpcError> {
    if query.is_empty() {
        return Ok(SearchEntry {
            query: String::new(),
            matches: Vec::new(),
            truncated: false,
            last_pushed_index: 0,
        });
    }
    let regex = {
        let has_upper = query.chars().any(|c| c.is_uppercase());
        regex::RegexBuilder::new(query)
            .case_insensitive(!has_upper)
            .multi_line(true)
            .build()
            .map_err(|e| RpcError::new(ErrorCode::INVALID_PARAMS, format!("invalid regex: {e}")))?
    };
    let mut matches: Vec<(LogicalPosition, LogicalPosition)> = Vec::new();
    let mut truncated = false;
    let len_bytes = buf.text.len_bytes();
    if len_bytes == 0 {
        return Ok(SearchEntry {
            query: query.to_string(),
            matches,
            truncated,
            last_pushed_index: 0,
        });
    }
    let source: String = buf.text.chunks().collect();
    for m in regex.find_iter(&source) {
        if matches.len() >= SEARCH_MAX_MATCHES {
            truncated = true;
            break;
        }
        if m.start() == m.end() {
            continue;
        }
        matches.push((byte_to_logical(buf, m.start()), byte_to_logical(buf, m.end())));
    }
    Ok(SearchEntry {
        query: query.to_string(),
        matches,
        truncated,
        last_pushed_index: 0,
    })
}

pub async fn search_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchSetParams,
) -> Result<SearchSetResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);

    let mut cursor = s.cursors.get(&key).copied().unwrap_or_default();
    let (summary, pushes) = if params.query.is_empty() {
        s.searches.remove(&key);
        let summary = SearchSummary {
            buffer_id: params.buffer_id,
            total: 0,
            truncated: false,
            current_index: 0,
        };
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    } else {
        let mut entry = compute_search_entry(buf, &params.query)?;
        // If the caller passed an anchor, jump the cursor to the first match at-or-after it
        // (wrapping to the first match if none). This is how incremental search keeps the cursor
        // anchored at `/`-press time across keystrokes.
        if let Some(anchor_pos) = params.anchor {
            if let Some((start, end_excl)) = first_match_at_or_after_with_wrap(&entry, anchor_pos) {
                let start_char = motion::pos_to_char(buf, start);
                let end_char_excl = motion::pos_to_char(buf, end_excl);
                let last_char = end_char_excl.saturating_sub(1).max(start_char);
                let position = motion::char_to_pos(buf, last_char);
                let anchor_p = motion::char_to_pos(buf, start_char);
                let new_cursor = if anchor_p == position {
                    CursorState { position, anchor: None, match_bracket: None }
                } else {
                    CursorState { position, anchor: Some(anchor_p), match_bracket: None }
                };
                let prev_cursor = cursor;
                s.cursors.insert(key, new_cursor);
                s.record_motion(key, prev_cursor, new_cursor);
                s.virtual_col.remove(&key);
                s.clear_tree_selection_history(client_id, params.buffer_id);
                cursor = new_cursor;
            }
        }
        let buf_ref = &s.buffers[&params.buffer_id];
        let summary = summary_for(buf_ref, &entry, params.buffer_id, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(SearchSetResult { cursor, summary })
}

fn first_match_at_or_after_with_wrap(
    entry: &SearchEntry,
    pos: LogicalPosition,
) -> Option<(LogicalPosition, LogicalPosition)> {
    entry
        .matches
        .iter()
        .copied()
        .find(|(start, _)| pos_tuple(*start) >= pos_tuple(pos))
        .or_else(|| entry.matches.first().copied())
}

pub async fn search_clear(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchClearParams,
) -> Result<(), RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    s.searches.remove(&(client_id, params.buffer_id));
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}

pub async fn search_next(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchNavParams,
) -> Result<SearchNavResult, RpcError> {
    search_navigate(state, ctx, params.buffer_id, Direction::Forward).await
}

pub async fn search_prev(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchNavParams,
) -> Result<SearchNavResult, RpcError> {
    search_navigate(state, ctx, params.buffer_id, Direction::Backward).await
}

async fn search_navigate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: Direction,
) -> Result<SearchNavResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let key = (client_id, buffer_id);
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let Some(entry) = s.searches.get(&key) else {
        // No active search — return a zero-summary with the current cursor untouched.
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: SearchSummary { buffer_id, total: 0, truncated: false, current_index: 0 },
        });
    };
    if entry.matches.is_empty() {
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: summary_for(buf, entry, buffer_id, &cursor),
        });
    }

    // Use the leftmost end of the current selection as the reference, so a `prev` from a match
    // doesn't re-select the current match. If the cursor isn't on a match, pick the natural
    // direction from the cursor's position.
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let reference = selection_start(&current);
    let target = match direction {
        Direction::Forward => entry
            .matches
            .iter()
            .copied()
            .find(|(start, _)| pos_tuple(*start) > pos_tuple(reference))
            .or_else(|| entry.matches.first().copied()),
        Direction::Backward => entry
            .matches
            .iter()
            .rev()
            .copied()
            .find(|(start, _)| pos_tuple(*start) < pos_tuple(reference))
            .or_else(|| entry.matches.last().copied()),
    };
    let Some((start, end_excl)) = target else {
        return Ok(SearchNavResult {
            cursor: current,
            summary: summary_for(buf, entry, buffer_id, &current),
        });
    };

    // Place anchor at start, cursor at the last char of the match. We compute the inclusive end
    // here (one char before the exclusive end) using char-index arithmetic, mirroring how
    // `Char` motion does it — that way multi-byte matches stay on char boundaries.
    let start_char = motion::pos_to_char(buf, start);
    let end_char_excl = motion::pos_to_char(buf, end_excl);
    let last_char = end_char_excl.saturating_sub(1).max(start_char);
    let position = motion::char_to_pos(buf, last_char);
    let anchor_pos = motion::char_to_pos(buf, start_char);
    let new_cursor = if anchor_pos == position {
        CursorState { position, anchor: None, match_bracket: None }
    } else {
        CursorState { position, anchor: Some(anchor_pos), match_bracket: None }
    };
    let prev_cursor = s.cursors.get(&key).copied().unwrap_or_default();
    s.cursors.insert(key, new_cursor);
    s.record_motion(key, prev_cursor, new_cursor);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, buffer_id);
    let buf_ref = &s.buffers[&buffer_id];
    let summary = {
        let entry_ref = s.searches.get(&key).expect("active search just confirmed");
        summary_for(buf_ref, entry_ref, buffer_id, &new_cursor)
    };
    let entry_mut = s.searches.get_mut(&key).expect("active search just confirmed");
    entry_mut.last_pushed_index = summary.current_index;
    Ok(SearchNavResult { cursor: new_cursor, summary })
}

fn selection_start(c: &CursorState) -> LogicalPosition {
    match c.anchor {
        Some(a) if pos_tuple(a) < pos_tuple(c.position) => a,
        _ => c.position,
    }
}

fn pos_tuple(p: LogicalPosition) -> (u32, u32) { (p.line, p.col) }

/// Compute the `SearchSummary` for the given entry and cursor.
fn summary_for(
    buf: &Buffer,
    entry: &SearchEntry,
    buffer_id: BufferId,
    cursor: &CursorState,
) -> SearchSummary {
    let current_index = match_index_for_cursor(buf, entry, cursor);
    SearchSummary {
        buffer_id,
        total: entry.matches.len() as u32,
        truncated: entry.truncated,
        current_index,
    }
}

/// 1-based index of the match whose range exactly equals the cursor's current selection
/// (`anchor == m.start` *and* `cursor == last char of m`), or `0` if no match matches. Single-char
/// matches collapse the anchor (server normalizes `anchor == position` to `None`), so we handle
/// that case too. Comparing both endpoints means the counter only shows when the user is
/// genuinely "on" a match — extending or shrinking the selection drops the counter.
fn match_index_for_cursor(buf: &Buffer, entry: &SearchEntry, cursor: &CursorState) -> u32 {
    let pos_char = motion::pos_to_char(buf, cursor.position);
    let anchor_char = cursor.anchor.map(|a| motion::pos_to_char(buf, a));
    entry
        .matches
        .iter()
        .position(|(start, end_excl)| {
            let m_start_char = motion::pos_to_char(buf, *start);
            let m_end_char = motion::pos_to_char(buf, *end_excl);
            let m_last_char = m_end_char.saturating_sub(1);
            if m_start_char == m_last_char {
                anchor_char.is_none() && pos_char == m_start_char
            } else {
                anchor_char == Some(m_start_char) && pos_char == m_last_char
            }
        })
        .map(|i| (i as u32).saturating_add(1))
        .unwrap_or(0)
    }

/// Build one `viewport/lines_changed` notification per viewport owned by `client_id` that's
/// subscribed to `buffer_id`. Used to refresh highlights when a search is set or cleared.
fn collect_viewport_refresh(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let mut pushes = Vec::new();
    let buf = match s.buffers.get(&buffer_id) {
        Some(b) => b,
        None => return pushes,
    };
    let revision = buf.revision;
    let search_entry = s.searches.get(&(client_id, buffer_id));
    for vp in s.viewports.values() {
        if vp.client_id != client_id || vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        let line_count = buf.line_count();
        let new_first = vp.first_logical_line.min(line_count);
        let new_last_excl = vp.last_logical_line_exclusive.min(line_count).max(new_first);
        let window = render_window(
            buf,
            new_first,
            new_last_excl,
            vp.cols,
            vp.wrap,
            vp.continuation_marker_width,
            vp.tab_width,
            vp.rows,
            search_entry,
        );
        let params = ViewportLinesChangedParams {
            viewport_id: vp.id,
            revision,
            range: LogicalLineRange {
                start_logical_line: vp.first_logical_line,
                end_logical_line_exclusive: vp.last_logical_line_exclusive,
            },
            replacement_lines: window.lines,
            line_count,
            max_scroll_logical_line: window.max_scroll_logical_line,
        };
        pushes.push((sender, Notification {
            jsonrpc: JsonRpc,
            method: ViewportLinesChanged::NAME.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
        }));
    }
    pushes
}

/// After a cursor change for `(client_id, buffer_id)`, build a `search/state_changed`
/// notification with the recomputed `current_index` — but only when a search is active *and*
/// the index actually changed since the last push. The cursor counts as "on" a match only when
/// the selection's full range coincides with the match (both endpoints), so extending or
/// shrinking the selection drops the counter rather than leaving it stale.
fn collect_cursor_search_update(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<(mpsc::Sender<Notification>, Notification)> {
    let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();
    let buf = s.buffers.get(&buffer_id)?;
    let new_idx = {
        let entry = s.searches.get(&(client_id, buffer_id))?;
        match_index_for_cursor(buf, entry, &cursor)
    };
    let entry = s.searches.get_mut(&(client_id, buffer_id))?;
    if new_idx == entry.last_pushed_index {
        return None;
    }
    entry.last_pushed_index = new_idx;
    let summary = SearchSummary {
        buffer_id,
        total: entry.matches.len() as u32,
        truncated: entry.truncated,
        current_index: new_idx,
    };
    let session = s.clients.get(&client_id)?;
    Some((
        session.outbound.clone(),
        Notification {
            jsonrpc: JsonRpc,
            method: SearchStateChanged::NAME.into(),
            params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
        },
    ))
}

/// Build the `buffer/state` notification pushes for every client that has a viewport on this
/// buffer. Only used by the save handler — mutations bump the buffer's `revision` (which clients
/// already learn from `viewport/lines_changed`) and the client derives `dirty` as
/// `revision != saved_revision`, so this notification is only needed when `saved_revision`
/// itself changes.
fn collect_buffer_state_pushes(
    s: &ServerState,
    buffer_id: BufferId,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let Some(buf) = s.buffers.get(&buffer_id) else { return Vec::new() };
    let params = BufferStateParams {
        buffer_id,
        saved_revision: buf.saved_revision(),
        saved_at_unix_ms: buf.last_modified_unix_ms,
    };
    let json = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
    let mut clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
    for vp in s.viewports.values() {
        if vp.buffer_id == buffer_id {
            clients.insert(vp.client_id);
        }
    }
    clients
        .into_iter()
        .filter_map(|cid| {
            let session = s.clients.get(&cid)?;
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferState::NAME.into(),
                    params: json.clone(),
                },
            ))
        })
        .collect()
}

/// Recompute every active search on this buffer after a mutation. Returns the pushes (search
/// summary notifications) to be sent after dropping the lock. The line-level highlight refresh
/// happens via the existing `viewport/lines_changed` flow (since `render_window` reads the
/// freshly-recomputed entries).
fn refresh_searches_for_buffer(
    s: &mut ServerState,
    buffer_id: BufferId,
) -> Vec<(mpsc::Sender<Notification>, Notification)> {
    let mut pushes = Vec::new();
    if !s.buffers.contains_key(&buffer_id) {
        return pushes;
    }
    let keys: Vec<(ClientId, BufferId)> = s
        .searches
        .keys()
        .filter(|(_, b)| *b == buffer_id)
        .copied()
        .collect();
    for key in keys {
        let query = s.searches[&key].query.clone();
        let buf = &s.buffers[&buffer_id];
        let mut entry = match compute_search_entry(buf, &query) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        let summary = summary_for(buf, &entry, buffer_id, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        if let Some(sender) = s.clients.get(&key.0).map(|c| c.outbound.clone()) {
            pushes.push((sender, Notification {
                jsonrpc: JsonRpc,
                method: SearchStateChanged::NAME.into(),
                params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
            }));
        }
    }
    pushes
}

/// Convert a buffer-wide byte offset to a `(line, col_bytes)` position.
fn byte_to_logical(buf: &Buffer, byte_idx: usize) -> aether_protocol::LogicalPosition {
    let char_idx = buf.text.byte_to_char(byte_idx);
    let line_idx = buf.text.char_to_line(char_idx);
    let line_start_char = buf.text.line_to_char(line_idx);
    let char_offset = char_idx - line_start_char;
    let line_slice = buf.text.line(line_idx);
    let col_bytes = line_slice.char_to_byte(char_offset);
    aether_protocol::LogicalPosition {
        line: line_idx as u32,
        col: col_bytes as u32,
    }
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
    let new_cursor = CursorState { position: motion::char_to_pos(buf_mut, start_char), anchor: None, match_bracket: None };
    s.cursors.insert((client_id, params.buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(params.buffer_id);
    s.clear_tree_selection_history_for_buffer(params.buffer_id);
    s.clear_virtual_col_for_buffer(params.buffer_id);

    let search_summary_pushes = refresh_searches_for_buffer(&mut s, params.buffer_id);
    let new_line_count = s.buffers[&params.buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, params.buffer_id, new_line_count);
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
        let search = s.searches.get(&(vp.client_id, params.buffer_id));
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision, search)));
    }

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(BufferCutResult { text, revision, cursor: new_cursor })
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
    let pushes = {
        let s = state.lock().await;
        collect_buffer_state_pushes(&s, params.buffer_id)
    };
    let _ = saved_at_unix_ms; // saved_at is captured inside the helper via Buffer::last_modified.
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
    let search = s.searches.get(&(client_id, params.buffer_id));
    let buf = &s.buffers[&params.buffer_id];
    let window = render_window(buf, first, last_excl, params.cols, params.wrap, params.continuation_marker_width, params.tab_width, params.rows, search);

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
        continuation_marker_width: params.continuation_marker_width,
        tab_width: params.tab_width,
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
    };
    s.viewports.insert(viewport_id, viewport);
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
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
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line) =
        (vp.cols, vp.rows, vp.overscan_rows, vp.wrap, vp.continuation_marker_width, vp.tab_width, vp.buffer_id, vp.scroll_logical_line);

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let buf = &s.buffers[&buffer_id];
    let window = render_window(buf, first, last_excl, cols, wrap, marker_width, tab_width, rows, search);

    let vp = s.viewports.get_mut(&params.viewport_id).expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_set_wrap(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSetWrapParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.wrap = params.wrap;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line) =
        (vp.cols, vp.rows, vp.overscan_rows, vp.wrap, vp.continuation_marker_width, vp.tab_width, vp.buffer_id, vp.scroll_logical_line);

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let buf = &s.buffers[&buffer_id];
    let window = render_window(buf, first, last_excl, cols, wrap, marker_width, tab_width, rows, search);

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
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line) =
        (vp.cols, vp.rows, vp.overscan_rows, vp.wrap, vp.continuation_marker_width, vp.tab_width, vp.buffer_id, vp.scroll_logical_line);

    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = s.searches.get(&(client_id, buffer_id));
    let buf = &s.buffers[&buffer_id];
    let window = render_window(buf, first, last_excl, cols, wrap, marker_width, tab_width, rows, search);

    let vp = s.viewports.get_mut(&params.viewport_id).expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
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

/// Recompute every viewport's pushed range for this buffer from `pushed_range` against the new
/// line count. Call **before** building `viewport/lines_changed` notifications after any
/// mutation that may grow or shrink the buffer — otherwise a growth (e.g. undoing a join)
/// leaves the viewport's range clamped to the smaller post-mutation size and the freshly
/// restored lines never reach the client.
fn refresh_viewport_ranges_for_buffer(s: &mut ServerState, buffer_id: BufferId, line_count: u32) {
    for vp in s.viewports.values_mut() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let (first, last_excl) = pushed_range(vp.scroll_logical_line, vp.rows, vp.overscan_rows, line_count);
        vp.first_logical_line = first;
        vp.last_logical_line_exclusive = last_excl;
    }
}

/// Find the largest `scroll_logical_line` such that the buffer's last visual row sits at the
/// bottom of the viewport. Walks logical lines from the end backward, accumulating their visual
/// row counts under the current wrap settings until we have `viewport_rows` rows.
fn compute_max_scroll(
    buf: &Buffer,
    viewport_rows: u32,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
) -> u32 {
    let line_count = buf.line_count();
    if viewport_rows == 0 || line_count == 0 {
        return 0;
    }
    if matches!(wrap, aether_protocol::viewport::WrapMode::None) {
        return line_count.saturating_sub(viewport_rows);
    }
    let mut rows_remaining = viewport_rows;
    for line_idx in (0..line_count).rev() {
        let line_slice = buf.text.line(line_idx as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        let n = wrap::compute_rows(&text, cols, marker_width, tab_width).len() as u32;
        if n >= rows_remaining {
            return line_idx;
        }
        rows_remaining -= n;
    }
    0
}

fn render_window(
    buf: &Buffer,
    first: u32,
    last_excl: u32,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    viewport_rows: u32,
    search: Option<&SearchEntry>,
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
                    &syntax.injections,
                    source,
                    line_byte_start,
                    line_byte_end,
                )
            }
            _ => Vec::new(),
        };

        let mut render = wrap::render_line(&text, i, cols, wrap, marker_width, tab_width, highlights);
        if let Some(entry) = search {
            render.search_matches = matches_on_line(entry, i, text.len() as u32);
        }
        lines.push(render);
    }
    Window {
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
        line_count: buf.line_count(),
        max_scroll_logical_line: compute_max_scroll(buf, viewport_rows, cols, wrap, marker_width, tab_width),
        lines,
    }
}

/// Per-line byte ranges from `entry.matches` clipped to `[0, line_len)` for `line_idx`. Matches
/// that span multiple lines contribute one range per line they touch.
fn matches_on_line(entry: &SearchEntry, line_idx: u32, line_len: u32) -> Vec<SearchMatchRange> {
    let mut out = Vec::new();
    for (start, end_excl) in &entry.matches {
        if line_idx < start.line || line_idx > end_excl.line {
            continue;
        }
        let s = if line_idx == start.line { start.col } else { 0 };
        let e = if line_idx == end_excl.line { end_excl.col } else { line_len };
        let s = s.min(line_len);
        let e = e.min(line_len);
        if s < e {
            out.push(SearchMatchRange { start: s, end: e });
        }
    }
    out
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

    // Visual motions need viewport state (wrap mode + width). Look it up and dispatch to the
    // dedicated resolver; everything else goes through `resolve_motion` which only needs the
    // buffer.
    let virtual_col_in = s.virtual_col.get(&key).copied();
    // `Some(col)` → set virtual col to `col`; `None` → clear it. Only `VisualLine` preserves it.
    let mut new_virtual_col: Option<u32> = None;
    let new_pos = match &params.motion {
        Motion::VisualLine { viewport_id, direction, count } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            let (pos, target_vcol) = motion::resolve_visual_line(
                buf,
                vp.wrap,
                vp.cols,
                vp.continuation_marker_width,
                vp.tab_width,
                current.position,
                virtual_col_in,
                *direction,
                *count,
            );
            new_virtual_col = Some(target_vcol);
            pos
        }
        Motion::VisualLineStart { viewport_id } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            motion::resolve_visual_line_start(buf, vp.wrap, vp.cols, vp.continuation_marker_width, vp.tab_width, current.position)
        }
        Motion::VisualLineEnd { viewport_id } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            motion::resolve_visual_line_end(buf, vp.wrap, vp.cols, vp.continuation_marker_width, vp.tab_width, current.position)
        }
        Motion::LogicalLine { direction, count, preserve_col } => {
            // LogicalLine doesn't reference a viewport, but it does preserve virtual column,
            // which is in display cells — so it needs `tab_width` to be right for tab-bearing
            // lines. Borrow it from any of this client's viewports on this buffer.
            let tab_width = s
                .viewports
                .values()
                .find(|v| v.buffer_id == params.buffer_id && v.client_id == client_id)
                .map(|v| v.tab_width)
                .unwrap_or(4);
            let (pos, target_vcol) = motion::resolve_logical_line(
                buf,
                current.position,
                virtual_col_in,
                *direction,
                *count,
                *preserve_col,
                tab_width,
            );
            new_virtual_col = target_vcol;
            pos
        }
        _ => motion::resolve_motion(buf, current.position, &params.motion),
    };
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

    let new_state = CursorState { position: new_pos, anchor: new_anchor, match_bracket: None };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    match new_virtual_col {
        Some(col) => {
            s.virtual_col.insert(key, col);
        }
        None => {
            s.virtual_col.remove(&key);
        }
    }
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
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

    // Advance the relevant edge only when the selection is already snapped to whole lines;
    // otherwise snap it without advancing. The no-anchor case is treated as a degenerate
    // single-point selection, which is never whole-line (on a non-empty line), so the first
    // press always picks the current line — repeats then walk via the anchor.
    let bottom_len = motion::line_byte_len_excl_newline(buf, bottom_edge.line);
    let already_whole =
        current.anchor.is_some() && top_edge.col == 0 && bottom_edge.col >= bottom_len;
    let new_top = if already_whole && params.direction == Direction::Backward {
        top_edge.line.saturating_sub(1)
    } else {
        top_edge.line
    };
    let new_bottom = if already_whole && params.direction == Direction::Forward {
        bottom_edge.line.saturating_add(1)
    } else {
        bottom_edge.line
    };
    let (top_line, bottom_line) = match (params.extend && current.anchor.is_some(), params.direction) {
        (true, _) => (new_top, new_bottom),
        (false, Direction::Forward) => (new_bottom, new_bottom),
        (false, Direction::Backward) => (new_top, new_top),
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
    let new_state = CursorState { position: cursor_pos, anchor, match_bracket: None };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
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
        Some(a) => CursorState { position: a, anchor: Some(current.position), match_bracket: None },
        None => current,
    };
    s.cursors.insert(key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
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
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let position = motion::clamp_position(buf, params.position);
    let anchor = params.anchor.map(|a| motion::clamp_position(buf, a));
    let anchor = match anchor {
        Some(a) if a == position => None,
        x => x,
    };
    let result = CursorState { position, anchor, match_bracket: None };
    s.cursors.insert(key, result);
    s.record_motion(key, current, result);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, params.buffer_id, result);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

/// Rewind one step on this client's per-buffer motion history. Independent of `input/undo`.
pub async fn cursor_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.undo.is_empty() {
        return Ok(CursorUndoResult { applied: false, cursor: current });
    }
    let prev = history.undo.pop_back().expect("just checked non-empty");
    history.redo.push(current);
    while history.redo.len() > MOTION_HISTORY_CAP {
        history.redo.remove(0);
    }

    s.cursors.insert(key, prev);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let prev = wrap_for_response(&s, params.buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult { applied: true, cursor: prev })
}

pub async fn cursor_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.redo.is_empty() {
        return Ok(CursorUndoResult { applied: false, cursor: current });
    }
    let next = history.redo.pop().expect("just checked non-empty");
    history.undo.push_back(current);
    while history.undo.len() > MOTION_HISTORY_CAP {
        history.undo.pop_front();
    }

    s.cursors.insert(key, next);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let next = wrap_for_response(&s, params.buffer_id, next);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult { applied: true, cursor: next })
}

// ---- cursor/expand and cursor/contract ---------------------------------------------------------

pub async fn cursor_expand(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let Some(syntax) = buf.syntax.as_ref() else {
        return Ok(current);
    };

    // Compute the current selection's byte range. For collapsed cursors, treat as the single
    // char under the cursor (one-byte minimum so descendant_for_byte_range can find it).
    let (sel_start_char, sel_end_char_excl) = current_selection_char_range(buf, &current);
    let total_bytes = buf.text.len_bytes();
    let start_byte = buf.text.char_to_byte(sel_start_char).min(total_bytes);
    let end_byte_excl = buf.text.char_to_byte(sel_end_char_excl).min(total_bytes);

    // Smallest descendant containing the byte range, then walk up while the node exactly equals
    // our selection — that gives the smallest *strictly larger* enclosing node.
    let root = syntax.tree.root_node();
    let mut node = root.descendant_for_byte_range(start_byte, end_byte_excl).unwrap_or(root);
    while node.start_byte() == start_byte && node.end_byte() == end_byte_excl {
        match node.parent() {
            Some(p) => node = p,
            None => return Ok(current), // already at the root
        }
    }

    let new_start_char = buf.text.byte_to_char(node.start_byte());
    let new_end_char_excl = buf.text.byte_to_char(node.end_byte()).max(new_start_char + 1);
    let new_last_char = new_end_char_excl.saturating_sub(1).max(new_start_char);
    let anchor = motion::char_to_pos(buf, new_start_char);
    let position = motion::char_to_pos(buf, new_last_char);
    let new_cursor = if anchor == position {
        CursorState { position, anchor: None, match_bracket: None }
    } else {
        CursorState { position, anchor: Some(anchor), match_bracket: None }
    };

    s.cursors.insert(key, new_cursor);
    s.record_motion(key, current, new_cursor);
    s.virtual_col.remove(&key);
    s.tree_selection_history.entry(key).or_default().push(current);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let new_cursor = wrap_for_response(&s, params.buffer_id, new_cursor);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(new_cursor)
}

pub async fn cursor_contract(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorBufferOnlyParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let prev = s
        .tree_selection_history
        .get_mut(&key)
        .and_then(|stack| stack.pop());
    let Some(prev) = prev else {
        // Nothing to contract back to.
        let cur = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(wrap_for_response(&s, params.buffer_id, cur));
    };
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    s.cursors.insert(key, prev);
    s.record_motion(key, current, prev);
    s.virtual_col.remove(&key);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let prev = wrap_for_response(&s, params.buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(prev)
}

/// Char range `[start, end_excl)` covered by the cursor's current selection. Collapsed cursors
/// (no anchor) yield a 1-char range so byte conversion produces a non-empty span.
fn current_selection_char_range(buf: &Buffer, cursor: &CursorState) -> (usize, usize) {
    let (lo_pos, hi_pos) = match cursor.anchor {
        Some(a) => motion::ordered(cursor.position, a),
        None => (cursor.position, cursor.position),
    };
    let total = buf.text.len_chars();
    let lo = motion::pos_to_char(buf, lo_pos).min(total);
    let hi_inclusive = motion::pos_to_char(buf, hi_pos).min(total);
    (lo, (hi_inclusive + 1).min(total).max(lo + 1).min(total.max(lo)))
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

pub async fn input_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Indent).await
}

pub async fn input_newline_and_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let indent = {
        let s = state.lock().await;
        let buf = s
            .buffers
            .get(&params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s.cursors.get(&(client_id, params.buffer_id)).copied().unwrap_or_default();
        compute_smart_indent(buf, cursor.position)
    };
    let mut text = String::with_capacity(indent.len() + 1);
    text.push('\n');
    text.push_str(&indent);
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith { text, select_pasted: false },
    )
    .await
}

/// Choose the indent to emit after `\n`. When the buffer's language has an `indents.scm`
/// query (vendored from Helix), runs the tree-sitter indent engine and multiplies its level
/// count by `INDENT_UNIT`. Otherwise falls back to copying the previous non-empty line's
/// leading whitespace.
///
/// The engine alone misses the very common "user just typed `fn foo() {` and pressed Enter"
/// case: the parser hasn't seen a closing brace yet, so no `block` node exists and no
/// `@indent` fires. We patch this with a small heuristic floor — `prev_line_levels +
/// opener_bonus` — taken as `max` with the engine's answer. For complete code the engine
/// already produces the right number, so the heuristic is a no-op; for incomplete code it
/// recovers the level the parser couldn't.
fn compute_smart_indent(buf: &Buffer, cursor_pos: LogicalPosition) -> String {
    let unit = buf.indent_style.unit();

    let line_idx = cursor_pos.line as usize;
    if line_idx >= buf.text.len_lines() {
        return String::new();
    }

    let Some(syntax) = buf.syntax.as_ref() else {
        return previous_line_indent(buf, line_idx);
    };
    let Some(iq) = syntax.config.indent_query.as_ref() else {
        return previous_line_indent(buf, line_idx);
    };

    let line_slice = buf.text.line(line_idx);
    let line_byte_len = {
        let n = line_slice.len_bytes();
        if n > 0 && line_slice.byte(n - 1) == b'\n' { n - 1 } else { n }
    };
    let col = (cursor_pos.col as usize).min(line_byte_len);
    let line_start_char = buf.text.line_to_char(line_idx);
    let line_start_byte = buf.text.char_to_byte(line_start_char);
    let cursor_byte = line_start_byte + col;
    let source: String = buf.text.chunks().collect();

    let target_levels = crate::indent::compute_indent_levels(
        iq,
        &syntax.tree,
        source.as_bytes(),
        cursor_byte,
        line_idx + 1,
    );

    // Engine-only is enough when it returned anything non-zero — the parse covered the
    // construct and the @indent / @outdent rules already account for it. We only step in
    // with the opener heuristic when the engine reported zero levels *and* the user just
    // typed a code-context opener — that's the "incomplete parse" signature.
    if target_levels > 0 {
        return unit.repeat(target_levels as usize);
    }
    let line_text: String = line_slice.chunks().collect();
    let line_content = line_text.strip_suffix('\n').unwrap_or(&line_text);
    let prefix = &line_content[..col];
    let trimmed = prefix.trim_end_matches(|c: char| c == ' ' || c == '\t');
    let mut opener_bonus = match trimmed.as_bytes().last() {
        Some(b'{') | Some(b'(') | Some(b'[') => 1,
        _ => 0,
    };
    if opener_bonus > 0 {
        let opener_byte = line_start_byte + trimmed.len() - 1;
        let node = syntax
            .tree
            .root_node()
            .descendant_for_byte_range(opener_byte, opener_byte + 1);
        if let Some(n) = node {
            let kind = n.kind();
            if kind.contains("string") || kind.contains("comment") || kind.contains("char") {
                opener_bonus = 0;
            }
        }
    }
    unit.repeat(opener_bonus as usize)
}

/// Fallback indent for buffers without an indent query: copy the leading whitespace of the
/// nearest preceding non-blank line. If no such line exists, return empty.
fn previous_line_indent(buf: &Buffer, line_idx: usize) -> String {
    let mut i = line_idx;
    loop {
        let line: String = buf.text.line(i).chunks().collect();
        let content = line.strip_suffix('\n').unwrap_or(&line);
        if !content.trim().is_empty() {
            return content.chars().take_while(|c| c.is_whitespace()).collect();
        }
        if i == 0 {
            return String::new();
        }
        i -= 1;
    }
}

pub async fn input_toggle_comment(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    apply_toggle_comment(state, ctx, params.buffer_id).await
}

/// Toggle comment status on the cursor/selection.
///
/// Decision tree (closest to what users expect from `Ctrl-/` in modern editors):
///   1. If the language has a *line* token and every non-blank line in the affected range
///      already starts with it → strip it.
///   2. Else if the language has *block* tokens and the cursor sits inside a block-comment
///      node (via tree-sitter) or the selection exactly wraps a `start…end` span → strip
///      them.
///   3. Else if the selection is *partial-line* and the language has block tokens → wrap.
///   4. Else if the language has a line token → add the line prefix on each line, aligned to
///      the smallest indent so prefixes line up.
///   5. Else if the language has block tokens → wrap (for languages with no line form).
///   6. Else → no-op.
async fn apply_toggle_comment(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();

    let (line_tok, block_tok) = buf
        .syntax
        .as_ref()
        .map(|sy| (sy.config.line_comment, sy.config.block_comment))
        .unwrap_or((None, None));
    if line_tok.is_none() && block_tok.is_none() {
        let revision = buf.revision;
        let response = wrap_for_response(&s, buffer_id, cursor);
        return Ok(EditResult { revision, cursor: response });
    }

    // Selection / line range.
    let (a, b) = match cursor.anchor {
        Some(anchor) => {
            let (start, end) = motion::ordered(cursor.position, anchor);
            (start.line, end.line)
        }
        None => (cursor.position.line, cursor.position.line),
    };
    let is_partial = is_partial_line_selection(buf, &cursor);

    // Phase 1: decide the action.
    let line_strings: Vec<String> =
        (a..=b).map(|i| buf.text.line(i as usize).chunks().collect()).collect();
    let line_classify = classify_line_range(&line_strings, line_tok);

    let sel_block_unwrap = block_tok.and_then(|(open, close)| {
        // Primary detector: tree-sitter `comment` ancestor containing the cursor. Handles the
        // natural "wrap, then re-toggle to unwrap" gesture where the selection sits on the
        // inner content rather than around the wrappers.
        if let Some(syntax) = buf.syntax.as_ref() {
            let cursor_byte =
                buf.text.char_to_byte(motion::pos_to_char(buf, cursor.position));
            let source: String = buf.text.chunks().collect();
            if let Some((s, e)) = find_enclosing_block_comment(
                &syntax.tree,
                source.as_bytes(),
                cursor_byte,
                open,
                close,
            ) {
                let span = source[s..e].to_string();
                return Some((s, e, span, open, close));
            }
        }
        // Fallback: the selection's text *exactly* equals a wrapped span. Catches incomplete
        // parses where tree-sitter doesn't recognise the comment yet (e.g. the user just
        // typed an opener without a closer).
        let (start_pos, end_pos) = ordered_selection_or_cursor_line(&cursor);
        let start_char = motion::pos_to_char(buf, start_pos);
        let end_char_excl =
            motion::pos_to_char(buf, end_pos).saturating_add(1).min(buf.text.len_chars());
        let span: String = buf.text.slice(start_char..end_char_excl).chunks().collect();
        if span.starts_with(open) && span.ends_with(close) && span.len() >= open.len() + close.len() {
            Some((start_char, end_char_excl, span, open, close))
        } else {
            None
        }
    });

    enum Plan {
        Noop,
        LineUncomment { prefix: &'static str },
        LineComment { prefix: &'static str, min_indent: usize },
        BlockUnwrap { start_char: usize, end_char_excl: usize, span: String, open: &'static str, close: &'static str },
        BlockWrap { start_char: usize, end_char_excl: usize, open: &'static str, close: &'static str },
    }

    let plan = if let (Some(prefix), Some(c)) = (line_tok, &line_classify) {
        if c.all_commented {
            Plan::LineUncomment { prefix }
        } else if let Some((sc, ec, span, open, close)) = sel_block_unwrap {
            Plan::BlockUnwrap { start_char: sc, end_char_excl: ec, span, open, close }
        } else if is_partial && block_tok.is_some() {
            let (start_pos, end_pos) = ordered_selection_or_cursor_line(&cursor);
            let sc = motion::pos_to_char(buf, start_pos);
            let ec = motion::pos_to_char(buf, end_pos).saturating_add(1).min(buf.text.len_chars());
            let (open, close) = block_tok.unwrap();
            Plan::BlockWrap { start_char: sc, end_char_excl: ec, open, close }
        } else if c.any_nonblank {
            Plan::LineComment { prefix, min_indent: c.min_indent }
        } else {
            Plan::Noop
        }
    } else if let Some((sc, ec, span, open, close)) = sel_block_unwrap {
        Plan::BlockUnwrap { start_char: sc, end_char_excl: ec, span, open, close }
    } else if let Some((open, close)) = block_tok {
        // No line tokens at all (markdown, html, css): everything routes to block.
        let endpoints = if cursor.anchor.is_some() {
            Some(ordered_selection_or_cursor_line(&cursor))
        } else {
            // Cursor-only: wrap the current line's content. Skip empty lines entirely —
            // otherwise the wrap would swallow the line's `\n` and merge it with the next.
            current_line_content_endpoints(buf, cursor.position.line)
        };
        match endpoints {
            None => Plan::Noop,
            Some((start_pos, end_pos)) => {
                let sc = motion::pos_to_char(buf, start_pos);
                let ec = motion::pos_to_char(buf, end_pos).saturating_add(1).min(buf.text.len_chars());
                if sc == ec {
                    Plan::Noop
                } else {
                    Plan::BlockWrap { start_char: sc, end_char_excl: ec, open, close }
                }
            }
        }
    } else {
        Plan::Noop
    };

    // Phase 2: materialize the edit. Each variant produces (edit_start_char, edit_end_char,
    // replacement_text, new_cursor).
    let edit: Option<(usize, usize, String, CursorState, u32, u32)> = match plan {
        Plan::Noop => None,
        Plan::LineUncomment { prefix } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) =
                build_line_uncomment(&line_strings, a, prefix);
            let nc = shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::LineComment { prefix, min_indent } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) =
                build_line_comment(&line_strings, a, prefix, min_indent);
            let nc = shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::BlockUnwrap { start_char, end_char_excl, span, open, close } => {
            // Strip `open` + optional inner space at the front, optional inner space + `close`
            // at the back. Replace the wrapped span with the inner content; re-select that
            // content.
            let inner_start = open.len();
            let inner_end = span.len() - close.len();
            let mut inner = &span[inner_start..inner_end];
            if inner.starts_with(' ') {
                inner = &inner[1..];
            }
            if inner.ends_with(' ') {
                inner = &inner[..inner.len() - 1];
            }
            let new_text = inner.to_string();
            let start_pos = motion::char_to_pos(buf, start_char);
            // Compute the post-edit position of inner's last byte directly. Walk to the last
            // byte and ask "how many newlines came strictly before it, and where was the last
            // one?". When inner *ends* with `\n` the cursor lands on that `\n` itself (which
            // belongs to the previous line) — naively splitting on `\n` would wrongly put the
            // cursor at col 0 of an empty trailing line.
            let new_position = if inner.is_empty() {
                start_pos
            } else {
                let last_byte_idx = inner.len() - 1;
                let prefix = &inner[..last_byte_idx];
                let newlines_before = prefix.matches('\n').count() as u32;
                match prefix.rfind('\n') {
                    Some(last_nl) => aether_protocol::LogicalPosition {
                        line: start_pos.line + newlines_before,
                        col: (last_byte_idx - last_nl - 1) as u32,
                    },
                    None => aether_protocol::LogicalPosition {
                        line: start_pos.line,
                        col: start_pos.col + last_byte_idx as u32,
                    },
                }
            };
            let nc = if start_pos == new_position {
                CursorState { position: new_position, anchor: None, match_bracket: None }
            } else {
                CursorState { position: new_position, anchor: Some(start_pos), match_bracket: None }
            };
            let last_line = motion::char_to_pos(buf, end_char_excl.saturating_sub(1)).line;
            Some((start_char, end_char_excl, new_text, nc, a.min(last_line), b.max(last_line)))
        }
        Plan::BlockWrap { start_char, end_char_excl, open, close } => {
            let selected: String =
                buf.text.slice(start_char..end_char_excl).chunks().collect();
            let new_text = format!("{open} {selected} {close}");
            // Compute new selection endpoints in (line, col) directly — `char_to_pos` on the
            // pre-edit buffer is wrong for post-edit char indices once the wrap spans lines.
            // Discriminate by whether the *selected text* contains a newline, not by whether
            // start_pos.line == end_pos.line: a selection ending exactly on the `\n` of its
            // line counts as single-line in (line, col) terms but produces multi-line output.
            let start_pos = motion::char_to_pos(buf, start_char);
            let end_pos = motion::char_to_pos(buf, end_char_excl.saturating_sub(1));
            let newlines = selected.matches('\n').count() as u32;
            let new_position = if newlines == 0 {
                aether_protocol::LogicalPosition {
                    line: end_pos.line,
                    col: end_pos.col + open.len() as u32 + close.len() as u32 + 2,
                }
            } else {
                // The wrap's last line consists of whatever followed the last newline in the
                // selected text, plus the inserted ` close`.
                let last_nl_byte = selected.rfind('\n').unwrap();
                let bytes_after_last_nl = (selected.len() - last_nl_byte - 1) as u32;
                let last_line_bytes = bytes_after_last_nl + 1 + close.len() as u32;
                aether_protocol::LogicalPosition {
                    line: start_pos.line + newlines,
                    col: last_line_bytes.saturating_sub(1),
                }
            };
            let nc = if start_pos == new_position {
                CursorState { position: new_position, anchor: None, match_bracket: None }
            } else {
                CursorState { position: new_position, anchor: Some(start_pos), match_bracket: None }
            };
            let last_touched_line = start_pos.line + newlines;
            Some((start_char, end_char_excl, new_text, nc, a.min(start_pos.line), b.max(last_touched_line)))
        }
    };

    let Some((start_char, end_char, new_text, new_cursor, edit_first, edit_last_incl)) = edit
    else {
        let revision = buf.revision;
        let response = wrap_for_response(&s, buffer_id, cursor);
        return Ok(EditResult { revision, cursor: response });
    };

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| if *bid == buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    let revision = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        buf_mut.apply_edit(start_char, end_char, &new_text, EditKindTag::Text, cursors_before)
    };
    // Re-clamp the new cursor against the post-edit buffer (positions computed above used the
    // pre-edit buffer; if the edit shortened lines, clamp_position keeps them legal).
    let new_cursor = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let mut c = new_cursor;
        c.position = motion::clamp_position(buf_mut, c.position);
        c.anchor = c.anchor.map(|a| motion::clamp_position(buf_mut, a));
        if c.anchor == Some(c.position) {
            c.anchor = None;
        }
        c
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_last_excl = edit_last_incl + 1;
    let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !ranges_overlap(
            vp.first_logical_line,
            vp.last_logical_line_exclusive,
            edit_first,
            edit_last_excl,
        ) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision, search)));
    }

    let new_cursor = wrap_for_response(&s, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult { revision, cursor: new_cursor })
}

/// Walk the cursor's ancestors looking for a tree-sitter node whose kind contains "comment"
/// and whose text starts with `open` and ends with `close`. Returns the node's byte range.
/// We match by kind-substring rather than exact name because grammars use different names
/// (`comment`, `block_comment`, `line_comment`, …) and the open/close suffix check validates
/// it's a block-style comment regardless.
fn find_enclosing_block_comment(
    tree: &tree_sitter::Tree,
    source: &[u8],
    byte: usize,
    open: &str,
    close: &str,
) -> Option<(usize, usize)> {
    let root = tree.root_node();
    let here = root.descendant_for_byte_range(byte, byte + 1)?;
    let mut node = Some(here);
    while let Some(n) = node {
        if n.kind().contains("comment") {
            let s = n.start_byte();
            let e = n.end_byte();
            let span = source.get(s..e)?;
            if span.starts_with(open.as_bytes()) && span.ends_with(close.as_bytes())
                && e - s >= open.len() + close.len()
            {
                return Some((s, e));
            }
        }
        node = n.parent();
    }
    None
}

struct LineClassify {
    any_nonblank: bool,
    all_commented: bool,
    min_indent: usize,
}

fn classify_line_range(lines: &[String], prefix: Option<&str>) -> Option<LineClassify> {
    let prefix = prefix?;
    let mut all_commented = true;
    let mut min_indent: Option<usize> = None;
    let mut any_nonblank = false;
    for line in lines {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let rest = &content[leading..];
        if rest.is_empty() {
            continue;
        }
        any_nonblank = true;
        min_indent = Some(min_indent.map_or(leading, |m| m.min(leading)));
        if !rest.starts_with(prefix) {
            all_commented = false;
        }
    }
    Some(LineClassify { any_nonblank, all_commented, min_indent: min_indent.unwrap_or(0) })
}

/// `true` when the selection doesn't cover a contiguous run of *complete* lines (lower
/// endpoint at col 0 of its line, upper endpoint at the line end of its line). Cursor-only
/// counts as non-partial. Partial selections — single mid-line, or multi-line where one of
/// the boundary lines isn't fully covered — route to block-comment when the language has it.
fn is_partial_line_selection(buf: &Buffer, cursor: &CursorState) -> bool {
    let Some(anchor) = cursor.anchor else { return false };
    let (lo, hi) = motion::ordered(cursor.position, anchor);
    let line_end_hi = motion::line_byte_len_excl_newline(buf, hi.line);
    let lo_at_start = lo.col == 0;
    // Accept either exclusive end (col == line_end) or inclusive last byte (col + 1 ==
    // line_end). Aether's selections are inclusive on both endpoints, so the natural
    // "whole-line" form has the cursor on the last byte.
    let hi_at_end = hi.col == line_end_hi || hi.col + 1 == line_end_hi;
    !(lo_at_start && hi_at_end)
}

/// `(start_pos, end_pos)` of the selection, both inclusive, ordered. Cursor-only collapses to
/// the single-char range at the cursor.
fn ordered_selection_or_cursor_line(cursor: &CursorState) -> (aether_protocol::LogicalPosition, aether_protocol::LogicalPosition) {
    match cursor.anchor {
        Some(anchor) => motion::ordered(cursor.position, anchor),
        None => (cursor.position, cursor.position),
    }
}

/// Endpoints `(line_start, line_end_inclusive)` for the content of `line_idx`, excluding the
/// trailing newline. Used to give "wrap the current line" a sensible char range when no
/// selection exists in a block-only language. Returns `None` for empty lines so the caller
/// can skip — otherwise a wrap on an empty line would replace its lone `\n` and merge the
/// line with the next.
fn current_line_content_endpoints(buf: &Buffer, line_idx: u32) -> Option<(aether_protocol::LogicalPosition, aether_protocol::LogicalPosition)> {
    let end_col = motion::line_byte_len_excl_newline(buf, line_idx);
    if end_col == 0 {
        return None;
    }
    Some((
        aether_protocol::LogicalPosition { line: line_idx, col: 0 },
        aether_protocol::LogicalPosition { line: line_idx, col: end_col - 1 },
    ))
}

fn line_edit_char_range(buf: &Buffer, a: u32, b: u32) -> (usize, usize) {
    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let start_char = buf.text.line_to_char(a as usize);
    let end_char = if (b + 1) < len_lines {
        buf.text.line_to_char((b + 1) as usize)
    } else {
        len_chars
    };
    (start_char, end_char)
}

fn build_line_comment(
    lines: &[String],
    a: u32,
    prefix: &str,
    min_indent: usize,
) -> (String, HashMap<u32, i32>, HashMap<u32, usize>) {
    let prefix_with_space = format!("{prefix} ");
    let mut text = String::new();
    let mut shifts = HashMap::new();
    let mut insert_cols = HashMap::new();
    for (offset, line) in lines.iter().enumerate() {
        let line_idx = a + offset as u32;
        let (content, newline) = match line.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line.as_str(), ""),
        };
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let is_blank = content[leading..].is_empty();
        if is_blank {
            text.push_str(content);
            text.push_str(newline);
            shifts.insert(line_idx, 0);
            insert_cols.insert(line_idx, leading);
            continue;
        }
        let (before, after) = content.split_at(min_indent);
        text.push_str(before);
        text.push_str(&prefix_with_space);
        text.push_str(after);
        text.push_str(newline);
        shifts.insert(line_idx, prefix_with_space.len() as i32);
        insert_cols.insert(line_idx, min_indent);
    }
    (text, shifts, insert_cols)
}

fn build_line_uncomment(
    lines: &[String],
    a: u32,
    prefix: &str,
) -> (String, HashMap<u32, i32>, HashMap<u32, usize>) {
    let mut text = String::new();
    let mut shifts = HashMap::new();
    let mut insert_cols = HashMap::new();
    for (offset, line) in lines.iter().enumerate() {
        let line_idx = a + offset as u32;
        let (content, newline) = match line.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line.as_str(), ""),
        };
        let leading: usize = content
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();
        let rest = &content[leading..];
        if rest.is_empty() {
            text.push_str(content);
            text.push_str(newline);
            shifts.insert(line_idx, 0);
            insert_cols.insert(line_idx, leading);
            continue;
        }
        // We've already classified the range as `all_commented` so this strip is safe.
        let after_prefix = rest.strip_prefix(prefix).unwrap_or(rest);
        let (stripped_tail, removed) = if let Some(after_space) = after_prefix.strip_prefix(' ') {
            (after_space, prefix.len() + 1)
        } else {
            (after_prefix, prefix.len())
        };
        text.push_str(&content[..leading]);
        text.push_str(stripped_tail);
        text.push_str(newline);
        shifts.insert(line_idx, -(removed as i32));
        insert_cols.insert(line_idx, leading);
    }
    (text, shifts, insert_cols)
}

fn shift_cursor_by_line_map(
    cursor: CursorState,
    a: u32,
    b: u32,
    shifts: &HashMap<u32, i32>,
    insert_cols: &HashMap<u32, usize>,
) -> CursorState {
    // When a selection exists, treat its endpoints asymmetrically so the selection *extends*
    // to cover any prefix we just added (rather than sliding with the content and leaving the
    // new prefix outside the selection). The lower endpoint stays put when it sits exactly at
    // the insert column; the upper endpoint shifts forward to follow the content.
    let lower = cursor.anchor.map(|anch| motion::ordered(cursor.position, anch).0);

    let shift_pos = |p: aether_protocol::LogicalPosition, is_lower_endpoint: bool| {
        if p.line < a || p.line > b {
            return p;
        }
        let shift = shifts.get(&p.line).copied().unwrap_or(0);
        let insert_col = insert_cols.get(&p.line).copied().unwrap_or(0) as u32;
        if p.col < insert_col {
            return p;
        }
        let col = if shift >= 0 {
            // The endpoint that anchors the selection's *start* stays at insert_col so the
            // selection grows; everything else (including cursor-only) shifts forward.
            if is_lower_endpoint && p.col == insert_col {
                p.col
            } else {
                p.col.saturating_add(shift as u32)
            }
        } else {
            let removed = (-shift) as u32;
            let prefix_end = insert_col + removed;
            if p.col >= prefix_end { p.col - removed } else { insert_col }
        };
        aether_protocol::LogicalPosition { line: p.line, col }
    };

    // Don't clamp here; positions are post-edit, and the post-edit clamp at the call site
    // handles legality. Clamping against the pre-edit buffer would clip to shorter lines.
    let position_is_lower = lower == Some(cursor.position);
    let position = shift_pos(cursor.position, position_is_lower);
    let anchor = cursor.anchor.map(|anch| {
        let is_lower = lower == Some(anch);
        shift_pos(anch, is_lower)
    });
    let anchor = match anchor {
        Some(a) if a == position => None,
        x => x,
    };
    CursorState { position, anchor, match_bracket: None }
}

pub async fn input_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Dedent).await
}

#[derive(Clone, Copy)]
enum IndentKind {
    Indent,
    Dedent,
}

/// Per-buffer-style soft indent. Selection's line range gets the prefix added (or stripped, on
/// dedent). Cursor and anchor are shifted by the per-line delta — on indent that's always
/// +unit.len(); on dedent it's 0/-1/-unit.len() depending on what was actually there to strip.
async fn apply_indent_or_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    kind: IndentKind,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let indent = buf.indent_style.unit();
    let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();

    let (a, b) = match cursor.anchor {
        Some(anchor) => {
            let (start, end) = motion::ordered(cursor.position, anchor);
            (start.line, end.line)
        }
        None => (cursor.position.line, cursor.position.line),
    };

    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let start_char = buf.text.line_to_char(a as usize);
    let end_char = if (b + 1) < len_lines {
        buf.text.line_to_char((b + 1) as usize)
    } else {
        len_chars
    };

    // Build the replacement text and a per-line column shift map.
    let mut new_text = String::new();
    let mut shifts: HashMap<u32, i32> = HashMap::new();
    let mut any_changed = false;
    for line_idx in a..=b {
        let line_str: String = buf.text.line(line_idx as usize).chunks().collect();
        let (content, newline) = match line_str.strip_suffix('\n') {
            Some(s) => (s, "\n"),
            None => (line_str.as_str(), ""),
        };
        let (modified, shift): (String, i32) = match kind {
            IndentKind::Indent => (format!("{indent}{content}"), indent.len() as i32),
            IndentKind::Dedent => {
                if let Some(s) = content.strip_prefix(indent.as_ref()) {
                    (s.to_string(), -(indent.len() as i32))
                } else if let Some(s) = content.strip_prefix(' ') {
                    (s.to_string(), -1)
                } else {
                    (content.to_string(), 0)
                }
            }
        };
        if shift != 0 {
            any_changed = true;
        }
        shifts.insert(line_idx, shift);
        new_text.push_str(&modified);
        new_text.push_str(newline);
    }

    if !any_changed {
        return Ok(EditResult {
            revision: buf.revision,
            cursor,
        });
    }

    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| if *bid == buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    let (revision, new_cursor) = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf_mut.apply_edit(
            start_char,
            end_char,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        );

        let shift_pos = |p: aether_protocol::LogicalPosition| {
            let shift = shifts.get(&p.line).copied().unwrap_or(0);
            let col = if shift >= 0 {
                p.col.saturating_add(shift as u32)
            } else {
                p.col.saturating_sub((-shift) as u32)
            };
            aether_protocol::LogicalPosition { line: p.line, col }
        };
        let new_cursor = CursorState {
            position: motion::clamp_position(buf_mut, shift_pos(cursor.position)),
            anchor: cursor.anchor.map(|a| motion::clamp_position(buf_mut, shift_pos(a))),
            match_bracket: None,
        };
        let new_cursor = match new_cursor.anchor {
            Some(a) if a == new_cursor.position => CursorState {
                position: new_cursor.position,
                anchor: None,
                match_bracket: None,
            },
            _ => new_cursor,
        };
        (revision, new_cursor)
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_first = a;
    let edit_last_excl = b + 1;
    let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !ranges_overlap(
            vp.first_logical_line,
            vp.last_logical_line_exclusive,
            edit_first,
            edit_last_excl,
        ) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision, search)));
    }

    let new_cursor = wrap_for_response(&s, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult { revision, cursor: new_cursor })
}

pub async fn input_move_lines(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputMoveLinesParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let buffer_id = params.buffer_id;

    // Phase 1: read state and compute the edit while holding the lock.
    let mut s = state.lock().await;
    let buf = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s.cursors.get(&(client_id, buffer_id)).copied().unwrap_or_default();

    // Selection's line range: the lines the user wants to move.
    let (a, b) = match cursor.anchor {
        Some(anchor) => {
            let (start_pos, end_pos) = motion::ordered(cursor.position, anchor);
            (start_pos.line, end_pos.line)
        }
        None => (cursor.position.line, cursor.position.line),
    };

    // The "last real line" — ropey counts a trailing empty line after a final newline that's not
    // user-visible; treat it as out-of-bounds for move purposes.
    let line_count = buf.line_count();
    let len_bytes = buf.text.len_bytes();
    let trailing_newline = len_bytes > 0 && buf.text.byte(len_bytes - 1) == b'\n';
    let last_real_line = if len_bytes == 0 {
        0
    } else if trailing_newline {
        line_count.saturating_sub(2)
    } else {
        line_count.saturating_sub(1)
    };

    let can_move = match params.direction {
        VerticalDirection::Down => b < last_real_line,
        VerticalDirection::Up => a > 0,
    };
    if !can_move {
        return Ok(EditResult {
            revision: buf.revision,
            cursor,
        });
    }

    // Compute the swap. `slice_top` contains the lines that come first in the original layout,
    // `slice_bottom` the lines that come second; we emit them in reverse. The only subtlety is
    // when the trailing slice doesn't end in '\n' (i.e. it's the buffer's final line without a
    // trailing newline): we have to move that newline-or-its-absence to the new last slice.
    let len_lines = buf.text.len_lines() as u32;
    let len_chars = buf.text.len_chars();
    let (edit_start, edit_end, new_text, line_delta) = match params.direction {
        VerticalDirection::Down => {
            let a_start = buf.text.line_to_char(a as usize);
            let bp1_start = buf.text.line_to_char((b + 1) as usize);
            let bp2_start = if (b + 2) <= len_lines {
                buf.text.line_to_char((b + 2) as usize)
            } else {
                len_chars
            };
            let slice_top: String = buf.text.slice(a_start..bp1_start).to_string();
            let slice_bottom: String = buf.text.slice(bp1_start..bp2_start).to_string();
            let new_text = swap_segments(&slice_top, &slice_bottom);
            (a_start, bp2_start, new_text, 1i32)
        }
        VerticalDirection::Up => {
            let am1_start = buf.text.line_to_char((a - 1) as usize);
            let a_start = buf.text.line_to_char(a as usize);
            let bp1_start = if (b + 1) <= len_lines {
                buf.text.line_to_char((b + 1) as usize)
            } else {
                len_chars
            };
            let slice_top: String = buf.text.slice(am1_start..a_start).to_string();
            let slice_bottom: String = buf.text.slice(a_start..bp1_start).to_string();
            let new_text = swap_segments(&slice_top, &slice_bottom);
            (am1_start, bp1_start, new_text, -1i32)
        }
    };

    // Snapshot per-client cursors so undo can restore them.
    let cursors_before: HashMap<ClientId, CursorState> = s
        .cursors
        .iter()
        .filter_map(|((c, bid), cs)| if *bid == buffer_id { Some((*c, *cs)) } else { None })
        .collect();

    let (revision, new_cursor) = {
        let buf_mut = s.buffers.get_mut(&buffer_id).expect("just checked");
        let revision = buf_mut.apply_edit(
            edit_start,
            edit_end,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        );

        // Shift the requesting client's cursor (position + anchor) by `line_delta`. Other
        // clients' cursors are clamped by the standard post-edit clamp below.
        let shift = |p: aether_protocol::LogicalPosition| aether_protocol::LogicalPosition {
            line: (p.line as i32 + line_delta).max(0) as u32,
            col: p.col,
        };
        let new_cursor = CursorState {
            position: motion::clamp_position(buf_mut, shift(cursor.position)),
            anchor: cursor
                .anchor
                .map(|a| motion::clamp_position(buf_mut, shift(a))),
            match_bracket: None,
        };
        let new_cursor = match new_cursor.anchor {
            Some(a) if a == new_cursor.position => CursorState {
                position: new_cursor.position,
                anchor: None,
                match_bracket: None,
            },
            _ => new_cursor,
        };
        (revision, new_cursor)
    };
    s.cursors.insert((client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Affected line range for viewport notifications.
    let (edit_first, edit_last_excl) = match params.direction {
        VerticalDirection::Down => (a, b + 2),
        VerticalDirection::Up => (a - 1, b + 1),
    };

    let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        if !ranges_overlap(
            vp.first_logical_line,
            vp.last_logical_line_exclusive,
            edit_first,
            edit_last_excl,
        ) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else { continue };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision, search)));
    }

    let new_cursor = wrap_for_response(&s, buffer_id, new_cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    Ok(EditResult { revision, cursor: new_cursor })
}

/// Build a new string with `bottom` first, then `top`, preserving "this is the last line of the
/// buffer and has no trailing newline" semantics. `top` is always followed by content so it ends
/// with '\n'; `bottom` ends with '\n' iff it's not the final segment of the buffer.
fn swap_segments(top: &str, bottom: &str) -> String {
    if bottom.ends_with('\n') {
        let mut s = String::with_capacity(top.len() + bottom.len());
        s.push_str(bottom);
        s.push_str(top);
        s
    } else {
        // `bottom` was the last line without a trailing '\n'. After the swap it sits in the
        // middle and needs a '\n' added; `top` takes the last-line spot and loses its '\n'.
        let mut s = String::with_capacity(top.len() + bottom.len() + 1);
        s.push_str(bottom);
        s.push('\n');
        s.push_str(top.strip_suffix('\n').unwrap_or(top));
        s
    }
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

    let (revision, new_cursor) = {
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
            match_bracket: None,
        };
        s.cursors.insert((client_id, buffer_id), new_cursor);
        s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
        s.clear_virtual_col_for_buffer(buffer_id);
        (revision, new_cursor)
    };

    // Push viewport/lines_changed for affected viewports (we changed multiple lines).
    let (pushes, search_summary_pushes, new_cursor): (Vec<_>, Vec<_>, _) = {
        let mut s = state.lock().await;
        let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);
        let new_line_count = s.buffers[&buffer_id].line_count();
        refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
        let buf = &s.buffers[&buffer_id];
        let mut pushes = Vec::new();
        for vp in s.viewports.values() {
            if vp.buffer_id != buffer_id {
                continue;
            }
            let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
                continue;
            };
            let search = s.searches.get(&(vp.client_id, buffer_id));
            pushes.push((sender, build_lines_changed_notif(buf, vp, revision, search)));
        }
        let new_cursor = wrap_for_response(&s, buffer_id, new_cursor);
        (pushes, search_summary_pushes, new_cursor)
    };

    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult { revision, cursor: new_cursor })
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
        });
    };

    let buf = s.buffers.get(&buffer_id).expect("just modified");
    let revision = buf.revision;

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
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);
    let undoing_cursor =
        new_cursors.get(&client_id).copied().unwrap_or_else(CursorState::default);

    // Push the full visible window to every viewport on this buffer — the rope was swapped
    // wholesale, so we can't be surgical about it.
    let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let buf_ref = &s.buffers[&buffer_id];
    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    for vp in s.viewports.values() {
        if vp.buffer_id != buffer_id {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, buffer_id));
        pushes.push((sender, build_lines_changed_notif(buf_ref, vp, revision, search)));
    }

    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(UndoResult { revision, applied: true, cursor: undoing_cursor })
}

fn clamp_cursor(buf: &Buffer, cursor: CursorState) -> CursorState {
    let position = motion::clamp_position(buf, cursor.position);
    let anchor = cursor.anchor.map(|a| motion::clamp_position(buf, a));
    let anchor = match anchor {
        Some(a) if a == position => None,
        x => x,
    };
    CursorState { position, anchor, match_bracket: None }
}

/// Populate `match_bracket` on a cursor that's about to cross the wire. Looks up the bracket
/// pair (if any) at the cursor's position and stamps it onto the state. `match_bracket` is
/// never stored in `state.cursors`; it's purely a derived per-response field that drives the
/// client's match-bracket highlight overlay.
fn with_match_bracket(buf: &Buffer, mut cursor: CursorState) -> CursorState {
    let Some(syntax) = buf.syntax.as_ref() else { return cursor };
    let byte = buf.text.char_to_byte(motion::pos_to_char(buf, cursor.position));
    if let Some((open, close)) = crate::brackets::find_match_bracket(&syntax.tree, byte) {
        let open_pos = motion::char_to_pos(buf, buf.text.byte_to_char(open));
        let close_pos = motion::char_to_pos(buf, buf.text.byte_to_char(close));
        cursor.match_bracket = Some((open_pos, close_pos));
    }
    cursor
}

/// Same as `with_match_bracket` but starts from a `ServerState`: a one-liner for the many
/// handlers that need to populate the field just before returning. Safe if the buffer was
/// already dropped (returns the cursor unchanged).
fn wrap_for_response(s: &ServerState, buffer_id: BufferId, cursor: CursorState) -> CursorState {
    s.buffers
        .get(&buffer_id)
        .map(|buf| with_match_bracket(buf, cursor))
        .unwrap_or(cursor)
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
            CursorState { position: position_pos, anchor: None, match_bracket: None }
        } else {
            CursorState { position: position_pos, anchor: Some(anchor_pos), match_bracket: None }
        }
    } else {
        CursorState {
            position: motion::char_to_pos(buf_mut, start_char + inserted_char_count),
            anchor: None,
            match_bracket: None,
        }
    };
    s.cursors.insert((client_id, buffer_id), new_cursor_state);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Recompute every active search on this buffer so the embedded `search_matches` in the
    // line-render data we're about to send out reflects the post-edit text.
    let search_summary_pushes = refresh_searches_for_buffer(&mut s, buffer_id);

    // Recompute every viewport's pushed range against the new line count, so a mutation that
    // *grew* the buffer (e.g. typing a newline) extends the window to cover the new lines.
    let new_line_count = s.buffers[&buffer_id].line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);

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
        let search = s.searches.get(&(vp.client_id, buffer_id));
        let notif = build_lines_changed_notif(buf_ref, vp, revision, search);
        pushes.push((sender, notif));
    }

    let new_cursor_state = wrap_for_response(&s, buffer_id, new_cursor_state);
    drop(s);

    for (sender, notif) in pushes {
        // If the receiver's gone, the client's connection has dropped; not our problem.
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult { revision, cursor: new_cursor_state })
}

fn ranges_overlap(a_start: u32, a_end_excl: u32, b_start: u32, b_end_excl: u32) -> bool {
    a_start < b_end_excl && b_start < a_end_excl
}

fn build_lines_changed_notif(
    buffer: &Buffer,
    vp: &Viewport,
    revision: Revision,
    search: Option<&SearchEntry>,
) -> Notification {
    let line_count = buffer.line_count();
    let new_first = vp.first_logical_line.min(line_count);
    let new_last_excl = vp.last_logical_line_exclusive.min(line_count).max(new_first);
    let window = render_window(buffer, new_first, new_last_excl, vp.cols, vp.wrap, vp.continuation_marker_width, vp.tab_width, vp.rows, search);
    let params = ViewportLinesChangedParams {
        viewport_id: vp.id,
        revision,
        range: LogicalLineRange {
            start_logical_line: vp.first_logical_line,
            end_logical_line_exclusive: vp.last_logical_line_exclusive,
        },
        replacement_lines: window.lines,
        line_count,
        max_scroll_logical_line: window.max_scroll_logical_line,
    };
    Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: serde_json::to_value(params).expect("infallible"),
    }
}

// ---- picker/* ----------------------------------------------------------------------------------

pub async fn picker_view(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerViewParams,
) -> Result<PickerViewResult, RpcError> {
    let client_id = ctx.require_hello()?;

    // Walk the workspace outside the global lock — on first call it can take seconds, and we
    // don't want to stall every other RPC. The `Arc<WorkspaceIndex>` is cheap to clone; the
    // walk itself is memoized inside the index.
    let workspace_index = {
        let s = state.lock().await;
        s.workspace_index.clone()
    };
    let candidates = workspace_index.files().await;

    let mut s = state.lock().await;
    let key = (client_id, params.kind);

    // (Re-)hydrate picker state. `reset` always wipes; otherwise we keep whatever was persisted
    // from a prior `view`/`query`/`hide` cycle. Split-borrow `pickers` and `matcher` from `s`
    // so we can hold mutable references to both at once.
    let ServerState { pickers, matcher, .. } = &mut *s;
    if params.reset {
        pickers.remove(&key);
    }
    if !pickers.contains_key(&key) {
        pickers.insert(key, picker_state::PickerState::new(params.kind, candidates.clone()));
    } else {
        // If the workspace index has been refreshed since the picker was last touched (file
        // watcher would do this once wired up), re-bind to the latest candidate set and re-rank.
        let p = pickers.get_mut(&key).expect("just checked");
        if !Arc::ptr_eq(&p.candidates, &candidates) {
            p.candidates = candidates.clone();
            p.rerank(matcher);
        }
    }
    let picker = pickers.get_mut(&key).expect("populated above");

    // Resolve the window. `center_on` wins over `offset` and picks a frame containing the item;
    // we centre it (roughly) so a small navigation away keeps it on screen. Falls through to
    // `offset` if the item isn't currently ranked.
    let limit = params.limit.max(1);
    let mut effective_offset = params.offset;
    if let Some(item) = params.center_on.as_ref() {
        if let Some(rank) = picker.rank_of(item) {
            let half = limit / 2;
            effective_offset = rank.saturating_sub(half);
        }
    }
    let total = picker.ranked.len() as u32;
    if effective_offset >= total {
        effective_offset = total.saturating_sub(limit);
    }
    picker.subscribed = Some(picker_state::SubscribedWindow { offset: effective_offset, limit });

    // Build the initial push so the client doesn't have to wait for an async update to arrive
    // before it can render. Caller will treat the response and the notification as redundant.
    let update = picker_state::build_update(picker, matcher);
    let result = PickerViewResult {
        query: picker.query.clone(),
        generation: picker.generation,
        total_candidates: picker.total_candidates(),
        effective_offset,
    };
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let notif = Notification {
            jsonrpc: JsonRpc,
            method: PickerUpdate::NAME.into(),
            params: serde_json::to_value(params).expect("infallible"),
        };
        let _ = sender.send(notif).await;
    }

    Ok(result)
}

pub async fn picker_query(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerQueryParams,
) -> Result<(), RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    let key = (client_id, params.kind);
    let ServerState { pickers, matcher, .. } = &mut *s;
    let Some(picker) = pickers.get_mut(&key) else {
        // No-op if the client never opened the picker. Could also error; silently dropping
        // matches the lenient style of other handlers.
        return Ok(());
    };
    picker.query = params.query;
    picker.generation = params.generation;
    picker.rerank(matcher);

    // After a query change, the prior `offset` may now be past the end of the result set. Clamp.
    if let Some(window) = picker.subscribed.as_mut() {
        let total = picker.ranked.len() as u32;
        if window.offset >= total {
            window.offset = total.saturating_sub(window.limit);
        }
    }

    let update = picker_state::build_update(picker, matcher);
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    drop(s);

    if let (Some(sender), Some(params)) = (outbound, update) {
        let notif = Notification {
            jsonrpc: JsonRpc,
            method: PickerUpdate::NAME.into(),
            params: serde_json::to_value(params).expect("infallible"),
        };
        let _ = sender.send(notif).await;
    }
    Ok(())
}

pub async fn picker_select(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerSelectParams,
) -> Result<PickerSelectResult, RpcError> {
    let client_id = ctx.require_hello()?;
    let s = state.lock().await;
    let key = (client_id, params.kind);
    let picker = s.pickers.get(&key).ok_or_else(|| {
        RpcError::new(ErrorCode::INVALID_REQUEST, "no active picker for this client")
    })?;
    match &params.item {
        PickerItem::File { .. } => {
            let abs = picker_state::resolve_file_abs(picker, &params.item).ok_or_else(|| {
                RpcError::invalid_params("selected item is not in the picker's candidate set")
            })?;
            Ok(PickerSelectResult::File { path: abs })
        }
    }
}

pub async fn picker_hide(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PickerHideParams,
) -> Result<(), RpcError> {
    let client_id = ctx.require_hello()?;
    let mut s = state.lock().await;
    if let Some(picker) = s.pickers.get_mut(&(client_id, params.kind)) {
        picker.subscribed = None;
    }
    Ok(())
}

