//! `nav/*` — per-client back/forward navigation history.

use super::*;

/// Map a buffer's canonical path to a `(path_index, relative_path)` within the client's active
/// workspace, so a nav entry can reopen the file even after it's been closed. `(None, None)` for a
/// scratch buffer (no path) or a buffer outside the active workspace's roots.
fn buffer_path_ref(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> (Option<u32>, Option<String>) {
    let Some(canonical) = s
        .try_doc_of(buffer_id)
        .and_then(|b| b.canonical_path.clone())
    else {
        return (None, None);
    };
    let Some(workspace) = s.active_workspace(client_id) else {
        return (None, None);
    };
    for (i, root) in workspace.paths.iter().enumerate() {
        if canonical == *root {
            return (Some(i as u32), Some(String::new())); // the root directory itself
        }
        if let Ok(rel) = canonical.strip_prefix(root) {
            return (Some(i as u32), Some(rel.to_string_lossy().into_owned()));
        }
    }
    (None, None)
}

/// The client's current location as a nav entry: the cursor it holds on `buffer_id` plus a
/// reopenable path ref. The buffer is supplied by the client (not inferred from a viewport, since
/// clients may hold several). `None` if that buffer no longer exists.
pub fn nav_entry_for(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<NavEntry> {
    if !s.buffers.contains_key(&buffer_id) {
        return None;
    }
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let (path_index, relative_path) = buffer_path_ref(s, client_id, buffer_id);
    let virtual_key = s
        .try_doc_of(buffer_id)
        .and_then(|d| d.virtual_source.as_ref())
        .map(|v| v.target.key());
    // A buffer with no view yet records as one nothing names, which a step back then reopens by
    // path — a view id is never `0`, so the entry cannot mistake a later view for its own.
    let view_id =
        crate::handlers::viewport::client_view_of(s, client_id, buffer_id).unwrap_or_default();
    Some(NavEntry {
        view_id,
        buffer_id,
        path_index,
        relative_path,
        virtual_key,
        cursor,
    })
}

/// Re-materialise whatever a virtual key names. Shared by nav-history restore and the dormant
/// (session) restore, which both hold an encoded key and no buffer.
///
/// `None` when the key doesn't decode — a session written by a future version, say — which the
/// callers treat as "not restorable" rather than as an error.
pub async fn materialise_virtual_key(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    key: &str,
) -> Option<Result<ViewOpenResult, RpcError>> {
    let target = crate::state::VirtualTarget::parse_key(key)?;
    let params = aether_protocol::git::GitShowParams {
        repo_id: Some(target.repo_id),
        buffer_id: None,
        target: target.what,
        // A reopen restores its own cursor; focusing a file would fight that.
        focus_path: None,
    };
    let shown = match git_show(state, ctx, params).await {
        Ok(shown) => shown,
        Err(e) => return Some(Err(e)),
    };
    // The key decoded fine — it named a working-changes view whose tree has since gone clean. An
    // error rather than `None`, which callers read as "this key means nothing" and answer by
    // falling back to a path or a buffer id that a virtual buffer never had.
    Some(shown.opened.ok_or_else(RpcError::nothing_to_show))
}

/// Open `entry`'s view (the view itself while it is still open, else its file by path) and restore
/// its full cursor/selection — clamped to the buffer's current bounds — *without* recording a
/// motion in the per-buffer `z` history. Shared by `nav/back`/`nav/forward` and `nav/goto`.
async fn navigate_to(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    entry: NavEntry,
) -> Result<ViewOpenResult, RpcError> {
    // A materialised revision has no path to reopen from, so it regenerates from its key — which
    // is stable across restarts (a repo id is its canonical workdir). Re-showing an already-open
    // revision attaches to the same buffer, so this is a switch when it's still there and a
    // regeneration when it isn't.
    if let Some(key) = entry.virtual_key.as_deref() {
        if let Some(opened) = materialise_virtual_key(state, ctx, key).await {
            let mut result = opened?;
            let mut s = state.lock().await;
            result.cursor = restore_cursor(&mut s, ctx.client_id, result.buffer_id, entry.cursor);
            return Ok(result);
        }
    }
    // The view itself while it is still open — that says which view of the file you were in, the
    // reader or the editor, and is the only handle a scratch has. Once it has closed, the path
    // reopens the file. `jump_to` is left unset — we restore the full selection below, not a
    // point.
    let view_is_live = state.lock().await.views.contains_key(&entry.view_id);
    let by_path = !view_is_live && (entry.path_index.is_some() || entry.relative_path.is_some());
    let open_params = ViewOpenParams {
        view_id: view_is_live.then_some(entry.view_id),
        path_index: entry.path_index.filter(|_| by_path),
        relative_path: entry.relative_path.clone().filter(|_| by_path),
        absolute_path: None,
        language: None,
        create_if_missing: false,
        jump_to: None,
        jump_to_anchor: None,
        // Stepping history through a since-closed file is a revisit, not a keep: reopen it
        // transient so walking the nav history doesn't re-accumulate buffers. No effect when the
        // buffer is still open (an open never demotes).
        transient: Some(true),
        record_nav_from: None,
        element: None,
        kind: None,
    };
    let mut result = view_open(state, ctx, open_params).await?;

    let mut s = state.lock().await;
    result.cursor = restore_cursor(&mut s, ctx.client_id, result.buffer_id, entry.cursor);
    Ok(result)
}

/// Seat a remembered cursor in `buffer_id`, clamped to what the buffer holds now.
///
/// A direct insert, *not* `record_motion` — a jump-back must not feed the per-buffer `z` history.
pub fn restore_cursor(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    remembered: CursorState,
) -> CursorState {
    let restored = match s.try_doc_of(buffer_id) {
        Some(buf) => CursorState {
            position: motion::clamp_position(buf, remembered.position),
            anchor: motion::clamp_position(buf, remembered.anchor),
            match_bracket: None,
            jumplist_position: None,
        },
        None => remembered,
    };
    s.cursors.insert((client_id, buffer_id), restored);
    restored
}

/// Shared back/forward step: pop the chosen stack (skipping unrecoverable entries — a closed
/// scratch), push the current location onto the other stack, and navigate to the popped entry.
async fn nav_step_dir(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    current_buffer: BufferId,
    forward: bool,
) -> Result<NavStepResult, RpcError> {
    let client_id = ctx.client_id;
    let chosen: Option<NavEntry> = {
        let mut s = state.lock().await;
        let current = nav_entry_for(&s, client_id, current_buffer);
        let mut chosen = None;
        loop {
            let popped = s.nav_history.get_mut(&client_id).and_then(|h| {
                if forward {
                    h.forward.pop()
                } else {
                    h.back.pop()
                }
            });
            let Some(entry) = popped else { break };
            // A file entry can always be reopened, and so can a revision (it regenerates from its
            // key); a scratch entry only if it's still open.
            let resolvable = entry.path_index.is_some()
                || entry.relative_path.is_some()
                || entry.virtual_key.is_some()
                || s.buffers.contains_key(&entry.buffer_id);
            if resolvable {
                chosen = Some(entry);
                break;
            }
        }
        if chosen.is_some() {
            if let Some(cur) = current {
                let hist = s.nav_history.entry(client_id).or_default();
                let other = if forward {
                    &mut hist.back
                } else {
                    &mut hist.forward
                };
                other.push(cur);
                if other.len() > crate::state::NAV_HISTORY_CAP {
                    other.remove(0);
                }
            }
        }
        chosen
    };
    match chosen {
        Some(entry) => Ok(NavStepResult {
            target: Some(navigate_to(state, ctx, entry).await?),
        }),
        None => Ok(NavStepResult { target: None }),
    }
}

pub async fn nav_step(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavStepParams,
) -> Result<NavStepResult, RpcError> {
    let forward = matches!(params.direction, Direction::Forward);
    nav_step_dir(state, ctx, params.buffer_id, forward).await
}

/// `nav/goto` — restore a stored entry without touching the server-side stacks. The web client
/// owns its back/forward stacks (native browser history); this just performs the navigation.
pub async fn nav_goto(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: NavGotoParams,
) -> Result<NavStepResult, RpcError> {
    let entry = NavEntry {
        view_id: params.view_id.unwrap_or_default(),
        buffer_id: 0,
        path_index: params.path_index,
        relative_path: params.relative_path,
        virtual_key: params.virtual_key,
        cursor: params.cursor,
    };
    Ok(NavStepResult {
        target: Some(navigate_to(state, ctx, entry).await?),
    })
}
