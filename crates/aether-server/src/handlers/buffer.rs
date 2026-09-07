//! `buffer/*` — close, save, reload, copy/cut, and the save-path divergence checks.

use super::*;

/// Close a buffer globally. Drops the buffer from the server, plus all viewports subscribed
/// to it across every client, all per-`(client, buffer)` state (cursors, motion history,
/// virtual col, tree-selection history, search, last scroll), and all MRU references.
/// Refreshes any subscribed view picker so clients see the buffer vanish from the list.
///
/// Closes are unconditional from the server's point of view — the client is expected to ask
/// for confirmation if the buffer is dirty.
pub async fn view_close(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewCloseParams,
) -> Result<aether_protocol::view::ViewCloseResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    // Close addresses a view; everything it then does — the live-buffer lookup, the dormant-row
    // fallback, the teardown — is about the buffer presenting it. One crossing, named here. A
    // view nothing knows is a *dormant* row's reserved view, whose reserved buffer the dormant
    // branch below forgets; a view that is neither falls through to `buffer_not_found`.
    let buffer_id = match s.try_presenting_buffer(params.view_id) {
        Some(buffer_id) => buffer_id,
        None => s
            .active_workspace(client_id)
            .map(|w| w.id.clone())
            .and_then(|w| s.dormant_buffer_of_view(&w, params.view_id))
            .unwrap_or(params.view_id.get()),
    };
    // What the closing client was showing through this view — its buffer and, for a composed
    // view, every file its elements window — so the collector can take what the close leaves
    // hidden and transient: a sibling asked for and never shown, a review's element buffers. A
    // close is a hiding like any other, and lands nowhere that was only ever a preview. Captured
    // before the close, which drops the viewport this reads.
    let left_behind: Vec<BufferId> = {
        let mut shown = vec![buffer_id];
        for vp in s
            .viewports
            .values()
            .filter(|v| v.client_id == client_id && v.view_id == params.view_id)
        {
            if let Some(view) = s.try_view(vp.view_id) {
                for b in vp.shown_buffers(view) {
                    if !shown.contains(&b) {
                        shown.push(b);
                    }
                }
            }
        }
        shown
    };
    // One view of several — a file's reader beside its editor — closes alone: the buffer, its text
    // and its other views stay, so there is nothing unsaved to ask about. Only a buffer's last
    // view is a buffer close. Whoever else was presenting the view loses their viewport with it,
    // and is told — the same push a buffer close sends, with no buffer in it.
    let presenting = clients_presenting_view(&s, params.view_id, client_id);
    if s.close_view(params.view_id) {
        let mut pushes = collect_after_close(&mut s, &left_behind);
        let next_view_id = next_view_for_client(&s, client_id);
        pushes.extend(refresh_view_pickers(&mut s));
        pushes.extend(view_closed_pushes(&s, params.view_id, &presenting));
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        let opened = if params.open_next {
            Some(view_open(state, ctx, successor_params(next_view_id)).await?)
        } else {
            None
        };
        return Ok(aether_protocol::view::ViewCloseResult {
            next_view_id,
            opened,
        });
    }
    if !s.buffers.contains_key(&buffer_id) {
        // Not a live buffer — but the view picker also lists *dormant* rows (session-restored,
        // not yet loaded), and `Ctrl-d` on one of those should drop it from the list just like
        // closing a live buffer. A dormant buffer has only a reserved id and a session entry, so its
        // close is much lighter: forget the entry, discard any backup it carried, refresh the picker,
        // and rewrite the session so it isn't restored next time. Falls through to `buffer_not_found`
        // when the id is neither live nor dormant.
        if let Some(workspace) = s.active_workspace(client_id).map(|w| w.id.clone()) {
            if let Some(dormant) = s.take_dormant(&workspace, buffer_id) {
                if let Some(root) = s.backups_path.as_deref() {
                    match &dormant.source {
                        // Discarding a dormant file discards the document-level backup: unsaved
                        // content is document-scoped now, so an explicit discard here discards it
                        // for every workspace whose session references the same path — the same
                        // way closing a shared live buffer with discard would.
                        crate::state::DormantSource::File(p) => {
                            crate::backup::delete(&crate::backup::file_backup_path(root, p))
                        }
                        crate::state::DormantSource::Scratch { number } => crate::backup::delete(
                            &crate::backup::scratch_backup_path(root, &workspace, *number),
                        ),
                        // A revision is read-only, so it never had a backup to discard. Dropping
                        // the dormant entry (above) is the whole of closing one.
                        crate::state::DormantSource::Virtual { .. } => {}
                        // Closing a dormant shell discards the snapshot it would have come
                        // back from.
                        crate::state::DormantSource::Shell { number } => crate::backup::delete(
                            &crate::backup::shell_backup_path(root, &workspace, *number),
                        ),
                        // Likewise a conversation: closing the row discards what it would have
                        // come back from.
                        crate::state::DormantSource::Agent { number } => crate::backup::delete(
                            &crate::backup::agent_backup_path(root, &workspace, *number),
                        ),
                    }
                }
                let pushes = refresh_view_pickers(&mut s);
                drop(s);
                for (sender, notif) in pushes {
                    let _ = sender.send(notif).await;
                }
                persist_workspace_session(state, &workspace, false).await;
                let next_view_id = {
                    let s = state.lock().await;
                    next_view_for_client(&s, client_id)
                };
                tracing::debug!(buffer_id = buffer_id, "dormant buffer closed");
                return Ok(aether_protocol::view::ViewCloseResult {
                    next_view_id,
                    opened: None,
                });
            }
        }
        return Err(RpcError::buffer_not_found(buffer_id));
    }
    // Any *other* client viewing this buffer is about to have it pulled out from under it — capture
    // them before teardown drops their viewports, so we can tell them to switch (see below).
    let affected = clients_affected_by_close(&s, &[buffer_id], client_id);
    // Remember the owning workspace before teardown drops the association, so we can retire an
    // ephemeral workspace once it loses its last buffer.
    let owning_workspace = s.workspace_for_buffer(buffer_id).map(str::to_string);
    // A file a composed view windows is not torn down out from under the view. Closing it means
    // the buffer stops being something opened *by name* — it leaves the view picker, the MRU and
    // the persisted session, and this client's presentation of it goes — while the content stays
    // exactly where the view's element shows it, in whichever client that is. It is collected with
    // the view, the way every element buffer is: a buffer lives while some view shows it, and this
    // is that rule applied to a close rather than an exception to it.
    //
    // Tearing it down instead dropped the review's layout and viewport silently; the window simply
    // stopped updating.
    let windowed_by_a_view = s.viewports.values().any(|v| {
        let view = s.view_of(v);
        view.presenting != buffer_id && view.binds(buffer_id)
    });
    if windowed_by_a_view {
        // Its views become previews — the composed view is what holds the buffer now — and this
        // client stops presenting them.
        let demoted: Vec<ViewId> = s
            .views_presenting(buffer_id)
            .into_iter()
            .filter(|view_id| {
                s.views
                    .get_mut(view_id)
                    .is_some_and(|view| !std::mem::replace(&mut view.transient, true))
            })
            .collect();
        s.drop_buffer_from_mru(buffer_id);
        let presenting = s.views_presenting(buffer_id);
        s.viewports
            .retain(|_, v| !(v.client_id == client_id && presenting.contains(&v.view_id)));
        let mut pushes = collect_after_close(&mut s, &left_behind);
        let next_view_id = next_view_for_client(&s, client_id);
        pushes.extend(collect_view_state_pushes(&s, &demoted));
        pushes.extend(refresh_view_pickers(&mut s));
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        tracing::debug!(
            buffer_id = buffer_id,
            "buffer closed by name; kept for the view windowing it"
        );
        let opened = if params.open_next {
            Some(view_open(state, ctx, successor_params(next_view_id)).await?)
        } else {
            None
        };
        if let Some(workspace) = &owning_workspace {
            persist_workspace_session(state, workspace, false).await;
        }
        return Ok(aether_protocol::view::ViewCloseResult {
            next_view_id,
            opened,
        });
    }
    // Closing is an explicit discard: drop any unsaved backup now, so the content isn't resurrected
    // the next time this path is opened (recover-on-open). Done before teardown drops the buffer.
    if let (Some(ws), Some(buf), Some(doc)) = (
        owning_workspace.as_deref(),
        s.buffers.get(&buffer_id),
        s.try_doc_of(buffer_id),
    ) {
        delete_buffer_backups(&s, ws, buf, doc);
    }
    let shell_number =
        s.try_doc_of(buffer_id)
            .and_then(|d| match d.virtual_source.as_ref().map(|v| &v.target) {
                Some(crate::state::VirtualTarget::Shell { number, .. }) => Some(*number),
                _ => None,
            });
    // Canonical teardown (drops the buffer + all its per-client slices, sends LSP `didClose`,
    // clears diagnostics, and tears down the language server if this was its last buffer).
    let stopped_server = s.close_buffer(buffer_id);
    // A shell writes itself down as it is torn down, so a workspace switch cannot lose it. This
    // is an explicit close — a discard — so the snapshot goes again.
    if let (Some(ws), Some(number), Some(root)) = (
        owning_workspace.as_deref(),
        shell_number,
        s.backups_path.as_deref(),
    ) {
        crate::backup::delete(&crate::backup::shell_backup_path(root, ws, number));
    }
    // The previews it was keeping alive — a review's element buffers — go with it.
    let collected = collect_after_close(&mut s, &left_behind);
    // If that was the last buffer of an ephemeral context, retire it — and evict any *other*
    // client still parked in it (e.g. one that joined it from the switcher). They're told the
    // buffer closed just below (`buffer_closed_pushes`) and drop to the chooser; the context
    // mustn't keep lingering in the switcher (or re-open onto a scratch) just because someone had
    // it selected. The initiating client closed with `open_next:false` (the ephemeral close path),
    // so no scratch successor is spawned here.
    let retired_ephemeral = owning_workspace
        .as_deref()
        .is_some_and(|pid| s.retire_ephemeral_if_empty(pid));
    // Pick the next buffer for the requesting client: top of the active workspace's MRU after
    // cleanup, or — if that's empty — any remaining buffer in the workspace. The client uses this
    // to attach without an extra RPC round-trip.
    let next_view_id = next_view_for_client(&s, client_id);
    let mut pushes = collected;
    pushes.extend(refresh_view_pickers(&mut s));
    // A retired ephemeral workspace drops out of any open switcher.
    if retired_ephemeral {
        pushes.extend(refresh_workspace_pickers(&mut s));
    }
    // Tell the other clients their active buffer vanished (each switches to its own next buffer).
    pushes.extend(buffer_closed_pushes(&s, &affected));
    // If closing this buffer shut its language server down, refresh any open "LSP servers"
    // picker so the now-gone server drops out of the list.
    if stopped_server.is_some() {
        pushes.extend(refresh_lsp_server_pickers(&mut s));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::debug!(buffer_id = buffer_id, "buffer closed");
    // Composite post-step: present the client's next view (or a placeholder) in the same
    // round-trip.
    let opened = if params.open_next {
        Some(view_open(state, ctx, successor_params(next_view_id)).await?)
    } else {
        None
    };
    // Closing changed the workspace's open-buffer set — refresh its persisted session so the closed
    // file isn't restored next time. No-op for an ephemeral (or already-retired) workspace.
    if let Some(workspace) = &owning_workspace {
        persist_workspace_session(state, workspace, false).await;
    }
    Ok(aether_protocol::view::ViewCloseResult {
        next_view_id,
        opened,
    })
}

/// The open a close hands on to: the next view when there is one, else a placeholder — a scratch
/// that is transient, as activation's is, gone as soon as something else is shown. Never a scratch
/// to keep: nothing asked for one.
fn successor_params(next_view_id: Option<ViewId>) -> ViewOpenParams {
    ViewOpenParams {
        view_id: next_view_id,
        transient: next_view_id.is_none().then_some(true),
        ..Default::default()
    }
}

/// Collect what a close left hidden and transient among `left_behind`
/// ([`ServerState::close_orphaned_transients`]), with the picker refreshes that owes.
fn collect_after_close(s: &mut ServerState, left_behind: &[BufferId]) -> PendingPushes {
    let (closed, stopped, closed_views) = s.close_orphaned_transients(left_behind.iter().copied());
    let mut pushes = Vec::new();
    if !closed.is_empty() || !closed_views.is_empty() {
        for &id in &closed {
            tracing::debug!(
                buffer_id = id,
                "transient buffer closed (left behind by a close)"
            );
        }
        pushes.extend(refresh_view_pickers(s));
    }
    if !stopped.is_empty() {
        pushes.extend(refresh_lsp_server_pickers(s));
    }
    pushes
}

// ---- buffer/save --------------------------------------------------------------------------------

pub async fn buffer_copy(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCopyResult, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let (start, end) = scope_range(buf, &cursor, params.scope);
    let text = buf.text.slice(start..end).to_string();
    Ok(BufferCopyResult { text })
}

/// Highlight a standalone snippet with the tree-sitter registry — the markdown reading view's
/// fenced code blocks. Stateless: a fresh parse per call (snippets are fence-sized), the fence
/// alias table resolving the language, injections included (a heredoc inside a snippet still
/// highlights). Unknown language → empty, never an error.
pub async fn syntax_highlight_snippet(
    _state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: aether_protocol::syntax::SyntaxHighlightSnippetParams,
) -> Result<aether_protocol::syntax::SyntaxHighlightSnippetResult, RpcError> {
    let empty = || aether_protocol::syntax::SyntaxHighlightSnippetResult {
        highlights: Vec::new(),
    };
    // The client sends the fence's full info string (the shells display it verbatim); the
    // first token names the grammar, the rest is fence metadata (```rust ignore).
    let Some(config) = crate::syntax::get_config(crate::syntax::fence_language(&params.language))
    else {
        return Ok(empty());
    };
    let mut parser = crate::syntax::make_parser(config);
    let Some(tree) = parser.parse(&params.text, None) else {
        return Ok(empty());
    };
    let injections = crate::syntax::compute_injections(config, &tree, &params.text);
    let highlights = crate::syntax::highlights_for_range(
        config,
        &tree,
        &injections,
        &params.text,
        0,
        params.text.len(),
    );
    Ok(aether_protocol::syntax::SyntaxHighlightSnippetResult { highlights })
}

/// Full buffer text at its current revision — the markdown reading view's content fetch. The whole
/// rope is materialized; markdown documents are small, and the reading view is the only caller.
pub async fn buffer_content(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: BufferContentParams,
) -> Result<BufferContentResult, RpcError> {
    let s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    Ok(BufferContentResult {
        revision: buf.revision,
        text: buf.text.to_string(),
    })
}

pub async fn buffer_cut(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferCopyParams,
) -> Result<BufferCutResult, RpcError> {
    let client_id = ctx.client_id;

    // Extract the text and compute the range while holding the lock; then apply the deletion via
    // `Buffer::apply_edit` (which handles the undo entry and tree update) and broadcast.
    let mut s = state.lock().await;
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let buf_ref = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let (start_char, end_char) = scope_range(buf_ref, &cursor, params.scope);
    let text = buf_ref.text.slice(start_char..end_char).to_string();
    let start_pos = motion::char_to_pos(buf_ref, start_char);
    let end_pos_exclusive = motion::char_to_pos(buf_ref, end_char);
    let old_first_line = start_pos.line;
    let old_last_line_excl = end_pos_exclusive.line.saturating_add(1);

    let cursors_before = document_cursor_snapshot(&s, params.buffer_id);

    let mut buf_mut = s.editable_doc(params.buffer_id)?;
    let was_dirty = buf_mut.dirty;
    let revision = buf_mut.apply_edit(
        start_char,
        end_char,
        "",
        EditKindTag::Delete,
        cursors_before,
    );
    let new_pos = motion::char_to_pos(&buf_mut, start_char);
    let new_cursor = CursorState {
        position: new_pos,
        anchor: new_pos,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, (client_id, params.buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(params.buffer_id);
    s.clear_tree_selection_history_for_buffer(params.buffer_id);
    s.clear_virtual_col_for_buffer(params.buffer_id);

    let mut search_summary_pushes = promote_transient(&mut s, params.buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, params.buffer_id));
    refresh_viewport_ranges_for_buffer(&mut s, params.buffer_id);

    let mut pushes: PendingPushes = Vec::new();
    for vp in s.viewports.values() {
        if !s.view_of(vp).binds(params.buffer_id) {
            continue;
        }
        if !vp.diff_view
            && !edit_touches_window(
                &s,
                vp,
                &[params.buffer_id],
                old_first_line,
                old_last_line_excl,
            )
        {
            // Out-of-window edit: nothing to render for this viewport, but a whole-document
            // consumer still needs the change signal.
            push_buffer_changed(&s, vp, &mut pushes);
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        pushes.push((
            sender,
            build_lines_changed_notif(&s, vp, lines_changed_cursor(&s, vp), SneakLabels::Hidden),
        ));
    }

    let picker_pushes = maybe_refresh_dirty(&mut s, params.buffer_id, was_dirty);
    // LSP: full-document sync.
    notify_lsp_change(&mut s, params.buffer_id);

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

    Ok(BufferCutResult {
        text,
        revision,
        cursor: new_cursor,
    })
}

/// Compute the `[start_char, end_char)` range for a copy/cut scope.
pub fn scope_range(buf: &Document, cursor: &CursorState, scope: CopyScope) -> (usize, usize) {
    match scope {
        CopyScope::Selection => {
            // The selection always covers at least 1 char (point: anchor == position). The
            // inclusive endpoint extension by 1 produces a non-empty char range.
            let (start_pos, end_pos) = motion::ordered(cursor.position, cursor.anchor);
            let start = motion::pos_to_char(buf, start_pos);
            let end = motion::pos_to_char(buf, end_pos);
            (start, (end + 1).min(buf.text.len_chars()))
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

/// Canonicalize a path that may not fully exist on disk: walk up to the deepest existing
/// ancestor, canonicalize that, then re-attach the not-yet-created tail components. Used by
/// `buffer_save`'s save-as path so we can boundary-check a yet-to-be-created subdirectory
/// before actually creating it.
///
/// Symlinks in the existing portion are resolved (standard `canonicalize` behaviour); the tail
/// is appended verbatim. Errors only on I/O other than `NotFound`, or when we walk all the way
/// up to a path with no parent.
pub fn canonicalize_partial(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(canon) => {
                let mut out = canon;
                // suffix was accumulated tail-first; reverse on attach.
                for component in suffix.iter().rev() {
                    out.push(component);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                let Some(parent) = cursor.parent().map(|p| p.to_path_buf()) else {
                    return Err(e);
                };
                suffix.push(name);
                cursor = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn buffer_save(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferSaveParams,
) -> Result<BufferSaveResult, RpcError> {
    let _client_id = ctx.client_id;
    {
        // A virtual buffer has no file behind it, and its content is generated from the repo
        // rather than owned — including via save-as, which would only make a copy nobody asked
        // the editor for.
        let s = state.lock().await;
        if s.try_doc_of(params.buffer_id)
            .is_some_and(|d| d.read_only())
        {
            return Err(RpcError::read_only_buffer(params.buffer_id));
        }
    }

    // Resolve the target absolute path.
    let target: std::path::PathBuf = match (params.path_index, params.relative_path.as_deref()) {
        (None, None) => {
            let s = state.lock().await;
            let buf = s
                .try_doc_of(params.buffer_id)
                .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
            buf.canonical_path
                .clone()
                .ok_or_else(RpcError::buffer_has_no_path)?
        }
        (Some(idx), rel) => {
            let s = state.lock().await;
            let base = s
                .active_workspace_or_err(ctx.client_id)?
                .paths
                .get(idx as usize)
                .ok_or_else(|| RpcError::invalid_path(format!("path_index {idx} out of range")))?
                .clone();
            drop(s);

            let target = match rel {
                None | Some("") => base,
                Some(r) => base.join(r),
            };

            // The target file may not exist yet (creating). Neither may some of its parent
            // directories — `save-as foo/bar/baz.txt` should `mkdir -p foo/bar` rather than
            // erroring. So: resolve the parent by canonicalizing the deepest *existing*
            // ancestor and re-attaching the not-yet-created tail; boundary-check that
            // resolved path *before* any I/O. The actual mkdir-p happens just before the
            // write below (which also covers the in-place save case where the buffer was
            // bound to a multi-segment path via `view/open { create_if_missing }`).
            let parent = target.parent().ok_or_else(|| {
                RpcError::invalid_path(format!("{} has no parent directory", target.display()))
            })?;
            let parent_canonical = canonicalize_partial(parent).map_err(|e| {
                RpcError::invalid_path(format!("canonicalizing {}: {e}", parent.display()))
            })?;
            let file_name = target
                .file_name()
                .ok_or_else(|| RpcError::invalid_path("save target has no file name"))?;
            let resolved = parent_canonical.join(file_name);

            let s = state.lock().await;
            if !s
                .active_workspace_or_err(ctx.client_id)?
                .contains(&resolved)
            {
                return Err(RpcError::invalid_path(format!(
                    "{} is outside the workspace's access boundary",
                    resolved.display()
                )));
            }
            drop(s);
            resolved
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params(
                "relative_path provided without path_index",
            ));
        }
    };

    // Save-as conflict + would-overwrite checks live in the same critical section as the
    // actual write so the existence check can't race with the save (TOCTOU). In v1 single-
    // client this is theoretical, but folding the locks keeps the invariant tidy.
    //
    // Conflict: target path already live as a *different* document — in this workspace or any
    // other — so saving onto it would fork the file (two documents over one inode). Refuse
    // rather than silently transferring the path. Skipped when the target is the saving
    // buffer's own document's path (the in-place save case).
    //
    // Would-overwrite: the file exists on disk but isn't this buffer's current path, and the
    // caller hasn't confirmed. The client retries with `overwrite: true` after asking.
    //
    // I/O happens under the lock; in v1 that's acceptable (single client). For multi-client
    // we'd clone the rope, drop the lock, write, then re-lock to update state.
    let (saved_at_unix_ms, revision) = {
        let mut s = state.lock().await;
        let active_workspace_name = s.active_workspace_or_err(ctx.client_id)?.id.clone();
        if let Some(owner) = s.document_for_path(&target) {
            if Some(owner) != s.buffers.get(&params.buffer_id).map(|b| b.document) {
                // Blame a buffer the client can name: this workspace's if it has one, else any
                // attachment of the owning document.
                let existing = s
                    .buffer_for_path_in_workspace(&active_workspace_name, &target)
                    .or_else(|| s.buffers_of_document(owner).into_iter().next());
                if let Some(existing) = existing {
                    return Err(RpcError::path_owned_by_buffer(existing));
                }
            }
        }
        if !params.overwrite && target.exists() {
            let own_path = s
                .try_doc_of(params.buffer_id)
                .and_then(|b| b.canonical_path.as_ref());
            if own_path.map(|p| p.as_path()) != Some(target.as_path()) {
                return Err(RpcError::would_overwrite(target.display()));
            }
        }
        // External-change check: only applies when saving in-place (target matches the buffer's
        // current path). Save-as to a different path is governed by the WOULD_OVERWRITE check
        // above; the buffer's external-change state for its prior path is no longer relevant.
        if !params.overwrite {
            let buf = s
                .try_doc_of(params.buffer_id)
                .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
            let saving_in_place = buf
                .canonical_path
                .as_deref()
                .map(|p| p == target.as_path())
                .unwrap_or(false);
            if saving_in_place {
                if buf.externally_deleted {
                    return Err(RpcError::externally_deleted(params.buffer_id));
                }
                if buf.externally_modified {
                    return Err(RpcError::externally_modified(params.buffer_id));
                }
            }
        }
        // Ensure the target's parent dir exists right before the write. Covers both:
        //   - save-as into a new subdir (`save-as foo/bar/baz.txt` with `foo/bar` missing);
        //   - in-place save of a buffer that was bound to a multi-segment path via
        //     `view/open { create_if_missing }` (the parent dirs deferred from open).
        // Idempotent when the parent already exists. Boundary check ran earlier — this
        // never creates dirs outside the workspace.
        if let Some(parent) = target.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(RpcError::file_io)?;
            }
        }
        let buf = s
            .try_doc_of_mut(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let saved_at = buf.save_to_disk(target).map_err(RpcError::file_io)?;
        (saved_at, buf.revision)
    };

    // Broadcast buffer/state to all clients with viewports on this buffer, and re-push any
    // open view pickers (the dirty flag just flipped off; the path may have moved on Save-As).
    let (state_pushes, picker_pushes) = {
        let mut s = state.lock().await;
        // Saving is a keep-this signal: promote the views the save came from (the first edit
        // normally got there already; this covers a clean-buffer save-as).
        let promoted = s.promote_views_of(params.buffer_id);
        if let Some(doc) = s.try_doc_of_mut(params.buffer_id) {
            doc.backed_up_revision = None;
        }
        // The content is now on disk — drop any unsaved backup (under both the file path and, for a
        // saved-as scratch, its old number). Cheap and immediate; the flush would otherwise clear it
        // on its next tick.
        if let (Some(ws), Some(buf), Some(doc)) = (
            s.workspace_for_buffer(params.buffer_id).map(str::to_string),
            s.buffers.get(&params.buffer_id),
            s.try_doc_of(params.buffer_id),
        ) {
            delete_buffer_backups(&s, &ws, buf, doc);
        }
        let mut state_pushes = collect_buffer_state_pushes(&s, params.buffer_id);
        state_pushes.extend(collect_view_state_pushes(&s, &promoted));
        let picker_pushes = refresh_view_pickers(&mut s);
        (state_pushes, picker_pushes)
    };
    let _ = saved_at_unix_ms; // saved_at is captured inside the helper via Buffer::last_modified.
    for (sender, notif) in state_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    // A save promotes the buffer to permanent (above) and is the clearest "this buffer matters"
    // signal — refresh the persisted session so an edit→save→quit reliably restores the file, even
    // though its transient open didn't persist it. Best-effort, named-workspaces-only.
    let workspace = {
        let s = state.lock().await;
        s.workspace_for_buffer(params.buffer_id).map(str::to_string)
    };
    if let Some(workspace) = workspace {
        persist_workspace_session(state, &workspace, false).await;
    }

    Ok(BufferSaveResult {
        saved_at_unix_ms,
        revision,
    })
}

pub async fn buffer_reload(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferReloadParams,
) -> Result<BufferReloadResult, RpcError> {
    let _client_id = ctx.client_id;
    let mut s = state.lock().await;
    // Nothing to reload from: the content came from a revision, not a file.
    if s.try_doc_of(params.buffer_id)
        .is_some_and(|d| d.read_only())
    {
        return Err(RpcError::read_only_buffer(params.buffer_id));
    }
    if !params.force {
        let buf = s
            .try_doc_of(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        if buf.dirty {
            return Err(RpcError::would_discard_changes(params.buffer_id));
        }
    }
    // A user-initiated reload is a keep-this-buffer signal: promote a transient view to
    // permanent, like save. Flipped before the reload so the view/state push below carries the
    // cleared flag. (The watcher's silent reload calls `reload_buffer_locked` directly and
    // deliberately doesn't promote — an external file change shouldn't pin a preview.)
    let promoted = s.promote_views_of(params.buffer_id);
    let (result, mut pushes) = reload_buffer_locked(&mut s, params.buffer_id)?;
    if !promoted.is_empty() {
        pushes.extend(collect_view_state_pushes(&s, &promoted));
        // Reloading a *clean* transient buffer flips no dirty state, so the reload's own
        // picker refresh doesn't fire — re-push open view pickers for the italics change.
        pushes.extend(refresh_view_pickers(&mut s));
    }
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// Set a view's transient flag explicitly — the `Space k` "keep" toggle. Unlike `view/open`'s
/// promote-only intent, this flips the flag either way. Applied unconditionally: the client owns
/// the "don't mark a view with unsaved edits transient" policy (auto-close would discard them), the
/// same way `view/close` leaves the discard decision to the client. Pushes `view/state` so every
/// client presenting the view updates, and refreshes open view pickers so the italic transient
/// label tracks it.
pub async fn view_set_transient(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewSetTransientParams,
) -> Result<ViewSetTransientResult, RpcError> {
    let _ = ctx;
    let mut s = state.lock().await;
    let view = s
        .views
        .get_mut(&params.view_id)
        .ok_or_else(|| RpcError::view_not_found(params.view_id))?;
    view.transient = params.transient;
    let buffer_id = view.presenting;
    let mut pushes = collect_view_state_pushes(&s, &[params.view_id]);
    pushes.extend(refresh_view_pickers(&mut s));
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    // Keeping/unkeeping changes whether this buffer is part of the persisted working set (the
    // session lists kept buffers only) — refresh it. `Space k` is the explicit "this matters"
    // signal, with no later save/switch to rely on, so it must persist here directly.
    let workspace = {
        let s = state.lock().await;
        s.workspace_for_buffer(buffer_id).map(str::to_string)
    };
    if let Some(workspace) = workspace {
        persist_workspace_session(state, &workspace, false).await;
    }
    Ok(ViewSetTransientResult {
        transient: params.transient,
    })
}

/// Re-read a buffer from disk inside the lock, returning the RPC result and the pushes the
/// caller should emit after dropping the lock. Shared between the `buffer/reload` handler and
/// the file-watcher's silent-reload path.
pub(crate) fn reload_buffer_locked(
    s: &mut ServerState,
    buffer_id: BufferId,
) -> Result<(BufferReloadResult, PendingPushes), RpcError> {
    let was_dirty = s.try_doc_of(buffer_id).map(|b| b.dirty).unwrap_or(false);

    let saved_at_unix_ms = {
        let mut buf = s.editable_doc(buffer_id)?;
        if buf.canonical_path.is_none() {
            return Err(RpcError::buffer_has_no_path());
        }
        buf.reload_from_disk().map_err(RpcError::file_io)?
    };

    // Clamp every cursor on the document (this buffer and any workspace sibling) to the new
    // bounds — rope was swapped wholesale.
    clamp_doc_cursors(s, buffer_id);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let search_summary_pushes = refresh_searches_for_buffer(s, buffer_id);
    refresh_viewport_ranges_for_buffer(s, buffer_id);
    // LSP: reload swapped the rope (manual or watcher-driven) — keep the server's analysis fresh.
    notify_lsp_change(s, buffer_id);

    let revision = s.doc_of(buffer_id).revision;
    let mut pushes: PendingPushes = collect_doc_lines_changed_pushes(s, buffer_id);

    let state_pushes = collect_buffer_state_pushes(s, buffer_id);
    let picker_pushes = maybe_refresh_dirty(s, buffer_id, was_dirty);

    pushes.extend(search_summary_pushes);
    pushes.extend(state_pushes);
    pushes.extend(picker_pushes);

    Ok((
        BufferReloadResult {
            revision,
            saved_at_unix_ms: Some(saved_at_unix_ms),
        },
        pushes,
    ))
}

/// Pick the cursor to return from `view/open`. When `clamped_jump` is set, build a fresh cursor
/// at that position and persist it into `s.cursors` (overriding any prior state for this
/// `(client, buffer)`); `clamped_anchor` makes it a *selection* (anchor there, cursor at the jump)
/// rather than a point. Otherwise return the previously-persisted cursor or default.
fn resolve_open_cursor(
    s: &mut ServerState,
    client_id: Option<ClientId>,
    buffer_id: BufferId,
    clamped_jump: Option<LogicalPosition>,
    clamped_anchor: Option<LogicalPosition>,
) -> CursorState {
    if let Some(clamped) = clamped_jump {
        let new = CursorState {
            position: clamped,
            anchor: clamped_anchor.unwrap_or(clamped),
            match_bracket: None,
            jumplist_position: None,
        };
        if let Some(c) = client_id {
            set_cursor(s, (c, buffer_id), new);
        }
        new
    } else {
        client_id
            .and_then(|c| s.cursors.get(&(c, buffer_id)).copied())
            .unwrap_or_default()
    }
}

pub async fn view_open(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewOpenParams,
) -> Result<ViewOpenResult, RpcError> {
    // Composite pre-step: record the jump origin onto this client's nav history — `nav/record`
    // folded in, so result-style opens are one round-trip.
    if let Some(from) = params.record_nav_from {
        let mut s = state.lock().await;
        if let Some(entry) = nav_entry_for(&s, ctx.client_id, from) {
            s.nav_history
                .entry(ctx.client_id)
                .or_default()
                .record(entry);
        }
    }
    let result = view_open_inner(state, ctx, params, OpenIntent::Navigate).await?;
    // Refresh the persisted session for this buffer's workspace: the open changed either its
    // membership (a new file) or its MRU order (a switch), both of which a restart restores from.
    // Skip transient opens — previews never enter the persisted set (see `session_buffer_paths`),
    // so persisting on every grep/picker peek would just rewrite identical content. Best-effort and
    // named-workspaces-only (the helper guards); a no-op when sessions aren't persisted.
    if !result.transient {
        let workspace = {
            let s = state.lock().await;
            s.workspace_for_buffer(result.buffer_id).map(str::to_string)
        };
        if let Some(workspace) = workspace {
            persist_workspace_session(state, &workspace, false).await;
        }
    }
    Ok(result)
}

/// The scroll position to seed a freshly-opened viewport with. A `jump_to` open (grep
/// navigation, goto-definition, nav history) deliberately moves the cursor elsewhere, so the
/// scroll the client last recorded for this buffer predates the jump and would frame the wrong
/// region — returning `None` lets the client centre on the jumped cursor with a single subscribe.
/// A plain (re)open with no jump restores the saved scroll, so reopening a file lands where you
/// left it.
fn open_scroll(
    s: &ServerState,
    client_id: Option<ClientId>,
    view: ViewId,
    jump_to: Option<LogicalPosition>,
) -> Option<ScrollPosition> {
    if jump_to.is_some() {
        return None;
    }
    client_id.and_then(|c| s.last_scroll.get(&(c, view)).copied())
}

/// The view an open of `buffer_id` presents — see [`ServerState::open_view_as`]: the kind the
/// caller asked for; else, for a `jump_to`, the reader the client has the file on screen in when
/// it does (a jump inside the document being read — its outline, a reference, a grep hit — stays
/// on the page) and otherwise the editor (a `line:col` means nothing over a rendered document the
/// client isn't on); else the file's most recently used view. Every open reports it, and every
/// open decides it here.
fn presented_view(
    s: &mut ServerState,
    client_id: Option<ClientId>,
    buffer_id: BufferId,
    params: &ViewOpenParams,
) -> ViewId {
    use aether_protocol::ui::ViewKind;
    let jumped = params.jump_to.is_some().then(|| {
        let reading = client_id.is_some_and(|c| {
            s.viewports.values().any(|vp| {
                let view = s.view_of(vp);
                vp.client_id == c
                    && view.presenting == buffer_id
                    && view.kind() == Some(ViewKind::Reader)
            })
        });
        if reading {
            ViewKind::Reader
        } else {
            ViewKind::Editor
        }
    });
    s.open_view_as(buffer_id, params.kind.or(jumped), params.transient)
}

/// Materialize a dormant *scratch* buffer (selected by id from the picker, or landed on at activate):
/// allocate a real buffer, restore its unsaved content from the backup keyed by `number`, and return
/// the open result. Mirrors the fresh-scratch arm of [`view_open_inner`] plus the backup overlay.
async fn open_restored_scratch(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewOpenParams,
    number: u32,
) -> Result<ViewOpenResult, RpcError> {
    let client_id = Some(ctx.client_id);
    let mut s = state.lock().await;
    let active_workspace_name = s.active_workspace_or_err(ctx.client_id)?.id.clone();
    let id = s.allocate_buffer_id();
    let doc_id = s.allocate_document_id();
    let mut doc = Document::scratch(doc_id, None);
    if let Some(root) = s.backups_path.clone() {
        let path = crate::backup::scratch_backup_path(&root, &active_workspace_name, number);
        if let Some((content, _mtime)) = crate::backup::read(&path) {
            doc.restore_unsaved(&content);
            // The on-disk backup already holds this content — don't let the flush rewrite it.
            doc.backed_up_revision = Some(doc.revision);
        }
    }
    let buf = Buffer {
        id,
        document: doc_id,
        scratch_number: Some(number),
    };
    let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(&doc, jt));
    let clamped_anchor = params
        .jump_to_anchor
        .map(|a| motion::clamp_position(&doc, a));
    let cursor = resolve_open_cursor(&mut s, client_id, id, clamped_jump, clamped_anchor);
    let mut result = ViewOpenResult {
        view_id: ViewId::default(), // minted below, once the buffer exists
        scroll: None,               // a fresh buffer has no view to have been scrolled in
        transient: false,           // the view's, read below once it exists
        buffer: BufferDescription {
            buffer_id: id,
            language: doc.language.clone(),
            line_count: doc.line_count(),
            byte_count: doc.byte_count(),
            revision: doc.revision,
            saved_revision: doc.saved_revision(),
            path: None,
            scratch_number: Some(number),
            cursor,
            lsp_server: None, // scratch buffers are never language-server-backed
            title: None,
            read_only: false,
            is_patch: false,
        },
    };
    s.documents.insert(doc_id, doc);
    s.buffers.insert(id, buf);
    result.view_id = presented_view(&mut s, client_id, id, &params);
    result.transient = s.view(result.view_id).transient;
    s.buffer_workspaces
        .insert(id, active_workspace_name.clone());
    s.touch_mru(id);
    let pushes = refresh_view_pickers(&mut s);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// `git/show`: materialise one of a repo's states into a read-only buffer — a commit's patch, one
/// file as of a commit, or everything not yet committed.
///
/// One handler for all three because they *are* one operation: build a diff (or read a blob),
/// render it, install it. The only thing the target changes is whether the work can be skipped —
/// a revision can't move, so re-showing one attaches to the buffer already holding it, while the
/// working tree has to be rebuilt. That is a guard, not a second code path.
///
/// Resolution is against **reachable** repos, not writable ones: this is a read, and a repo
/// reachable only through an open buffer stays fully read-eligible. Nothing here can mutate
/// anything, so the writability guard doesn't apply.
///
/// The content is generated off the lock: a large commit's patch is O(diff), which has no business
/// on the keystroke path's mutex.
///
/// Answers [`GitShowResult::opened`] `None` for a clean working tree — see that field for why an
/// empty patch is worth *not* opening.
/// Describe an already-open buffer, as an open would — without opening it.
///
/// `view/open` builds this same shape, but on the way it restores scroll, resolves a remembered
/// cursor, pins transients, touches the MRU and refreshes the pickers. Focus moving *within* a view
/// wants none of that: it is not a navigation (see [`OpenIntent`]), and the cursor is the caller's,
/// already resolved to the element it just moved to.
pub fn describe_buffer(
    s: &ServerState,
    buffer_id: BufferId,
    cursor: CursorState,
) -> Result<aether_protocol::view::BufferDescription, RpcError> {
    let buffer = s
        .buffers
        .get(&buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let doc = s.doc_of(buffer_id);
    Ok(aether_protocol::view::BufferDescription {
        buffer_id,
        language: doc.language.clone(),
        line_count: doc.line_count(),
        byte_count: doc.byte_count(),
        revision: doc.revision,
        saved_revision: doc.saved_revision(),
        path: doc.canonical_path.as_ref().map(|p| p.display().to_string()),
        scratch_number: buffer.scratch_number,
        cursor,
        lsp_server: buffer_lsp_server_ref(s, buffer_id),
        title: doc.virtual_source.as_ref().map(|v| v.title.clone()),
        read_only: doc.read_only(),
        is_patch: doc.patch().is_some(),
    })
}

/// The buffer for a working-tree file a *view* needs — opened once and reused, without counting as
/// somewhere the user navigated.
///
/// The working-changes patch's new side **is** the working tree, so its elements window the files
/// you are actually editing: edit a hunk and you are editing the file. That is the whole point of
/// the view, and the reason its elements bind to real paths rather than to blobs.
#[allow(dead_code)]
async fn buffer_for_working_file(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    absolute_path: &std::path::Path,
) -> Result<BufferId, RpcError> {
    Ok(view_open_inner(
        state,
        ctx,
        ViewOpenParams {
            absolute_path: Some(absolute_path.to_string_lossy().into_owned()),
            // Transient: this buffer exists because a **view** windows it, not because anyone asked
            // for the file. Opened permanent, `Space g w` on a busy tree quietly added every changed
            // file to the view picker, and they stayed after the view was gone.
            //
            // Nothing is lost by it. The flag only decides whether the buffer survives being
            // *hidden*, an edit or a save promotes it (so a hunk you type in stops being a preview),
            // and re-opening an already-permanent buffer never demotes it — `pin_buffer_if_requested`
            // only ever promotes. So a file you already had open stays exactly as you left it.
            transient: Some(true),
            ..Default::default()
        },
        OpenIntent::Bind,
    )
    .await?
    .buffer_id)
}

/// Bind a generated view being presented again to its files, in full.
///
/// Every rebuild between a view's first open and now binds only the files that have a buffer open
/// — `rebuild_view_layout` runs on the save, stage and teardown refreshes, where nothing can open
/// one — and hiding the view closes its transient element buffers. So a review brought back
/// through the view picker, a nav-history step or a jump came back windowing generated text
/// for every file but the one you happened to be in, and named those hunks in patch coordinates.
/// Being navigated to is a client's request, which is what it takes to open the files, so a view
/// presented again is bound exactly as a fresh one is. A no-op for anything but a patch view with
/// a file-backed element still windowing generated text.
async fn rebind_presented_view(state: &SharedState, ctx: &mut ConnectionCtx, buffer_id: BufferId) {
    let (repo_id, rev, paths) = {
        let s = state.lock().await;
        let Some(doc) = s.try_doc_of(buffer_id) else {
            return;
        };
        let (Some(generated), Some(source), Some(view)) = (
            doc.patch(),
            doc.virtual_source.as_ref(),
            s.view_presenting(buffer_id).and_then(|v| s.try_view(v)),
        ) else {
            return;
        };
        let rev = match source.target.what() {
            Some(aether_protocol::git::ShowTarget::WorkingChanges) => None,
            Some(aether_protocol::git::ShowTarget::Commit { rev }) => Some(rev.clone()),
            // A file at a revision has no regions to rebind; a shell has no files at all.
            Some(aether_protocol::git::ShowTarget::File { .. }) | None => return,
        };
        // The file each element's region belongs to, when it has one — the same reading of the
        // index `layout_over_files` makes.
        let file_of = |span: &crate::patch::ElementSpan| -> Option<&str> {
            let info = generated
                .index
                .lines
                .get(span.start_line as usize)
                .copied()
                .flatten()?;
            let hunk = info.hunk?;
            let file = generated.plan.files.get(info.file as usize)?;
            file.regions.get(hunk as usize)?;
            file.new_path.as_deref()
        };
        let unbound_with_file = generated
            .decorations
            .elements
            .iter()
            .zip(view.elements.iter())
            .any(|(span, binding)| binding.buffer_id == buffer_id && file_of(span).is_some());
        if !unbound_with_file {
            return;
        }
        let mut paths: Vec<String> = generated
            .decorations
            .elements
            .iter()
            .filter_map(file_of)
            .map(str::to_string)
            .collect();
        paths.sort_unstable();
        paths.dedup();
        (
            source.target.repo_id().unwrap_or_default().to_string(),
            rev,
            paths,
        )
    };
    // Resolve every file first — opening one is asynchronous — then lay out under the lock.
    let mut buffers: std::collections::HashMap<String, BufferId> = std::collections::HashMap::new();
    for path in paths {
        let resolved = match rev.as_deref() {
            Some(rev) => buffer_for_file_at_rev(state, ctx, &repo_id, rev, &path).await,
            None => {
                buffer_for_working_file(state, ctx, &std::path::Path::new(&repo_id).join(&path))
                    .await
            }
        };
        if let Ok(id) = resolved {
            buffers.insert(path, id);
        }
    }
    let mut s = state.lock().await;
    let Some(generated) = s.try_doc_of(buffer_id).and_then(|d| d.patch()) else {
        return;
    };
    let layout = crate::patch::layout_over_files(generated, |path| buffers.get(path).copied());
    s.set_view_layout(buffer_id, layout);
    s.rebind_viewports_of(buffer_id);
}

/// Build a patch view's elements over the **real files** it describes.
///
/// One element per generated region, each windowing its file at `rev` over the hunk's new-side
/// lines, carrying the diff's account of those lines as decorations and the region's chrome. Which
/// region an element came from is not guessed positionally: `PatchIndex.lines[start]` records every
/// line's `(file, hunk)`, so the mapping is read off the structure that produced the text.
///
/// Regions the diff gave no hunks — a binary swap, a mode change, a deletion with nothing to show —
/// keep windowing the generated document, which is where their placeholder line lives. A patch is
/// therefore a mix until the last of the generated text goes away.
async fn patch_elements_over_files(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    repo_id: &str,
    rev: Option<&str>,
    generated: &crate::patch::GeneratedPatch,
) -> Vec<crate::state::ElementLayout> {
    // Resolve every file the patch touches first — opening one is asynchronous, and the layout
    // itself is a pure function of the diff plus this map.
    let mut buffers: std::collections::HashMap<String, BufferId> = std::collections::HashMap::new();
    for file in &generated.plan.files {
        let Some(path) = file.new_path.as_deref() else {
            continue;
        };
        if buffers.contains_key(path) {
            continue;
        }
        // A revision's diff windows the blobs; the working-changes diff windows the working tree,
        // which is what makes its hunks editable.
        let resolved = match rev {
            Some(rev) => buffer_for_file_at_rev(state, ctx, repo_id, rev, path).await,
            None => {
                buffer_for_working_file(state, ctx, &std::path::Path::new(repo_id).join(path)).await
            }
        };
        // A file that will not materialise keeps its generated text rather than showing nothing:
        // the patch stays readable, just not editable there.
        if let Ok(buffer_id) = resolved {
            buffers.insert(path.to_string(), buffer_id);
        }
    }
    crate::patch::layout_over_files(generated, |path| buffers.get(path).copied())
}

/// The workspace's buffer already showing `target`, if any — the identity that stops re-showing the
/// same revision stacking duplicate buffers.
pub fn virtual_buffer_for(
    s: &ServerState,
    workspace: &str,
    target: &crate::state::VirtualTarget,
) -> Option<BufferId> {
    s.buffers_in_workspace(workspace).into_iter().find(|id| {
        s.buffers
            .get(id)
            .and_then(|b| s.documents.get(&b.document))
            .and_then(|d| d.virtual_source.as_ref())
            .is_some_and(|v| &v.target == target)
    })
}

/// The buffer holding `path` as of `rev` — `git show <rev>:<path>` as a real, parsed, highlighted
/// document.
///
/// Opened once per `(repo, rev, path)` and reused after: a revision cannot change under us, so an
/// already-open one is simply the answer and needs no regeneration. This is what lets a view window
/// a file it does not own — a patch's hunks over the real blobs rather than over a generated copy of
/// them — and it is deliberately *per file*, so a forty-file patch opens only what it renders.
// Exercised by tests, and by the patch driver once it binds elements to real files. Marked rather
// than wired to a contrived caller: the alternative was routing `git/follow_patch_line` through it,
// which needs the full open result and would have made this fit that caller instead of its purpose.
pub async fn buffer_for_file_at_rev(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    repo_id: &str,
    rev: &str,
    path: &str,
) -> Result<BufferId, RpcError> {
    let target = crate::state::VirtualTarget::new(
        repo_id.to_string(),
        aether_protocol::git::ShowTarget::File {
            rev: rev.to_string(),
            path: path.to_string(),
        },
    );
    let workspace = {
        let s = state.lock().await;
        s.active_workspace_or_err(ctx.client_id)?.id.clone()
    };
    if let Some(id) = {
        let s = state.lock().await;
        virtual_buffer_for(&s, &workspace, &target)
    } {
        return Ok(id);
    }

    let workdir = std::path::PathBuf::from(repo_id);
    let (rev, path) = (rev.to_string(), path.to_string());
    let content = tokio::task::spawn_blocking(move || crate::git::show_file(&workdir, &rev, &path))
        .await
        .map_err(|e| RpcError::internal(format!("git show: {e}")))?
        .map_err(RpcError::git_show_failed)?;
    Ok(
        open_generated_buffer(state, ctx, target, content, None, None, OpenIntent::Bind)
            .await?
            .buffer_id,
    )
}

pub async fn git_show(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::git::GitShowParams,
) -> Result<aether_protocol::git::GitShowResult, RpcError> {
    // A buffer answers for itself: its own status carries the baseline, so the field beside it is
    // for the answer that has no buffer.
    let opened = |open| aether_protocol::git::GitShowResult {
        opened: Some(open),
        baseline: None,
    };
    let client_id = ctx.client_id;
    let (target, workspace, sibling_repos) = {
        let s = state.lock().await;
        let repo_id = match &params.repo_id {
            Some(repo_id) => resolve_repo(&s, client_id, repo_id)?.repo_id,
            None => resolve_readable_repo(&s, client_id, params.buffer_id)?.repo_id,
        };
        // How many repos the workspace itself opened. The working-changes label disambiguates only
        // when there is something to disambiguate from — which repo you are in is otherwise the
        // status bar's branch indicator's job. Counted off the *roots*, not every reachable repo,
        // so opening a file from a dependency checkout doesn't retitle a buffer that has nothing to
        // do with it; skipped for a revision, whose title already carries a hash.
        let sibling_repos = match params.target {
            aether_protocol::git::ShowTarget::WorkingChanges => reachable_repos(&s, client_id)?
                .iter()
                .filter(|r| !r.roots.is_empty())
                .count(),
            _ => 1,
        };
        (
            crate::state::VirtualTarget::new(repo_id, params.target.clone()),
            s.active_workspace_or_err(client_id)?.id.clone(),
            sibling_repos,
        )
    };

    // Is this target already open? Its buffer is the one to reuse, whether by attaching to it or
    // by rewriting it in place.
    let existing = {
        let s = state.lock().await;
        virtual_buffer_for(&s, &workspace, &target)
    };
    // An immutable target that's already open has nothing to regenerate — attach, keeping whatever
    // cursor and pinned state it has, so re-selecting a log row lands you where you were.
    if let Some(buffer_id) = existing.filter(|_| target.is_immutable()) {
        return present_buffer(state, ctx, buffer_id, OpenIntent::Navigate)
            .await
            .map(opened);
    }

    let workdir = std::path::PathBuf::from(target.repo_id().unwrap_or_default());
    let what = target
        .what()
        .cloned()
        .unwrap_or(aether_protocol::git::ShowTarget::WorkingChanges);
    // The working-tree diff is measured against whatever `git/set_baseline` last set for this repo,
    // so the view and the gutters of the files in it can't disagree. Read before the spawn, since
    // the generation runs off the lock.
    let baseline = baseline_choice(state, &workdir).await;
    // The repo-level status rides along with the generation, off the lock and out of one repo
    // open: the buffer has no file to hang a baseline off, but the status bar still has to say
    // which checkout is in front of you.
    let (content, repo_status) = tokio::task::spawn_blocking({
        let baseline = baseline.clone();
        move || {
            use aether_protocol::git::ShowTarget;
            let content = match &what {
                ShowTarget::Commit { rev } => crate::git::show_commit(&workdir, rev),
                ShowTarget::File { rev, path } => crate::git::show_file(&workdir, rev, path),
                ShowTarget::WorkingChanges => {
                    crate::git::show_working_changes(&workdir, baseline.as_ref())
                }
            };
            (
                content,
                crate::git::repo_status(&workdir).map(|mut s| {
                    // The one buffer whose entire content is measured against the baseline has to
                    // carry the same status-bar token every ordinary buffer in the repo does — it
                    // has no `GitBaseline` of its own to carry it.
                    s.baseline = baseline;
                    s
                }),
            )
        }
    })
    .await
    .map_err(|e| RpcError::internal(format!("git show: {e}")))?;
    let mut content = content.map_err(RpcError::git_show_failed)?;
    // Two repos in one workspace would otherwise both list as "Working changes", naming neither.
    if sibling_repos > 1 {
        if let Some(name) = std::path::Path::new(target.repo_id().unwrap_or_default())
            .file_name()
            .and_then(|n| n.to_str())
        {
            content.title = format!("{} — {name}", content.title);
        }
    }

    // Nothing changed, and nothing already open to drain: report the emptiness instead of minting
    // a buffer whose entire content is a header saying so. Only the working tree can be empty —
    // a revision that resolves always has something to show, and one that doesn't is an error.
    let empty = content
        .generated
        .as_ref()
        .is_some_and(|g| g.index.files.is_empty());
    if empty && existing.is_none() {
        // Carry the baseline: with no buffer to hold the explanation, the toast is the only place
        // the user can be told that "nothing" was measured against something they chose.
        return Ok(aether_protocol::git::GitShowResult {
            opened: None,
            baseline,
        });
    }

    // A mutable target that's already open is rewritten in place — same buffer id, so viewports,
    // cursors and the nav history stay pointed at it, and the client's scroll anchor keeps your
    // place across the rebuild.
    if let Some(buffer_id) = existing {
        {
            let mut s = state.lock().await;
            s.replace_generated(
                buffer_id,
                &content.text,
                content.generated.map(Generated::Patch),
            );
            // Re-showing is a refresh point for the branch too: it may have moved (a checkout in a
            // terminal) since this buffer was minted.
            if let Some(status) = repo_status.clone() {
                s.virtual_git_status.insert(buffer_id, status);
            }
            // Clamp every viewer's cursor into the rebuilt text, as a reload does.
            let viewers: Vec<ClientId> = s
                .cursors
                .keys()
                .filter(|(_, b)| *b == buffer_id)
                .map(|(c, _)| *c)
                .collect();
            for c in viewers {
                let remembered = s.cursors[&(c, buffer_id)];
                restore_cursor(&mut s, c, buffer_id, remembered);
            }
        }
        return present_buffer(state, ctx, buffer_id, OpenIntent::Navigate)
            .await
            .map(opened);
    }

    // ---- patch element binding ---------------------------------------------------------------
    //
    // Both a commit and the working tree bind their hunks to the real files, and the difference
    // between them is mutability: a commit's blobs cannot change, so its extents cannot go stale,
    // while the working tree's can and do — editing a hunk is the point. That is what
    // `ServerState::shift_element_extents` handles, and `ViewLayout::of` clamps whatever it cannot
    // (a wholesale replacement), so a stale extent yields a short window rather than a panic.
    //
    // Regions the diff gave no hunks keep windowing the generated document either way; see
    // `patch::layout_over_files`.
    let layout = match (target.what(), content.generated.as_ref()) {
        (Some(aether_protocol::git::ShowTarget::Commit { rev }), Some(generated)) => {
            let (repo_id, rev) = (
                target.repo_id().unwrap_or_default().to_string(),
                rev.clone(),
            );
            Some(patch_elements_over_files(state, ctx, &repo_id, Some(&rev), generated).await)
        }
        (Some(aether_protocol::git::ShowTarget::WorkingChanges), Some(generated)) => {
            let repo_id = target.repo_id().unwrap_or_default().to_string();
            Some(patch_elements_over_files(state, ctx, &repo_id, None, generated).await)
        }
        _ => None,
    };

    let mut open = open_generated_buffer(
        state,
        ctx,
        target,
        content,
        repo_status,
        params.focus_path.as_deref(),
        OpenIntent::Navigate,
    )
    .await?;
    if let Some(layout) = layout {
        state.lock().await.set_view_layout(open.buffer_id, layout);
    }
    // Where the file the caller came for begins **in the view**: the element windowing it and the
    // line of that element's buffer. The client frames its subscribe on this, and a fresh subscribe
    // takes its focused element from the scroll — so this is what makes a patch opened on a file
    // open *in* that file. Read after the layout is installed, since the outline's lines are the
    // elements' own: before it they would be patch lines. The patch-line cursor the open reported
    // is the same place named in the patch document, for the callers that read the patch itself.
    if let Some(want) = params.focus_path.as_deref() {
        let mut s = state.lock().await;
        let id = open.buffer_id;
        let elements = s.view_elements_of(id).into_owned();
        if let Some(entry) = view_outline_of(&s, id, &elements)
            .into_iter()
            .find(|e| e.file == want)
        {
            open.scroll = Some(ScrollPosition {
                element: entry.element,
                line: entry.line,
                sub_row: 0.0,
            });
            if let Some(binding) = elements.get(entry.element as usize) {
                if binding.buffer_id != id {
                    let position = LogicalPosition {
                        line: entry.line,
                        col: 0,
                    };
                    set_cursor(
                        &mut s,
                        (ctx.client_id, binding.buffer_id),
                        CursorState {
                            position,
                            anchor: position,
                            match_bracket: None,
                            jumplist_position: None,
                        },
                    );
                }
            }
        }
    }
    Ok(opened(open))
}

/// Where a patch line leads: a blob in history, or — from the working-changes view's new side —
/// the working-tree file itself.
enum FollowTarget {
    Revision { rev: String, path: String },
    WorkingFile { abs_path: std::path::PathBuf },
}

/// `Enter` in a generated patch: open the file the line under the cursor came from, at the
/// revision that side of the diff belongs to.
pub async fn git_follow_patch_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::git::GitFollowPatchLineParams,
) -> Result<aether_protocol::git::GitFollowPatchLineResult, RpcError> {
    use crate::patch::PatchFileStatus;
    use aether_protocol::git::{GitFollowPatchLineResult, GitShowParams};

    let client_id = ctx.client_id;
    let none = || GitFollowPatchLineResult { opened: None };

    // Resolve everything the jump needs under one short lock, then let go: materialising the file
    // is `git_show`'s job and it takes the lock itself.
    // A patch element windows the file *itself*, at a revision, so following from one needs no
    // patch index: the buffer already says which file and which revision it is, and the cursor is
    // already on one of its real lines. `Enter` leads from the blob to the working tree's copy of
    // it, at the same place. The index path below is for a patch whose elements are still slices of
    // a generated document.
    let from_element = {
        let s = state.lock().await;
        match s
            .try_doc_of(params.buffer_id)
            .and_then(|d| d.virtual_source.as_ref())
            .and_then(|v| Some((v.target.repo_id()?.to_string(), v.target.what()?)))
        {
            Some((repo_id, aether_protocol::git::ShowTarget::File { path, .. })) => {
                let cursor_line = s
                    .cursors
                    .get(&(client_id, params.buffer_id))
                    .map(|c| c.position.line)
                    .unwrap_or_default();
                let abs_path = std::path::Path::new(&repo_id).join(path);
                // `lineno` is 1-based, as libgit2 reports it; the cursor is 0-based.
                Some((
                    repo_id,
                    FollowTarget::WorkingFile { abs_path },
                    Some(cursor_line + 1),
                ))
            }
            _ => None,
        }
    };

    let resolved = match from_element {
        Some(found) => Some(found),
        None => {
            let s = state.lock().await;
            let Some(doc) = s.try_doc_of(params.buffer_id) else {
                return Ok(none());
            };
            let Some(source) = doc.virtual_source.as_ref() else {
                return Ok(none());
            };

            let Some(generated) = doc.patch() else {
                return Ok(none());
            };
            let repo_id = source.target.repo_id().unwrap_or_default();
            let cursor_line = s
                .cursors
                .get(&(client_id, params.buffer_id))
                .map(|c| c.position.line)
                .unwrap_or_default();
            // The metadata block and the message belong to no file — nothing to follow.
            let Some(Some(info)) = generated.index.lines.get(cursor_line as usize).copied() else {
                return Ok(none());
            };
            let Some(file) = generated.index.files.get(info.file as usize) else {
                return Ok(none());
            };

            // A deleted file exists only on the old side and an added file only on the new one, so
            // those decide before the line's own side does — otherwise `Enter` on a context line of a
            // deleted file would ask for a blob that isn't there.
            let take_old = match file.status {
                PatchFileStatus::Deleted => true,
                PatchFileStatus::Added => false,
                _ => info.side == Some(aether_protocol::viewport::PatchLine::Removed),
            };
            let (path, lineno) = if take_old {
                (file.old_path.clone(), info.old_lineno)
            } else {
                (file.new_path.clone(), info.new_lineno)
            };
            let Some(path) = path else {
                return Ok(none());
            };
            let follow = match source.target.rev() {
                // A commit's diff: both sides are blobs in history. The old side is the first parent's.
                // A root commit has none, but also has no removals to follow, so that can't be reached.
                Some(rev) if take_old => {
                    let Some(parent) = crate::git::first_parent(std::path::Path::new(repo_id), rev)
                    else {
                        return Ok(none());
                    };
                    FollowTarget::Revision { rev: parent, path }
                }
                Some(rev) => FollowTarget::Revision {
                    rev: rev.to_string(),
                    path,
                },
                // The working-changes view. Its new side is the working tree itself, so `Enter` leads
                // to the **real file** — the one place a patch leads somewhere editable, and what makes
                // the view a place to work from rather than only to read. Its old side is HEAD, which
                // is what `git diff HEAD` compared against.
                None if take_old => FollowTarget::Revision {
                    rev: "HEAD".to_string(),
                    path,
                },
                None => FollowTarget::WorkingFile {
                    abs_path: std::path::Path::new(repo_id).join(path),
                },
            };
            Some((repo_id.to_string(), follow, lineno))
        }
    };
    let Some((repo_id, follow, lineno)) = resolved else {
        return Ok(none());
    };

    // Record the jump origin before leaving, as an ordinary open would through
    // `record_nav_from` — `git/show` mints its buffer itself, so nothing else would. This is what
    // `Backspace` walks back to, and the entry carries the patch's key rather than a path, so it
    // returns even though the transient patch closes the moment this open hides it.
    {
        let mut s = state.lock().await;
        if let Some(entry) = nav_entry_for(&s, client_id, params.buffer_id) {
            s.nav_history.entry(client_id).or_default().record(entry);
        }
    }

    let opened = match follow {
        // A file at a revision always materialises, so this is `Some` — the empty answer belongs
        // to the working tree, which this arm is not.
        FollowTarget::Revision { rev, path } => {
            let shown = git_show(
                state,
                ctx,
                GitShowParams {
                    repo_id: Some(repo_id),
                    buffer_id: None,
                    target: aether_protocol::git::ShowTarget::File { rev, path },
                    focus_path: None,
                },
            )
            .await?;
            let Some(opened) = shown.opened else {
                return Ok(none());
            };
            opened
        }
        // An ordinary open, deliberately not transient: following a line into your own working
        // tree is going somewhere to work, not previewing.
        FollowTarget::WorkingFile { abs_path } => {
            Box::pin(view_open(
                state,
                ctx,
                ViewOpenParams {
                    absolute_path: Some(abs_path.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            ))
            .await?
        }
    };

    // Land on the line the patch line came from. libgit2 counts from 1; buffers from 0. A
    // placeholder line (a binary or mode-only delta) carries no line number and opens at the top.
    let Some(lineno) = lineno else {
        return Ok(GitFollowPatchLineResult {
            opened: Some(opened),
        });
    };
    let mut s = state.lock().await;
    let buf = s.doc_of(opened.buffer_id);
    let position = motion::clamp_position(
        buf,
        LogicalPosition {
            line: lineno.saturating_sub(1),
            col: 0,
        },
    );
    let cursor = CursorState {
        position,
        anchor: position,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, (client_id, opened.buffer_id), cursor);
    Ok(GitFollowPatchLineResult {
        opened: Some(ViewOpenResult {
            buffer: BufferDescription {
                cursor,
                ..opened.buffer
            },
            ..opened
        }),
    })
}

/// Install a freshly materialised revision or diff as a new read-only buffer.
///
/// Takes whatever `git/show` materialised — a commit's patch, a file at a revision, the working
/// tree's diff — since all three want identical buffer semantics and differ only in what they
/// generated and what key it answers to.
/// Why a buffer is being opened.
///
/// A **navigation** is something the user did: it belongs at the top of the MRU list and the buffer
/// pickers should learn about it. A **binding** is a buffer a *view* needs in order to render — a
/// patch's hunks over the real blobs — and must do neither, or opening a forty-file diff would put
/// forty files at the top of your recent list and flood the picker with content you never asked to
/// open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    Navigate,
    Bind,
}

async fn open_generated_buffer(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    target: crate::state::VirtualTarget,
    content: crate::git::RevisionContent,
    repo_status: Option<aether_protocol::git::GitBufferStatus>,
    focus_path: Option<&str>,
    intent: OpenIntent,
) -> Result<ViewOpenResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let active_workspace_name = s.active_workspace_or_err(client_id)?.id.clone();
    let id = s.allocate_buffer_id();
    let doc_id = s.allocate_document_id();
    let doc = Document::virtual_content(
        doc_id,
        crate::state::VirtualSource {
            target,
            title: content.title.clone(),
        },
        content.text,
        content.language,
        content.generated.map(Generated::Patch),
        // A view's buffer must not stall the view: see `OpenIntent`.
        intent == OpenIntent::Bind,
    );
    // Transient: a revision view is a preview, so it closes itself once nothing shows it. Since a
    // read-only buffer can never be promoted by an edit or a save, `Space k` is the only way to
    // pin one — which is exactly the affordance a user wants for "keep this diff open".
    let buf = Buffer {
        id,
        document: doc_id,
        scratch_number: None,
    };
    // Land on the file the caller came here *for* — the file-locked log picker is showing this
    // commit because of one path, so opening at the top of a diff touching thirty others answers a
    // question nobody asked. Its first *change*, not its header: the header is a virtual row and
    // has no cursor position, and the changes are what you came to read.
    let focused = focus_path
        .zip(doc.patch())
        .and_then(|(want, generated)| {
            let file = generated.index.files.iter().find(|f| f.path() == want)?;
            let line = file
                .changes
                .first()
                .map_or(file.start_line, |c| c.start_line);
            Some(CursorState {
                position: LogicalPosition { line, col: 0 },
                anchor: LogicalPosition { line, col: 0 },
                match_bracket: None,
                jumplist_position: None,
            })
        })
        .unwrap_or_default();
    let mut result = ViewOpenResult {
        view_id: ViewId::default(), // minted below, once the buffer exists
        scroll: None,
        transient: true, // a materialised revision opens as a preview, kept with `Space k`
        buffer: BufferDescription {
            buffer_id: id,
            language: doc.language.clone(),
            line_count: doc.line_count(),
            byte_count: doc.byte_count(),
            revision: doc.revision,
            saved_revision: doc.saved_revision(),
            path: None,
            scratch_number: None,
            cursor: focused,
            lsp_server: None, // no file on disk for a server to have an opinion about
            title: Some(content.title),
            read_only: true,
            // A commit's diff, not a file at a revision — both are read-only, only the first has a
            // patch index for `Enter` to follow through.
            is_patch: doc.patch().is_some(),
        },
    };
    s.documents.insert(doc_id, doc);
    s.buffers.insert(id, buf);
    result.view_id = s.open_view_as(id, None, Some(true));
    if let Some(status) = repo_status {
        s.virtual_git_status.insert(id, status);
    }
    s.buffer_workspaces
        .insert(id, active_workspace_name.clone());
    if focused.position.line != 0 {
        set_cursor(&mut s, (client_id, id), focused);
    }
    let pushes = match intent {
        OpenIntent::Navigate => {
            s.touch_mru(id);
            refresh_view_pickers(&mut s)
        }
        OpenIntent::Bind => Vec::new(),
    };
    let parse_pending = s.doc_of(id).syntax_pending;
    let parse_token = parse_pending.then(|| s.deferred.start());
    drop(s);
    // The tree lands later and a `viewport/lines_changed` push restyles whatever is on screen.
    if let Some(token) = parse_token {
        tokio::spawn(finish_pending_parse(state.clone(), id, token));
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// Present a live buffer the server already holds — a revision found materialised, a scratch
/// rebuilt — through its most recently used view. The wire has no open by buffer: a client names
/// a view, and only the server ever arrives at an open holding a buffer and nothing else.
pub(crate) async fn present_buffer(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    intent: OpenIntent,
) -> Result<ViewOpenResult, RpcError> {
    let view_id = {
        let s = state.lock().await;
        s.view_presenting(buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?
    };
    view_open_inner(
        state,
        ctx,
        ViewOpenParams {
            view_id: Some(view_id),
            ..Default::default()
        },
        intent,
    )
    .await
}

async fn view_open_inner(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewOpenParams,
    intent: OpenIntent,
) -> Result<ViewOpenResult, RpcError> {
    // `Option` wrapping is vestigial — every connected client has an id now (assigned at WS
    // accept). Kept locally so the surrounding code (which threads cursor/scroll lookups through
    // `Option<ClientId>`) stays unchanged.
    let client_id = Some(ctx.client_id);
    let active_workspace_name: String = {
        let s = state.lock().await;
        s.active_workspace_or_err(ctx.client_id)?.id.clone()
    };

    // A view named outright — a picker row, the view a close hands on to, the one `Space u` is
    // leaving — is its buffer and a kind: the kind asked for, else the view's own. The open below
    // finds the view again by them (a buffer has at most one view per kind), or makes the sibling.
    // A dormant row's reserved view is its reserved buffer, which the by-buffer path materialises.
    let (attach, params) = match params.view_id {
        Some(view) => {
            let s = state.lock().await;
            let (buffer_id, kind) = match (s.try_presenting_buffer(view), params.element) {
                // The file one of the view's elements windows, as its own view — of no kind the
                // view's own says anything about.
                (Some(_), Some(element)) => (
                    s.try_view(view)
                        .and_then(|v| v.elements.get(element as usize))
                        .map(|b| b.buffer_id)
                        .ok_or_else(|| {
                            RpcError::invalid_params(format!(
                                "view {} has no element {element}",
                                view.get()
                            ))
                        })?,
                    None,
                ),
                (Some(buffer_id), None) => (buffer_id, s.try_view(view).and_then(|v| v.kind())),
                (None, _) => (
                    s.dormant_buffer_of_view(&active_workspace_name, view)
                        .ok_or_else(|| RpcError::view_not_found(view))?,
                    None,
                ),
            };
            (
                Some(buffer_id),
                ViewOpenParams {
                    view_id: None,
                    element: None,
                    kind: params.kind.or(kind),
                    ..params
                },
            )
        }
        None => (None, params),
    };

    // Attach to a buffer: the view's, or one the server holds (`present_buffer`). Ignores the
    // path fields.
    if let Some(buffer_id) = attach {
        // A dormant (session-restored) row the picker selected has no live buffer behind it yet.
        // Materialize it: remove it from the dormant list and rebuild the real buffer. The
        // reserved dormant ids are discarded — the client switches to the freshly-allocated real
        // view in the response.
        let dormant = {
            let mut s = state.lock().await;
            if s.buffers.contains_key(&buffer_id) {
                None
            } else {
                s.take_dormant(&active_workspace_name, buffer_id)
            }
        };
        let dormant_kind = dormant.as_ref().and_then(|d| d.kind);
        match dormant.map(|d| d.source) {
            // A file re-dispatches as an absolute-path open, which loads the file and attaches
            // git/LSP exactly like a fresh open — and picks up any backup via recover-on-open. As
            // the kind the row stood for: a kept reader's row opens the reader.
            Some(crate::state::DormantSource::File(path)) => {
                let materialize = ViewOpenParams {
                    absolute_path: Some(path.display().to_string()),
                    kind: dormant_kind.or(params.kind),
                    ..params
                };
                return Box::pin(view_open_inner(
                    state,
                    ctx,
                    materialize,
                    OpenIntent::Navigate,
                ))
                .await;
            }
            // A scratch has no path to re-dispatch through — rebuild it directly, restoring its
            // unsaved content from the backup keyed by its number.
            Some(crate::state::DormantSource::Scratch { number }) => {
                return open_restored_scratch(state, ctx, params, number).await;
            }
            // A revision regenerates from its key. An unresolvable one — a commit rebased away
            // since the session was written — surfaces as the `git/show` error rather than a
            // silently empty buffer.
            Some(crate::state::DormantSource::Virtual { key }) => {
                let mut opened = match Box::pin(materialise_virtual_key(state, ctx, &key)).await {
                    Some(opened) => opened?,
                    None => return Err(RpcError::buffer_not_found(buffer_id)),
                };
                // A revision opens as a preview when asked for. This one was kept, or it would
                // not have been in the session to come back from — so it comes back kept.
                let pushes = {
                    let mut s = state.lock().await;
                    pin_view_if_requested(&mut s, opened.view_id, Some(false))
                };
                for (sender, notif) in pushes {
                    let _ = sender.send(notif).await;
                }
                opened.transient = false;
                return Ok(opened);
            }
            // A shell comes back from its snapshot: the transcript and its runs, where it was,
            // what it had assigned, and what was being typed.
            Some(crate::state::DormantSource::Shell { number }) => {
                return Box::pin(crate::handlers::shell::open_restored_shell(
                    state, ctx, number,
                ))
                .await;
            }
            Some(crate::state::DormantSource::Agent { number }) => {
                return Box::pin(crate::handlers::agent::open_restored_agent(
                    state, ctx, number,
                ))
                .await;
            }
            None => {}
        }
        // A view navigated back to is bound to its files in full — see `rebind_presented_view`.
        if intent == OpenIntent::Navigate {
            Box::pin(rebind_presented_view(state, ctx, buffer_id)).await;
        }

        let mut s = state.lock().await;
        let scratch_number = s
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?
            .scratch_number;
        let doc = s.doc_of(buffer_id);
        let language = doc.language.clone();
        let line_count = doc.line_count();
        let byte_count = doc.byte_count();
        let revision = doc.revision;
        let saved_revision = doc.saved_revision();
        let path = doc.canonical_path.as_ref().map(|p| p.display().to_string());
        // The only reopen path that can meet a virtual buffer: switching back to one through the
        // view picker, which opens by id because there's no path to dispatch on.
        let virtual_title = doc.virtual_source.as_ref().map(|v| v.title.clone());
        let read_only = doc.read_only();
        let is_patch = doc.patch().is_some();
        let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(doc, jt));
        let clamped_anchor = params
            .jump_to_anchor
            .map(|a| motion::clamp_position(doc, a));
        let cursor =
            resolve_open_cursor(&mut s, client_id, buffer_id, clamped_jump, clamped_anchor);
        let view_id = presented_view(&mut s, client_id, buffer_id, &params);
        let scroll = open_scroll(&s, client_id, view_id, params.jump_to);
        let mut pushes = pin_view_if_requested(&mut s, view_id, params.transient);
        let result = ViewOpenResult {
            view_id,
            scroll,
            transient: s.view(view_id).transient,
            buffer: BufferDescription {
                buffer_id,
                language,
                line_count,
                byte_count,
                revision,
                saved_revision,
                path,
                scratch_number,
                cursor,
                lsp_server: buffer_lsp_server_ref(&s, buffer_id),
                title: virtual_title,
                read_only,
                is_patch,
            },
        };
        if intent == OpenIntent::Navigate {
            s.touch_mru(buffer_id);
            pushes.extend(refresh_view_pickers(&mut s));
        }
        drop(s);
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        return Ok(result);
    }

    let canonical = if let Some(abs) = params.absolute_path.clone() {
        // Absolute-path open (workspace/open_path, goto-definition). Resolved directly rather than
        // against a workspace root, and — unlike root-relative opens — allowed to land outside the
        // active workspace's roots. The boundary check below is skipped for this route; the buffer
        // is simply marked external.
        let raw = crate::config::expand_home(std::path::Path::new(&abs));
        match std::fs::canonicalize(&raw) {
            Ok(p) => p,
            Err(_) if params.create_if_missing => canonicalize_partial(&raw).map_err(|e| {
                RpcError::invalid_path(format!("canonicalizing {}: {e}", raw.display()))
            })?,
            Err(e) => {
                return Err(RpcError::invalid_path(format!(
                    "canonicalizing {}: {e}",
                    raw.display()
                )));
            }
        }
    } else {
        match (params.path_index, params.relative_path.as_deref()) {
            (None, None) => {
                let mut s = state.lock().await;
                let id = s.allocate_buffer_id();
                let doc_id = s.allocate_document_id();
                let scratch_number = s.next_scratch_number(&active_workspace_name);
                let doc = Document::scratch(doc_id, params.language.clone());
                let buf = Buffer {
                    id,
                    document: doc_id,
                    scratch_number: Some(scratch_number),
                };
                let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(&doc, jt));
                let clamped_anchor = params
                    .jump_to_anchor
                    .map(|a| motion::clamp_position(&doc, a));
                let cursor =
                    resolve_open_cursor(&mut s, client_id, id, clamped_jump, clamped_anchor);
                let mut result = ViewOpenResult {
                    view_id: ViewId::default(), // minted below, once the buffer exists
                    scroll: None, // a fresh buffer has no view to have been scrolled in
                    transient: false, // the view's, read below once it exists
                    buffer: BufferDescription {
                        buffer_id: id,
                        language: doc.language.clone(),
                        line_count: doc.line_count(),
                        byte_count: doc.byte_count(),
                        revision: 0,
                        saved_revision: doc.saved_revision(),
                        path: None,
                        scratch_number: Some(scratch_number),
                        cursor,
                        lsp_server: None, // scratch buffers are never language-server-backed
                        title: None,
                        read_only: false,
                        is_patch: false,
                    },
                };
                s.documents.insert(doc_id, doc);
                s.buffers.insert(id, buf);
                result.view_id = presented_view(&mut s, client_id, id, &params);
                result.transient = s.view(result.view_id).transient;
                s.buffer_workspaces
                    .insert(id, active_workspace_name.clone());
                let pushes = match intent {
                    OpenIntent::Navigate => {
                        s.touch_mru(id);
                        refresh_view_pickers(&mut s)
                    }
                    OpenIntent::Bind => Vec::new(),
                };
                drop(s);
                for (sender, notif) in pushes {
                    let _ = sender.send(notif).await;
                }
                return Ok(result);
            }
            (Some(idx), rel) => {
                let s = state.lock().await;
                let base = s
                    .active_workspace_or_err(ctx.client_id)?
                    .paths
                    .get(idx as usize)
                    .ok_or_else(|| {
                        RpcError::invalid_path(format!("path_index {idx} out of range"))
                    })?
                    .clone();
                drop(s);
                let candidate = match rel {
                    None | Some("") => base.clone(),
                    Some(r) => base.join(r),
                };
                // Resolve to a canonical-shaped path. When the target file already exists,
                // straight canonicalize. When `create_if_missing` is set and the file (or even
                // some of its parents — multi-segment paths like `foo/bar/baz.rs`) doesn't
                // exist, walk up to the deepest existing ancestor via `canonicalize_partial`
                // and re-attach the not-yet-existing tail. The file (and any missing parents)
                // is written to disk at the first save; the boundary check below runs against
                // the resolved path either way.
                match std::fs::canonicalize(&candidate) {
                    Ok(p) => p,
                    Err(_) if params.create_if_missing => canonicalize_partial(&candidate)
                        .map_err(|e| {
                            RpcError::invalid_path(format!(
                                "canonicalizing {}: {e}",
                                candidate.display()
                            ))
                        })?,
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
        }
    };

    {
        let mut s = state.lock().await;
        // Root-relative opens are confined to the workspace boundary (blocks `../` traversal).
        // Absolute-path opens (`absolute_path`) are deliberately allowed outside the roots — they
        // become external buffers — so the boundary check only applies to the relative route.
        if params.absolute_path.is_none()
            && !s
                .active_workspace_or_err(ctx.client_id)?
                .contains(&canonical)
        {
            return Err(RpcError::invalid_path(format!(
                "{} is outside the workspace's access boundary",
                canonical.display()
            )));
        }
        if let Some(existing) = s.buffer_for_path_in_workspace(&active_workspace_name, &canonical) {
            let doc = s.doc_of(existing);
            let language = doc.language.clone();
            let line_count = doc.line_count();
            let byte_count = doc.byte_count();
            let revision = doc.revision;
            let saved_revision = doc.saved_revision();
            let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(doc, jt));
            let clamped_anchor = params
                .jump_to_anchor
                .map(|a| motion::clamp_position(doc, a));
            let cursor =
                resolve_open_cursor(&mut s, client_id, existing, clamped_jump, clamped_anchor);
            let cursor = match client_id {
                Some(c) => wrap_for_response(&s, c, existing, cursor),
                None => cursor,
            };
            let view_id = presented_view(&mut s, client_id, existing, &params);
            let scroll = open_scroll(&s, client_id, view_id, params.jump_to);
            let mut pushes = pin_view_if_requested(&mut s, view_id, params.transient);
            let result = ViewOpenResult {
                view_id,
                scroll,
                transient: s.view(view_id).transient,
                buffer: BufferDescription {
                    buffer_id: existing,
                    language,
                    line_count,
                    byte_count,
                    revision,
                    saved_revision,
                    path: Some(canonical.display().to_string()),
                    scratch_number: None,
                    cursor,
                    lsp_server: buffer_lsp_server_ref(&s, existing),
                    title: None,
                    read_only: false,
                    is_patch: false,
                },
            };
            s.touch_mru(existing);
            pushes.extend(refresh_view_pickers(&mut s));
            drop(s);
            for (sender, notif) in pushes {
                let _ = sender.send(notif).await;
            }
            return Ok(result);
        }
    }

    let mut s = state.lock().await;
    let id = s.allocate_buffer_id();
    // Cross-workspace sharing: if any workspace already holds this file live, attach a new buffer
    // to the *same* document rather than loading a second copy — both workspaces then see the same
    // pending changes (rope, undo history, dirty state). The workspace-scoped dedup above didn't
    // hit, so this buffer is this workspace's first view of it. Recover-on-open is skipped when
    // attaching: the live document *is* the authoritative unsaved content.
    let doc_id = match s.document_for_path(&canonical) {
        Some(doc_id) => doc_id,
        None => {
            // Recover-on-open: unsaved content for this path may survive as a backup (hot-exit
            // restore, or a crash that beat the session write). File backups are document-level
            // (`files/<hash>`, no workspace in the key), so the content comes back no matter
            // which workspace — even an ephemeral one — opens the path first.
            let backup = s.backups_path.as_deref().and_then(|root| {
                crate::backup::read(&crate::backup::file_backup_path(root, &canonical))
            });
            let doc_id = s.allocate_document_id();
            let mut doc = if params.create_if_missing && !canonical.exists() {
                // New file: empty document with the target path attached. Save will write to disk.
                Document::new_at_path(doc_id, canonical.clone(), params.language.clone())
            } else {
                match Document::load_from_file(doc_id, canonical.clone()) {
                    Ok(d) => d,
                    // The file is gone but we still hold unsaved content for it — recover from the
                    // backup and flag it externally-deleted rather than failing the open.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && backup.is_some() => {
                        let mut d = Document::new_at_path(
                            doc_id,
                            canonical.clone(),
                            params.language.clone(),
                        );
                        d.externally_deleted = true;
                        d
                    }
                    Err(e) => return Err(RpcError::file_io(e)),
                }
            };
            // Overlay the backup: the disk content loaded above becomes the saved baseline; the
            // backup becomes the live (dirty) text. A source file newer than the backup means it
            // changed externally while we were down — surfaced via the same `externally_modified`
            // flag as an in-session collision.
            if let Some((content, backup_mtime)) = &backup {
                doc.restore_unsaved(content);
                doc.backed_up_revision = Some(doc.revision);
                if doc
                    .last_modified_unix_ms
                    .is_some_and(|disk| disk > *backup_mtime)
                {
                    doc.externally_modified = true;
                }
            }
            s.documents.insert(doc_id, doc);
            doc_id
        }
    };
    let buf = Buffer {
        id,
        document: doc_id,
        scratch_number: None,
    };
    let doc = &s.documents[&doc_id];
    let clamped_jump = params.jump_to.map(|jt| motion::clamp_position(doc, jt));
    let clamped_anchor = params
        .jump_to_anchor
        .map(|a| motion::clamp_position(doc, a));
    // First-time open of this buffer: no prior cursor or scroll to surface — but the client could
    // already have one if a previous server-side session allocated state. Look it up anyway for
    // consistency with the reopen path.
    let cursor = resolve_open_cursor(&mut s, client_id, id, clamped_jump, clamped_anchor);
    // External buffers (path outside the active workspace's roots) are guests: no language server
    // (see the trust reasoning at `lsp_launch` below).
    let external = !s
        .workspaces
        .get(&active_workspace_name)
        .is_some_and(|p| p.contains(&canonical));
    // Git eligibility is the *wider* test ([`WorkspaceEntry::git_eligible`]): containment, or
    // inside the working tree of a repo a root reaches. The two deliberately differ — a file in
    // your own repo should show its diff and be stageable however you reached it, while attaching
    // a language server to it is an act of trust that containment, not the repo, still governs.
    // Skipping the baseline for a true external also avoids repo discovery walking up out of the
    // workspace tree.
    let git_external = !git_baseline_eligible(&s, &active_workspace_name, &canonical);
    // Resolve the Git baseline once (repo discovery + reading the committed blob) and diff the
    // buffer against it, so git-aware views have hunks from the first frame and later edits can
    // re-diff cheaply without touching the repo. Best-effort; untracked / no-repo → empty.
    // Only *small* files resolve it on the open round-trip: blob decompression and the two
    // full-file diffs are O(file), so past the limit the load runs in a background task
    // ([`finish_git_baseline`]) and the gutter fills in via a `viewport/lines_changed` push —
    // the same deal the deferred parse gets. Computed per *buffer* even for a shared document:
    // git is workspace-scoped, so each attachment resolves its own baseline.
    let doc = &s.documents[&doc_id];
    let git_deferred = !git_external && doc.byte_count() > GIT_BASELINE_SYNC_LIMIT_BYTES;
    let git = (!git_external).then(|| {
        // The repo *identity* is resolved however big the file is: which repo a file is in is a
        // fact about the workspace, and every git verb resolves its repo through it. Only the
        // O(file) content half defers — see `GitBaseline::content`.
        if git_deferred {
            let git_baseline = crate::git::load_repo_identity(&canonical, &s.git_baseline_choices);
            return (git_baseline, Vec::new(), Vec::new());
        }
        let git_baseline = crate::git::load_baseline(&canonical, &s.git_baseline_choices);
        let content = git_baseline
            .content()
            .expect("loaded, not deferred")
            .clone();
        let git_unstaged = crate::git::diff_hunks(content.index_blob.as_deref(), &doc.text);
        let git_both = crate::git::compose_both(&content.staged_hunks, &git_unstaged);
        (git_baseline, git_unstaged, git_both)
    });
    let syntax_pending = doc.syntax_pending;
    let syntax_token = syntax_pending.then(|| s.deferred.start());
    s.buffers.insert(id, buf);
    // The buffer's view is minted below with the open's intent (`presented_view`), once the
    // document is registered — nothing between here and there asks for it.
    s.buffer_workspaces
        .insert(id, active_workspace_name.clone());
    // Enforce the dormant invariant at materialization: a path now has a live buffer, so drop any
    // dormant (session-restored) entry for it. The by-id open route already does this via
    // `take_dormant`, but path opens (file picker, grep, goto-def, absolute path) reach a dormant
    // file without ever touching its reserved id — without this they'd leave the dormant twin behind,
    // which the picker only papers over by hiding, and a later close would un-hide. No-op when no
    // dormant entry matches.
    let dormant_kinds = s.promote_dormant(&active_workspace_name, &canonical);
    if let Some((git_baseline, git_unstaged, git_both)) = git {
        s.git_baseline.insert(id, git_baseline);
        // Mask rather than recompute: the two diffs were run off the lock (the point of doing them
        // there), and the conflict scan only costs anything on a file the index calls conflicted.
        recompute_conflicts(&mut s, id);
        let (git_unstaged, git_both) = mask_hunks_against_conflicts(&s, id, git_unstaged, git_both);
        s.git_unstaged_hunks.insert(id, git_unstaged);
        s.git_both_hunks.insert(id, git_both);
    }

    // LSP, for *internal* files in a *trusted* workspace. Discover a workspace root within it and
    // ensure a server keyed to that workspace + root, launching one if needed. `ensure` returns a
    // launch request when it created a fresh (Starting) handle; we spawn the handshake after
    // releasing the lock. `notify_open` is a no-op until the server is ready — the launch task opens
    // every registered buffer once the handshake lands.
    //
    // Two exclusions, for the same reason from different directions:
    //
    // - **External files** (outside every root) get no server: we never launch one in untrusted
    //   territory, and we don't attach them to another workspace's running server either — that
    //   would put two buffers (this guest + the owning workspace's) on one server for the same URI,
    //   the very ambiguity per-workspace keying exists to avoid.
    // - **Temporary workspaces** get no server *even though their files are now inside a root*. That
    //   root is synthesized from the file's own directory to make the pickers work
    //   (`adopt_ephemeral_root`), and it must not be read as an act of trust: `ae /tmp/whatever.rs`
    //   is precisely the case where you don't want a language server executing over content you
    //   haven't vouched for. Trust follows the workspace you configured, not the file you opened —
    //   which is why this asks the workspace rather than deriving it from containment, as the
    //   external check does. (Git is deliberately *not* excluded: reading a repo's index is not the
    //   same risk as launching a binary, and diff/blame on a file you opened in passing is useful.)
    let untrusted_workspace = s
        .workspaces
        .get(&active_workspace_name)
        .is_some_and(|w| w.is_ephemeral());
    let mut lsp_launch: Option<(
        crate::lsp::manager::LspServerKey,
        crate::lsp::config::LspServerSpec,
        u64,
    )> = None;
    if !external && !untrusted_workspace {
        if let Some(language) = s.doc_of(id).language.clone() {
            if let Some(spec) = crate::lsp::config::server_spec(&language) {
                let roots = s
                    .workspaces
                    .get(&active_workspace_name)
                    .map(|p| p.paths.clone())
                    .unwrap_or_default();
                let root = crate::lsp::manager::discover_root(
                    &canonical,
                    spec.root_markers,
                    crate::lsp::config::workspace_marker(&language),
                    &roots,
                );
                let key = crate::lsp::manager::LspServerKey::new(root, &language);
                if let Some(generation) = s.lsp.ensure(&key, spec.command) {
                    lsp_launch = Some((key.clone(), spec, generation));
                }
                s.lsp.register_doc(id, &key);
                let uri = crate::lsp::uri::path_to_uri(&canonical);
                let text = s.doc_of(id).text.to_string();
                let version = s.doc_of(id).revision as i64;
                let document = s.buffers[&id].document;
                s.lsp
                    .notify_open(id, document, &key, &uri, &language, version, &text);
            }
        }
    }

    let cursor = match client_id {
        Some(c) => wrap_for_response(&s, c, id, cursor),
        None => cursor,
    };
    let view_id = presented_view(&mut s, client_id, id, &params);
    // The views its dormant rows stood for — a kept reader beside the editor — come back kept.
    s.restore_dormant_views(id, dormant_kinds);
    let doc = s.doc_of(id);
    let scroll = open_scroll(&s, client_id, view_id, params.jump_to);
    let result = ViewOpenResult {
        view_id,
        scroll,
        transient: s.view(view_id).transient,
        buffer: BufferDescription {
            buffer_id: id,
            language: doc.language.clone(),
            line_count: doc.line_count(),
            byte_count: doc.byte_count(),
            revision: doc.revision,
            saved_revision: doc.saved_revision(),
            path: Some(canonical.display().to_string()),
            scratch_number: None,
            cursor,
            lsp_server: buffer_lsp_server_ref(&s, id),
            title: None,
            read_only: false,
            is_patch: false,
        },
    };
    let mut pushes = match intent {
        OpenIntent::Navigate => {
            s.touch_mru(id);
            refresh_view_pickers(&mut s)
        }
        OpenIntent::Bind => Vec::new(),
    };
    // A restored buffer can come back externally-modified/-deleted (the source changed or vanished
    // while we were down). That state rides a `buffer/state` push, not the open result, so emit one
    // now — otherwise the client wouldn't learn until some later edit triggered a push.
    if s.try_doc_of(id)
        .is_some_and(|d| d.externally_modified || d.externally_deleted)
    {
        pushes.extend(collect_buffer_state_pushes(&s, id));
    }
    let watcher = s.watcher.clone();
    drop(s);
    // Make sure this buffer's directory carries a watch: the workspace-root registration is
    // ignore-aware, so a file opened from inside a gitignored tree (or outside the roots
    // entirely) wouldn't otherwise get external-change events. Idempotent — the common
    // in-workspace open finds its parent already watched.
    if let Some(w) = watcher {
        crate::watcher::watch_buffer_parent(&w, &canonical);
    }
    if let Some((key, spec, generation)) = lsp_launch {
        tokio::spawn(crate::lsp::manager::launch(
            state.clone(),
            key,
            spec,
            generation,
        ));
    }
    // Warm the document-symbol outline so `o` and `Space o` work as soon as possible. A no-op if
    // the server isn't ready yet — the `publishDiagnostics` hook refreshes once it is.
    spawn_document_symbol_refresh(state.clone(), id);
    if let Some(token) = syntax_token {
        tokio::spawn(finish_pending_parse(state.clone(), id, token));
    }
    if git_deferred {
        tokio::spawn(finish_git_baseline(state.clone(), id, canonical.clone()));
    }
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::debug!(buffer_id = id, path = %canonical.display(), "buffer opened");
    Ok(result)
}

/// Whether this path gets a Git baseline at all in `workspace`. One definition, so the on-demand
/// load ([`ensure_git_baseline`]) can never disagree with the open path about which files have one
/// — a disagreement there would show up as a git verb refusing a file whose gutter works.
fn git_baseline_eligible(s: &ServerState, workspace: &str, canonical: &std::path::Path) -> bool {
    s.workspaces
        .get(workspace)
        .is_some_and(|p| p.git_eligible(canonical))
}

/// Finish a buffer's Git baseline **now** if its content half is still loading.
///
/// A large file's blobs load off the open path ([`finish_git_baseline`]), and an apply needs them:
/// it is a one-shot answer to a keypress, unlike the gutter, which simply fills in when the load
/// lands. Staging from the working-changes view reaches this every time rather than occasionally —
/// that path opens the file and applies to it in the same breath, so there is no interval for the
/// background load to have finished in.
///
/// Only ever *completes* a baseline: a buffer with no entry at all has none by design (pathless, or
/// outside the workspace's git-eligible tree), and minting one here would quietly make a file
/// stageable that the open path refused.
pub async fn ensure_git_baseline(state: &SharedState, buffer_id: BufferId) {
    let (path, revs) = {
        let s = state.lock().await;
        if !s
            .git_baseline
            .get(&buffer_id)
            .is_some_and(crate::git::GitBaseline::is_pending)
        {
            return;
        }
        let Some(path) = s
            .try_doc_of(buffer_id)
            .and_then(|d| d.canonical_path.clone())
        else {
            return;
        };
        (path, s.git_baseline_choices.clone())
    };
    let Ok(baseline) =
        tokio::task::spawn_blocking(move || crate::git::load_baseline(&path, &revs)).await
    else {
        return;
    };
    let pushes = {
        let mut s = state.lock().await;
        if !s.buffers.contains_key(&buffer_id) {
            return; // closed while the baseline was loading
        }
        attach_git_baseline(&mut s, buffer_id, baseline)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// Files at or below this size resolve their Git baseline synchronously inside `view/open`, so
/// the first frame already carries gutter markers and branch status; larger files defer to
/// [`finish_git_baseline`]. 128 KB costs single-digit milliseconds (two blob decompressions plus
/// two in-memory diffs) — the same order as the deferred-parse limit in `state::sync_parse_affordable`.
const GIT_BASELINE_SYNC_LIMIT_BYTES: u64 = 128 * 1024;

/// Complete a deferred Git baseline load: repo discovery and blob reads run on a blocking thread
/// with no locks held, then the result attaches under the lock via [`attach_git_baseline`] — which
/// diffs against the buffer's *current* text, so edits that landed while the blobs were loading
/// are already accounted for. Every viewport on the buffer then gets a `viewport/lines_changed`
/// push carrying the freshly-known hunks and branch status.
async fn finish_git_baseline(
    state: SharedState,
    buffer_id: BufferId,
    canonical: std::path::PathBuf,
) {
    // Snapshotted rather than read inside the blocking task, which holds no lock. A
    // `git/set_baseline` landing mid-load re-resolves every buffer in the repo anyway, so a
    // snapshot that's one revision stale is corrected moments later rather than left wrong.
    let revs = state.lock().await.git_baseline_choices.clone();
    let Ok(baseline) =
        tokio::task::spawn_blocking(move || crate::git::load_baseline(&canonical, &revs)).await
    else {
        return;
    };
    let pushes = {
        let mut s = state.lock().await;
        if !s.buffers.contains_key(&buffer_id) {
            return; // closed while the baseline was loading
        }
        attach_git_baseline(&mut s, buffer_id, baseline)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    tracing::debug!(buffer_id, "deferred git baseline attached");
}

/// Complete a deferred open parse (`Buffer::syntax_pending`): snapshot the rope (cheap — ropey
/// clones share chunks), run the full tree-sitter parse on a blocking thread, and attach the
/// result under the lock — but only if the buffer is still at the snapshotted revision. Edits that
/// landed mid-parse make the tree stale, so the loop re-snapshots and parses again; it ends when a
/// parse survives unchallenged or the buffer is gone. On attach, every viewport on the buffer
/// (any client) gets a `viewport/lines_changed` re-render, restyling the unhighlighted first frame.
/// `_token` keeps the server's [`crate::state::Deferred`] count raised for the life of the parse,
/// so `wait_quiet` covers it. Without it a caller waiting for the server to go quiet could observe
/// a buffer *before* its tree lands — which is exactly what a test asserting on highlighting does,
/// and it fails only under load, which is the worst way to find out.
async fn finish_pending_parse(
    state: SharedState,
    buffer_id: BufferId,
    _token: crate::state::DeferredToken,
) {
    loop {
        let (text, revision, language) = {
            let mut s = state.lock().await;
            let Some(buf) = s.try_doc_of_mut(buffer_id) else {
                return; // closed while pending
            };
            if !buf.syntax_pending {
                return; // superseded (e.g. a reload already attached a tree)
            }
            let Some(language) = buf.language.clone() else {
                // Shouldn't happen — pending is only set for detected languages — but don't
                // leave the flag stuck if it does.
                buf.syntax_pending = false;
                return;
            };
            (buf.text.clone(), buf.revision, language)
        };
        let parsed =
            tokio::task::spawn_blocking(move || crate::state::make_syntax(&text, &language))
                .await
                .unwrap_or(None);
        let pushes = {
            let mut s = state.lock().await;
            let Some(buf) = s.try_doc_of_mut(buffer_id) else {
                return;
            };
            if !buf.syntax_pending {
                return;
            }
            if buf.revision != revision {
                continue; // buffer changed mid-parse — the tree is stale, go again
            }
            buf.syntax = parsed;
            buf.syntax_pending = false;
            // The tree belongs to the shared document — restyle every attached buffer's
            // viewports, not just the one this task was spawned for.
            let mut pushes = PendingPushes::new();
            for id in s.doc_siblings(buffer_id) {
                pushes.extend(collect_buffer_refresh_pushes(&s, id));
            }
            pushes
        };
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        tracing::debug!(buffer_id, "deferred parse attached");
        return;
    }
}

/// One `viewport/lines_changed` per viewport — any client — showing `buffer_id`, re-rendering each
/// viewport's currently pushed range. For server-side changes that restyle a window without a
/// content edit: a deferred open parse landing, a Git baseline attaching or refreshing. The
/// buffer's current revision rides through unchanged.
pub fn collect_buffer_refresh_pushes(s: &ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut pushes: PendingPushes = Vec::new();
    if s.try_doc_of(buffer_id).is_none() {
        return pushes;
    }
    for vp in s.viewports.values() {
        if !vp.shows(s.view_of(vp), buffer_id) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let notif =
            build_lines_changed_notif(s, vp, lines_changed_cursor(s, vp), SneakLabels::Hidden);
        pushes.push((sender, notif));
    }
    pushes
}

#[cfg(test)]
mod next_buffer_tests {
    use super::*;

    /// Build a state with workspace "p" active for one client; returns the state and client id.
    fn state_with_active_workspace() -> (ServerState, ClientId) {
        let mut st = ServerState::new();
        let root = std::path::PathBuf::from("/p");
        st.workspaces.insert(
            "p".to_string(),
            crate::state::WorkspaceEntry {
                worktrees: Default::default(),
                id: "p".to_string(),
                name: Some("p".to_string()),
                base_paths: None,
                paths: vec![root.clone()],
                workspace_index: std::sync::Arc::new(crate::workspace_index::WorkspaceIndex::new(
                    vec![root],
                )),
                mru_views: std::collections::VecDeque::new(),
                dormant_views: Vec::new(),
                jumplist: None,
                projects: Vec::new(),
            },
        );
        let client_id = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        std::mem::forget(_rx);
        st.clients.insert(
            client_id,
            crate::state::ClientSession {
                client_id,
                outbound: tx,
                pushes_written: Default::default(),
                active_workspace: Some("p".to_string()),
            },
        );
        (st, client_id)
    }

    /// After a session restore, closing the last live view should land on the most-recent dormant
    /// row's view (which `view/open` materializes) rather than returning `None` — which is what
    /// makes the client spawn a blank scratch.
    #[test]
    fn next_view_falls_back_to_dormant_before_scratch() {
        let (mut st, client_id) = state_with_active_workspace();
        // No live buffers; two dormant ones restored from the session (front = most-recent).
        let d1 = st.allocate_buffer_id();
        let d2 = st.allocate_buffer_id();
        st.workspaces.get_mut("p").unwrap().dormant_views = vec![
            crate::state::DormantView {
                id: d1,
                view: ViewId(d1),
                kind: None,
                source: crate::state::DormantSource::File(std::path::PathBuf::from("/p/a.rs")),
            },
            crate::state::DormantView {
                id: d2,
                view: ViewId(d2),
                kind: None,
                source: crate::state::DormantSource::File(std::path::PathBuf::from("/p/b.rs")),
            },
        ];
        assert_eq!(next_view_for_client(&st, client_id), Some(ViewId(d1)));

        // A live view still wins over the dormant fallback.
        let live = st.allocate_buffer_id();
        st.insert_buffer_with_document(live, None, false, |d| {
            Document::new_at_path(d, std::path::PathBuf::from("/p/live.rs"), None)
        });
        st.buffer_workspaces.insert(live, "p".to_string());
        let live_view = st.open_view(live);
        st.touch_mru(live);
        assert_eq!(next_view_for_client(&st, client_id), Some(live_view));
    }

    /// With neither live nor dormant views, `None` falls through so the caller opens a scratch.
    #[test]
    fn next_view_is_none_when_workspace_is_empty() {
        let (st, client_id) = state_with_active_workspace();
        assert_eq!(next_view_for_client(&st, client_id), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with `src/main.rs` committed twice, returning both revisions oldest-first.
    fn repo_with_two_revisions(root: &std::path::Path) -> (git2::Repository, String, String) {
        let repo = git2::Repository::init(root).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let mut revs = Vec::new();
        for body in ["fn main() {}\n", "fn main() { changed(); }\n"] {
            std::fs::write(root.join("src/main.rs"), body).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("src/main.rs")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .into_iter()
                .collect();
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            let oid = repo
                .commit(Some("HEAD"), &sig, &sig, "c", &tree, &parent_refs)
                .unwrap();
            revs.push(oid.to_string());
        }
        let (a, b) = (revs[0].clone(), revs[1].clone());
        (repo, a, b)
    }

    /// A client with an activated (ephemeral) workspace — the minimum for a handler to run.
    fn state_with_client() -> (SharedState, ConnectionCtx) {
        let mut s = ServerState::new();
        let workspace = s.register_ephemeral_workspace();
        let client_id = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        s.clients.insert(
            client_id,
            crate::state::ClientSession {
                client_id,
                outbound: tx,
                pushes_written: Default::default(),
                active_workspace: Some(workspace),
            },
        );
        (
            std::sync::Arc::new(tokio::sync::Mutex::new(s)),
            ConnectionCtx { client_id },
        )
    }

    /// A file at a revision resolves to one buffer per `(repo, rev, path)`, reused on re-resolution.
    ///
    /// This is what lets a view window a file it does not own. Reuse is not an optimisation here: a
    /// patch resolves the same file once per hunk, so without it a forty-hunk diff would mint forty
    /// copies of the same blob.
    #[tokio::test]
    async fn a_file_at_a_revision_resolves_to_one_reused_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_repo, first, second) = repo_with_two_revisions(&root);
        let repo_id = root.to_string_lossy().into_owned();
        let (state, mut ctx) = state_with_client();

        let a = buffer_for_file_at_rev(&state, &mut ctx, &repo_id, &first, "src/main.rs")
            .await
            .expect("the first revision resolves");
        let again = buffer_for_file_at_rev(&state, &mut ctx, &repo_id, &first, "src/main.rs")
            .await
            .expect("re-resolving the same target");
        assert_eq!(a, again, "the same (repo, rev, path) is the same buffer");

        let b = buffer_for_file_at_rev(&state, &mut ctx, &repo_id, &second, "src/main.rs")
            .await
            .expect("the second revision resolves");
        assert_ne!(a, b, "a different revision is a different buffer");

        let s = state.lock().await;
        assert_eq!(
            s.doc_of(a).text.to_string(),
            "fn main() {}\n",
            "the buffer holds that revision's content"
        );
        assert_eq!(s.doc_of(b).text.to_string(), "fn main() { changed(); }\n");
    }

    /// A buffer a view binds defers its parse, whatever its size.
    ///
    /// The measurement behind this: over the 40 largest files in this repo, reading costs 6.6 ms
    /// and parsing costs 590 ms. A patch that parsed each file as it bound it would stall for most
    /// of a second before showing anything — even though every individual file is small enough that
    /// the size-based rule would happily parse it inline, which is why size alone is not the test.
    #[tokio::test]
    async fn a_bound_buffer_defers_its_parse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_repo, first, _second) = repo_with_two_revisions(&root);
        let repo_id = root.to_string_lossy().into_owned();
        let (state, mut ctx) = state_with_client();

        let id = buffer_for_file_at_rev(&state, &mut ctx, &repo_id, &first, "src/main.rs")
            .await
            .expect("resolves");

        let s = state.lock().await;
        let doc = s.doc_of(id);
        assert_eq!(
            doc.language.as_deref(),
            Some("rust"),
            "the language is known, so an inline parse was affordable and was skipped anyway"
        );
        assert!(
            doc.syntax_pending,
            "a bound buffer hands its parse to the background"
        );
        assert!(
            doc.syntax.is_none(),
            "and has no tree yet, so binding forty costs forty reads, not forty parses"
        );
    }

    /// Resolving a file for a *view* is not a navigation: it must not touch the MRU list.
    ///
    /// A forty-file patch resolves forty buffers. If each counted as somewhere the user had been,
    /// the recent-buffers list — and the picker built on it — would be forty files they never
    /// opened, burying the ones they did.
    #[tokio::test]
    async fn resolving_a_file_for_a_view_does_not_touch_the_mru() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_repo, first, _second) = repo_with_two_revisions(&root);
        let repo_id = root.to_string_lossy().into_owned();
        let (state, mut ctx) = state_with_client();

        let client_id = ctx.client_id;
        let mru_len = |state: SharedState| async move {
            let s = state.lock().await;
            let workspace = s.active_workspace(client_id).unwrap().id.clone();
            s.workspaces[&workspace].mru_views.len()
        };
        assert_eq!(mru_len(state.clone()).await, 0, "nothing opened yet");

        buffer_for_file_at_rev(&state, &mut ctx, &repo_id, &first, "src/main.rs")
            .await
            .expect("resolves");
        assert_eq!(
            mru_len(state.clone()).await,
            0,
            "a buffer a view bound is not a buffer the user visited"
        );
    }
}
