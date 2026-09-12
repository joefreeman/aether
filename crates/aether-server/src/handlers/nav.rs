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
    path_ref(s, client_id, &canonical)
}

/// [`buffer_path_ref`] for a path held on its own — what a document that has since been torn down
/// *would* have recorded. Per client, because the pair is relative to the client's own workspace
/// roots and two clients need not stand in the same workspace.
fn path_ref(
    s: &ServerState,
    client_id: ClientId,
    canonical: &Path,
) -> (Option<u32>, Option<String>) {
    let Some(workspace) = s.active_workspace(client_id) else {
        return (None, None);
    };
    for (i, root) in workspace.paths.iter().enumerate() {
        if canonical == root.as_path() {
            return (Some(i as u32), Some(String::new())); // the root directory itself
        }
        if let Ok(rel) = canonical.strip_prefix(root) {
            return (Some(i as u32), Some(rel.to_string_lossy().into_owned()));
        }
    }
    (None, None)
}

/// The client's current location as a nav entry: the **view** it is in, the cursor in it, and a
/// reopenable handle. `buffer_id` is the buffer the client names — its current one, which inside
/// a composed view is the buffer under the cursor rather than the view's own — and the entry is
/// the view's whichever it was: a step taken from inside a commit's patch names the hunk's file at
/// the revision, and an entry made of *that* stepped back onto the file, not the patch. `None` if
/// the buffer no longer exists.
pub fn nav_entry_for(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<NavEntry> {
    if !s.buffers.contains_key(&buffer_id) {
        return None;
    }
    // A buffer with no view yet records as one nothing names, which a step back then reopens by
    // path — a view id is never `0`, so the entry cannot mistake a later view for its own.
    let view_id =
        crate::handlers::viewport::client_view_of(s, client_id, buffer_id).unwrap_or_default();
    // The view's own buffer is the entry's identity: what the path fields and the virtual key
    // name, and what a step back re-presents.
    let buffer_id = s.try_presenting_buffer(view_id).unwrap_or(buffer_id);
    let (path_index, relative_path) = buffer_path_ref(s, client_id, buffer_id);
    let virtual_key = s
        .try_doc_of(buffer_id)
        .and_then(|d| d.virtual_source.as_ref())
        .map(|v| v.target.key());
    // Where the cursor *is*, which in a composed view is not this buffer. The viewport already
    // tracks which element holds it — the same field an edit, a search and an undo act through —
    // so the location is that element and the cursor of the buffer it windows.
    let element = focused_element(s, client_id, view_id)
        .filter(|(_, in_buffer)| *in_buffer != buffer_id)
        .map(|(element, _)| element);
    let cursor_in = match element {
        Some(e) => element_buffer(s, view_id, e).unwrap_or(buffer_id),
        None => buffer_id,
    };
    let cursor = s
        .cursors
        .get(&(client_id, cursor_in))
        .copied()
        .unwrap_or_default();
    Some(NavEntry {
        view_id,
        buffer_id,
        path_index,
        relative_path,
        virtual_key,
        element,
        cursor,
        read: Some(s.read_mode(client_id, buffer_id)),
    })
}

/// The element holding `view`'s cursor for this client, and the buffer it windows. `None` when the
/// client has no viewport on the view, or the view has no such element.
fn focused_element(
    s: &ServerState,
    client_id: ClientId,
    view: ViewId,
) -> Option<(aether_protocol::viewport::FieldId, BufferId)> {
    let focused = s
        .viewports
        .values()
        .find(|vp| vp.client_id == client_id && vp.view_id == view)?
        .focused;
    Some((focused, element_buffer(s, view, focused)?))
}

/// The buffer `element` of `view` windows.
fn element_buffer(
    s: &ServerState,
    view: ViewId,
    element: aether_protocol::viewport::FieldId,
) -> Option<BufferId> {
    Some(
        s.views
            .get(&view)?
            .elements
            .get(element as usize)?
            .buffer_id,
    )
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
    // Only the git-shaped targets can be re-materialised from a key: a shell's transcript is
    // this process's memory of what commands printed, and there is nothing to regenerate it from.
    // `None` is exactly right here — the callers read it as "this key names nothing restorable".
    let (repo_id, what) = (target.repo_id()?.to_string(), target.what()?.clone());
    let params = aether_protocol::git::GitShowParams {
        repo_id: Some(repo_id),
        buffer_id: None,
        target: what,
        // A reopen restores its own cursor; focusing a file would fight that.
        focus_path: None,
        // A history step is not a jump to record.
        record_nav_from: None,
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
/// motion in the per-buffer `z` history. Shared by `nav/back`/`nav/forward`, `nav/goto` and the
/// landing a `view/close` takes ([`history_landing`]).
pub async fn navigate_to(
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
            if let Some((scroll, cursor)) = restore_element(&mut s, ctx.client_id, &result, &entry)
            {
                result.scroll = Some(scroll);
                result.cursor = cursor;
            }
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
        // The mode you were in, not whatever the file has been shown as since: a preview that
        // closed behind you took the file's memory with it, and the setting would then decide.
        read: entry.read,
    };
    let mut result = view_open(state, ctx, open_params).await?;

    let mut s = state.lock().await;
    result.cursor = restore_cursor(&mut s, ctx.client_id, result.buffer_id, entry.cursor);
    if let Some((scroll, cursor)) = restore_element(&mut s, ctx.client_id, &result, &entry) {
        result.scroll = Some(scroll);
        result.cursor = cursor;
    }
    Ok(result)
}

/// Put a composed view's location back: the cursor into the buffer its element windows, and a
/// scroll naming that element so the view reopens framed on it.
///
/// Ordinary views need neither. Their open answers `scroll: None` on purpose — the client then
/// centres on the restored cursor with a single subscribe, and with one element there is nowhere
/// else for the cursor to be. A composed view has an element per hunk and a fresh subscribe takes
/// its focused element *from the scroll it names*, so `None` means element 0: the cursor is
/// restored into a buffer nothing on screen is showing and the view opens at the top of the patch.
///
/// Answers `None` when the entry named no element, or when the view no longer has it — a patch
/// regenerated against a tree that has moved on may be shorter than the one you left.
fn restore_element(
    s: &mut ServerState,
    client_id: ClientId,
    result: &ViewOpenResult,
    entry: &NavEntry,
) -> Option<(ScrollPosition, CursorState)> {
    let element = entry.element?;
    let view = crate::handlers::viewport::client_view_of(s, client_id, result.buffer_id)?;
    let buffer = element_buffer(s, view, element)?;
    let cursor = restore_cursor(s, client_id, buffer, entry.cursor);
    Some((
        ScrollPosition {
            element,
            line: cursor.position.line,
            sub_row: 0.0,
        },
        cursor,
    ))
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

/// Whether a history entry still names somewhere the client can be put.
///
/// A file entry can always be reopened, and so can a revision (it regenerates from its key); a
/// scratch, a shell or an agent only while it's still open — their keys name nothing that can be
/// regenerated ([`materialise_virtual_key`]), so a key alone is not a way back. Asked of the
/// target, not of the key's presence: judged by the key, a closed shell was "resolvable" and the
/// step fell through to an open with nothing to open.
fn resolvable(s: &ServerState, entry: &NavEntry) -> bool {
    let regenerates = entry.virtual_key.as_deref().is_some_and(|key| {
        crate::state::VirtualTarget::parse_key(key).is_some_and(|t| t.repo_id().is_some())
    });
    entry.path_index.is_some()
        || entry.relative_path.is_some()
        || regenerates
        || s.buffers.contains_key(&entry.buffer_id)
}

/// Pop one direction of `client_id`'s history until an entry that can be presented again, dropping
/// the ones that cannot. `None` once the stack is drained.
fn pop_resolvable(s: &mut ServerState, client_id: ClientId, forward: bool) -> Option<NavEntry> {
    loop {
        let entry = s.nav_history.get_mut(&client_id).and_then(|h| {
            if forward {
                h.forward.pop()
            } else {
                h.back.pop()
            }
        })?;
        if resolvable(s, &entry) {
            return Some(entry);
        }
    }
}

/// Every way a nav entry can name the document a close is tearing down: its buffer, the file it
/// was loaded from, and the key it was generated by. Captured **before** the teardown, which drops
/// the document the path and the key are read from.
#[derive(Clone, Debug, Default)]
pub struct ClosedDoc {
    pub buffer_id: BufferId,
    /// The canonical path, resolved per client into the pair [`nav_entry_for`] would have
    /// recorded — two clients need not stand in the same workspace.
    pub path: Option<std::path::PathBuf>,
    /// [`crate::state::VirtualSource`]'s key for a generated document (a patch, a file at a
    /// revision) — the handle a step back would have regenerated it from.
    pub virtual_key: Option<String>,
}

impl ClosedDoc {
    /// What closing `buffer_id` strikes from a trail. Call before the buffer is torn down.
    pub fn of(s: &ServerState, buffer_id: BufferId) -> Self {
        let doc = s.try_doc_of(buffer_id);
        ClosedDoc {
            buffer_id,
            path: doc.and_then(|d| d.canonical_path.clone()),
            virtual_key: doc
                .and_then(|d| d.virtual_source.as_ref())
                .map(|v| v.target.key()),
        }
    }
}

/// Strike `closed` from both of `client_id`'s stacks. A close is an explicit "not this", so no
/// step may resurrect what it closed — by buffer id, by path, or by the key it would regenerate
/// from.
pub fn prune_closed(s: &mut ServerState, client_id: ClientId, closed: &ClosedDoc) {
    // The pair the closed document would have recorded *for this client*. `None` for a scratch or
    // a file outside the client's roots, which then matches nothing — an entry with no path is
    // only ever named by its buffer id.
    let path = closed
        .path
        .as_deref()
        .map(|p| path_ref(s, client_id, p))
        .filter(|(_, rel)| rel.is_some());
    let names_it = |entry: &NavEntry| {
        entry.buffer_id == closed.buffer_id
            || (closed.virtual_key.is_some() && entry.virtual_key == closed.virtual_key)
            || path.as_ref().is_some_and(|(index, rel)| {
                entry.path_index == *index && entry.relative_path == *rel
            })
    };
    let Some(history) = s.nav_history.get_mut(&client_id) else {
        return;
    };
    history.back.retain(|entry| !names_it(entry));
    history.forward.retain(|entry| !names_it(entry));
}

/// Where a close should land `client_id`: the step back its own trail would take.
///
/// Nothing is pushed onto the other stack — the closed view is gone, not left. Back first; once
/// that is empty, the place you came *from* before stepping back is the natural landing, so
/// forward answers. `None` when the trail has nothing to say (a fresh window), leaving the caller
/// on its own successor rule.
///
/// Call **after** the teardown and after [`prune_closed`]: what is resolvable depends on which
/// buffers the close left standing, so an entry whose buffer the close collected reopens by path
/// or by key like any other step.
pub fn history_landing(s: &mut ServerState, client_id: ClientId) -> Option<NavEntry> {
    pop_resolvable(s, client_id, false).or_else(|| pop_resolvable(s, client_id, true))
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
        let chosen = pop_resolvable(&mut s, client_id, forward);
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
        // The web owns its own stacks and hands back what it was given; a composed view's element
        // is not in that payload, so this restores the cursor and lets the client frame itself.
        element: None,
        cursor: params.cursor,
        // The web owns its stacks and says what it recorded; nothing recorded leaves the mode to
        // the server's memory, as an ordinary open does.
        read: params.read,
    };
    Ok(NavStepResult {
        target: Some(navigate_to(state, ctx, entry).await?),
    })
}
