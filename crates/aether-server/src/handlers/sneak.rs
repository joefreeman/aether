//! `sneak/*` — the `s`/`S` word-jump: candidate collection, label assignment, and selection.

use super::*;

/// `sneak/update` — set or refine the sneak query. Recomputes the matching word-starts within the
/// named viewport's visible range, (re)assigns labels (keeping survivors' labels stable), stores
/// the session, and pushes refreshed viewport renders carrying the labels. Returns the live label
/// set so the client can tell a label keystroke (jump) from a refinement keystroke (narrow).
pub async fn sneak_update(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SneakUpdateParams,
) -> Result<SneakUpdateResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let scope = s.motion_scope(client_id, params.buffer_id)?;
    let buf = scope.doc();
    let key = (client_id, params.buffer_id);

    // Scope to the client-reported visible range, intersected with the focused element's window.
    // The client's range says what is on screen — the viewport's own carries a screen of overscan
    // and the native clients pixel-scroll within it — and the element says which of that the cursor
    // may go to: a label on a line of the hunk *below* would jump into another element's lines
    // without focus following, which is the one thing a jump must not do silently.
    let first = params
        .first_line
        .clamp(scope.first_line(), scope.last_line());
    let last_excl = params
        .last_line
        .clamp(first, scope.last_line().saturating_add(1));

    let raw = crate::sneak::compute_candidates(
        &buf.text,
        first as usize,
        last_excl as usize,
        &params.query,
        params.big,
    );
    let query_char_len = params.query.chars().count();
    let labels = crate::sneak::assign_labels(&raw);

    let candidates: Vec<SneakCandidate> = raw
        .iter()
        .zip(&labels)
        .map(|(c, label)| SneakCandidate {
            start_char: c.start_char,
            start: motion::char_to_pos(buf, c.start_char),
            end_excl: motion::char_to_pos(buf, c.end_char_excl),
            // Just past the typed prefix: the word starts with `query`, so the first
            // `query_char_len` chars are the matched prefix.
            prefix_end: motion::char_to_pos(buf, c.start_char + query_char_len),
            label: *label,
        })
        .collect();

    let match_count = candidates.len() as u32;
    let live_labels: Vec<char> = candidates.iter().filter_map(|c| c.label).collect();

    s.sneaks.insert(
        key,
        SneakEntry {
            query: params.query,
            viewport_id: params.viewport_id,
            candidates,
        },
    );
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(SneakUpdateResult {
        labels: live_labels,
        match_count,
    })
}

/// `sneak/select` — jump to the labelled word and select it (or extend the current selection to the
/// hull spanning it and the target word). Clears the session and refreshes the viewport. No-op
/// (returns the current cursor) when there's no session or the label is unknown.
pub async fn sneak_select(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SneakSelectParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let cursor = s.cursors.get(&key).copied().unwrap_or_default();

    let target = s
        .sneaks
        .get(&key)
        .and_then(|e| e.candidates.iter().find(|c| c.label == Some(params.label)))
        .copied();
    let Some(target) = target else {
        // Unknown label / no session: leave the cursor put.
        let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
        return Ok(cursor);
    };

    let tgt_lo = target.start_char;
    let tgt_hi = motion::pos_to_char(buf, target.end_excl)
        .saturating_sub(1)
        .max(tgt_lo);

    let (anchor_char, pos_char) = if params.extend {
        // Bounding hull of the current selection and the target word, so extend never shrinks
        // coverage. The head lands on the target side (its far edge); the anchor sits on the
        // opposite end of the hull.
        let cur_a = motion::pos_to_char(buf, cursor.anchor);
        let cur_p = motion::pos_to_char(buf, cursor.position);
        let cur_lo = cur_a.min(cur_p);
        let cur_hi = cur_a.max(cur_p);
        let lo = cur_lo.min(tgt_lo);
        let hi = cur_hi.max(tgt_hi);
        if tgt_lo < cur_lo {
            (hi, lo) // target reaches before the selection → head on the low (word) end
        } else {
            (lo, hi) // target at/after the selection → head on the high (word) end
        }
    } else {
        (tgt_lo, tgt_hi) // select just the word
    };

    let new_cursor = CursorState {
        position: motion::char_to_pos(buf, pos_char),
        anchor: motion::char_to_pos(buf, anchor_char),
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, new_cursor);
    s.record_motion(key, cursor, new_cursor);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    s.sneaks.remove(&key);

    let cursor = wrap_for_response(&s, client_id, params.buffer_id, new_cursor);
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(cursor)
}

/// `sneak/cancel` — abandon the session (the `Esc` path). The cursor never moved, so just drop the
/// labels and refresh.
pub async fn sneak_cancel(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SneakCancelParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    s.sneaks.remove(&(client_id, params.buffer_id));
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}

/// The single write path for a client's cursor on a buffer. Every handler that moves, edits,
/// clamps, or restores a cursor lands here, so the server can re-arm the cursor-following
/// decorations (blame label, symbol highlights) itself — clients never report cursor movement,
/// they only toggle following on mode/search transitions. The send is a no-op unless the pair
/// actually follows something (and in bare-state unit tests, where no follow loop runs).
pub fn set_cursor(s: &mut ServerState, key: (ClientId, BufferId), new: CursorState) {
    s.cursors.insert(key, new);
    // The status-bar breadcrumb follows unconditionally (there's no toggle for it), but only for a
    // buffer that actually has an outline — a plain-text buffer costs nothing here.
    let follows = s.blame_follow.contains(&key)
        || s.symbol_highlight_follow.contains(&key)
        || s.document_symbols.contains_key(&key.1);
    if follows {
        if let Some(tx) = &s.cursor_moved_tx {
            // Counted here — synchronously, under the caller's state lock — so the work is already
            // outstanding by the time the provoking RPC replies. Counting it in the follow loop
            // instead would leave a window where the server looks quiet but a refresh is coming.
            let token = s.deferred.start();
            let _ = tx.send((key.0, key.1, token));
        }
    }
}

/// Build one `viewport/lines_changed` notification per viewport owned by `client_id` that's
/// subscribed to `buffer_id`. Used to refresh highlights when a search is set or cleared.
pub fn collect_viewport_refresh(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> PendingPushes {
    let mut pushes = Vec::new();
    if s.try_doc_of(buffer_id).is_none() {
        return pushes;
    }
    for vp in s.viewports.values() {
        if vp.client_id != client_id || !s.view_of(vp).binds(buffer_id) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        // A search refresh carries the labels: unlike the post-edit broadcast, a sneak session is
        // exactly what may be live here.
        pushes.push((
            sender,
            build_lines_changed_notif(s, vp, lines_changed_cursor(s, vp), SneakLabels::Shown),
        ));
    }
    pushes
}

/// Build the `buffer/state` notification pushes for every client that has a viewport on this
/// buffer. Used by save, reload, and the file-watcher — mutations bump the buffer's `revision`
/// (which clients already learn from `viewport/lines_changed`) and the client derives `dirty`
/// as `revision != saved_revision`, so this notification is only needed when `saved_revision`
/// changes or when the external-change flags flip.
pub(crate) fn collect_buffer_state_pushes(s: &ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut pushes = Vec::new();
    // Fan out per attached buffer: the document state (saved revision, external flags, path) is
    // shared, but each sibling's viewers hear it under their own buffer id.
    for id in s.doc_siblings(buffer_id) {
        let Some(buf) = s.try_doc_of(id) else {
            continue;
        };
        let mut clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
        for vp in s.viewports.values() {
            if vp.shows(s.view_of(vp), id) {
                clients.insert(vp.client_id);
            }
        }
        let params = BufferStateParams {
            buffer_id: id,
            saved_revision: buf.saved_revision(),
            saved_at_unix_ms: buf.last_modified_unix_ms,
            externally_modified: buf.externally_modified,
            externally_deleted: buf.externally_deleted,
            // Lets a save-as rename follow to every other client viewing this shared buffer.
            path: buf.canonical_path.as_ref().map(|p| p.display().to_string()),
            language: buf.language.clone(),
        };
        let params = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
        pushes.extend(clients.into_iter().filter_map(|cid| {
            let session = s.clients.get(&cid)?;
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferState::NAME.into(),
                    params: params.clone(),
                },
            ))
        }));
    }
    pushes
}

/// Build the `view/state` pushes for every client presenting one of these views.
///
/// Transience is the view's, so the audience is the viewports on the view itself rather than
/// everyone showing its buffer: a file's reader is kept or dropped without touching its editor,
/// and a client watching the sibling has no business hearing the flag move.
pub(crate) fn collect_view_state_pushes(s: &ServerState, view_ids: &[ViewId]) -> PendingPushes {
    let mut pushes = Vec::new();
    for &view_id in view_ids {
        let Some(view) = s.views.get(&view_id) else {
            continue;
        };
        let params = serde_json::to_value(ViewStateParams {
            view_id,
            transient: view.transient,
        })
        .unwrap_or(serde_json::Value::Null);
        let clients: std::collections::HashSet<ClientId> = s
            .viewports
            .values()
            .filter(|vp| vp.view_id == view_id)
            .map(|vp| vp.client_id)
            .collect();
        pushes.extend(clients.into_iter().filter_map(|cid| {
            let session = s.clients.get(&cid)?;
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: ViewState::NAME.into(),
                    params: params.clone(),
                },
            ))
        }));
    }
    pushes
}

/// Promote the transient views an edit of `buffer_id` is a keep-signal for — see
/// [`ServerState::promote_views_of`]. Called from every buffer-mutation handler (the first edit
/// is what makes a previewed view worth keeping) and from `buffer/save`. Returns the `view/state`
/// pushes telling viewers the flag flipped; empty when nothing was transient (the common case) or
/// the buffer doesn't exist.
pub fn promote_transient(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let promoted = s.promote_views_of(buffer_id);
    collect_view_state_pushes(s, &promoted)
}

/// Apply a `view/open { transient }` intent to the view an open of an *existing* buffer
/// presents: `Some(false)` pins (promotes) it; `Some(true)` / `None` leave it alone — an open never
/// demotes a permanent view to transient. Returns the promotion's `view/state` pushes (usually
/// empty).
pub fn pin_view_if_requested(
    s: &mut ServerState,
    view_id: ViewId,
    transient: Option<bool>,
) -> PendingPushes {
    if transient != Some(false) || !s.set_view_transient(view_id, false) {
        return Vec::new();
    }
    collect_view_state_pushes(s, &[view_id])
}

/// The view a client should land on after its current one is closed: the top of its active
/// workspace's MRU, else any other open view in that workspace, else `None` — the caller's cue for
/// a transient scratch. Shared by `view/close` and the deletion paths so the requesting client and
/// any other clients that were viewing the buffer resolve their next view identically.
///
/// **Only ever an open view.** A dormant row — a file the session restored but nobody has looked
/// at yet, a shell or conversation whose view was closed — is something listed, not something
/// open, and a close never opens one. It used to: closing the last open view reopened the first
/// dormant row, so each `Space x` walked the session list, and a conversation (which a close keeps
/// as a dormant row, first in line) came straight back the moment its view closed.
///
/// A plain close asks the closing client's own navigation history first and reaches this only when
/// that trail has nothing to say (see `view_close`). This stays the answer to "is anything left
/// open in this workspace?" — a question about the workspace, not about where anyone has been.
/// Activation, which *does* resume onto a dormant row, asks its own question (`landing_view_id`).
pub fn next_view_for_client(s: &ServerState, client_id: ClientId) -> Option<ViewId> {
    let workspace_name = s.active_workspace(client_id).map(|p| p.id.clone());
    workspace_name
        .as_deref()
        .and_then(|name| s.mru_view(name))
        .or_else(|| {
            // A live view the MRU does not list — one restored kept beside a file the session
            // materialised, say — by recency, so the choice is the one the MRU would have made.
            workspace_name.as_deref().and_then(|name| {
                s.views
                    .iter()
                    .filter(|(_, v)| {
                        s.buffer_workspaces.get(&v.presenting).map(String::as_str) == Some(name)
                    })
                    .max_by_key(|(_, v)| v.last_used)
                    .map(|(id, _)| *id)
            })
        })
}

/// `(client, buffer)` pairs for every client *other than* `except` affected by closing
/// `buffer_ids`: clients with a viewport showing one (the push hands them a successor to switch
/// to), plus clients whose active workspace context holds one in its MRU — those may have it as
/// their *tether*, which must exit even while the client is viewing something else. Capture this
/// BEFORE tearing the buffers down — teardown drops the viewports and MRU entries this reads. At
/// most one entry per `(client, buffer)` pair; non-matching pushes are ignored client-side, so the
/// broad audience is safe.
///
/// A viewport is affected by everything it **shows** — the view it presents and every buffer its
/// elements window — not only by the focused element's buffer. Matching the focused buffer alone
/// left a client whose review windowed the closed file uninformed while its viewport was torn down
/// underneath it.
pub fn clients_affected_by_close(
    s: &ServerState,
    buffer_ids: &[BufferId],
    except: ClientId,
) -> Vec<AffectedByClose> {
    let mut seen: std::collections::HashSet<(ClientId, BufferId)> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    for vp in s.viewports.values() {
        if vp.client_id == except {
            continue;
        }
        for &id in buffer_ids {
            if vp.shows(s.view_of(vp), id) && seen.insert((vp.client_id, id)) {
                out.push(AffectedByClose {
                    client_id: vp.client_id,
                    view_id: vp.view_id,
                    buffer_id: id,
                });
            }
        }
    }
    let targets: std::collections::HashSet<BufferId> = buffer_ids.iter().copied().collect();
    for (&client_id, session) in &s.clients {
        if client_id == except {
            continue;
        }
        let Some(ws) = session
            .active_workspace
            .as_deref()
            .and_then(|id| s.workspaces.get(id))
        else {
            continue;
        };
        for (view_id, id) in ws
            .mru_views
            .iter()
            .filter_map(|v| Some((*v, s.try_presenting_buffer(*v)?)))
            .filter(|(_, id)| targets.contains(id))
        {
            if seen.insert((client_id, id)) {
                out.push(AffectedByClose {
                    client_id,
                    view_id,
                    buffer_id: id,
                });
            }
        }
    }
    out
}

/// One client a buffer close reaches, and the view it reached it through — the one its viewport
/// presented, or the workspace MRU entry a tether rides — so the `view/closed` push can name what
/// the client held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AffectedByClose {
    pub client_id: ClientId,
    pub view_id: ViewId,
    pub buffer_id: BufferId,
}

/// `workspace/changed` for every *other* client standing in `workspace_id`, carrying the shape it
/// now has.
///
/// The workspace is one server-side entity, so a change to its shape is a change for everyone in
/// it — but only the client that asked for it gets a `WorkspaceInfo` back in its RPC result. This
/// is that payload for everyone else.
///
/// Call it **after** the entry is mutated (it reads the live shape) and send after the lock drops.
/// Every handler that edits roots or projects owes this push; without it a second client keeps a
/// stale root list, and every path it renders is resolved against a shape the workspace no longer
/// has.
pub fn workspace_changed_pushes(
    s: &ServerState,
    workspace: &str,
    except: ClientId,
) -> PendingPushes {
    // Per **context**, not per workspace: the shape a client has to be told about is its own
    // context's — same configured roots, different materialisation — so two clients in two
    // worktrees of one repo each get their own roots rather than one of them getting the other's.
    contexts_of(s, workspace)
        .into_iter()
        .flat_map(|context| {
            let Some(entry) = s.workspaces.get(&context) else {
                return Vec::new();
            };
            let info = WorkspaceInfo {
                // The workspace's *name*, never the context id — the id is internal, and a client
                // that rendered it would show `aether/aether-3f9c=feature-auth` where it means
                // `aether`.
                name: entry.name.clone().unwrap_or_else(|| context.clone()),
                paths: entry
                    .paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                worktrees: wire_bindings(&entry.worktrees),
                projects: workspace_project_views(entry),
            };
            s.clients
                .iter()
                .filter(|(id, session)| {
                    **id != except && session.active_workspace.as_deref() == Some(context.as_str())
                })
                .map(|(_, session)| {
                    (
                        session.outbound.clone(),
                        Notification {
                            jsonrpc: JsonRpc,
                            method: aether_protocol::workspace::WorkspaceChanged::NAME.into(),
                            params: serde_json::to_value(&info).unwrap_or(serde_json::Value::Null),
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Build the `view/closed` pushes for the clients captured by [`clients_affected_by_close`],
/// telling each which buffer to switch to. Call AFTER teardown so each next-buffer reflects the
/// settled MRU. Clients that have since disconnected are skipped.
pub fn buffer_closed_pushes(s: &ServerState, affected: &[AffectedByClose]) -> PendingPushes {
    buffer_closed_pushes_with(s, affected, &Default::default(), &Default::default())
}

/// `path` as a root index + root-relative path in the client's active workspace, when it is inside
/// one of its roots. `None` for anything outside them — the caller then falls back to an id.
fn workspace_location_of(
    s: &ServerState,
    client_id: ClientId,
    path: &Path,
) -> Option<aether_protocol::buffer::BufferLocation> {
    let entry = s.active_workspace(client_id)?;
    entry.paths.iter().enumerate().find_map(|(i, root)| {
        path.strip_prefix(root)
            .ok()
            .map(|rel| aether_protocol::buffer::BufferLocation {
                path_index: i as u32,
                relative_path: rel.to_string_lossy().into_owned(),
            })
    })
}

/// [`buffer_closed_pushes`], with a per-buffer successor override given as a **path** and a
/// per-client landing taken from that client's own navigation history.
///
/// A worktree rebind closes each buffer and reopens it at the same *relative* path on the new tree,
/// so it knows something the generic rule can't: which file replaces which. Handed back as a path
/// rather than an id, because the id it could offer — a reserved dormant entry — is not stable:
/// the initiating client activates straight after the rebind, and a landing buffer on that same
/// file materialises the entry under a different id, leaving whoever opens second asking for one
/// that no longer exists. `view/open` on a path already open returns the existing buffer, so both
/// clients converge whichever order they arrive in.
///
/// A `landing` is the step back the receiving client's own trail would have taken (a plain close —
/// see `view_close`). The server cannot navigate on another client's behalf, so the push carries
/// what it can express: the view while it is still live, else the entry's path. An entry that is
/// only a virtual key names nothing this payload has a field for, so it falls through to the MRU
/// rule — as does a client with no trail.
pub fn buffer_closed_pushes_with(
    s: &ServerState,
    affected: &[AffectedByClose],
    successor: &std::collections::HashMap<BufferId, std::path::PathBuf>,
    landings: &std::collections::HashMap<ClientId, crate::state::NavEntry>,
) -> PendingPushes {
    affected
        .iter()
        .filter_map(
            |&AffectedByClose {
                 client_id,
                 view_id,
                 buffer_id,
             }| {
                let session = s.clients.get(&client_id)?;
                let landing = landings.get(&client_id);
                let landing_view = landing
                    .map(|e| e.view_id)
                    .filter(|id| s.views.contains_key(id));
                let landing_path = landing.filter(|_| landing_view.is_none()).and_then(|e| {
                    Some(aether_protocol::buffer::BufferLocation {
                        path_index: e.path_index?,
                        relative_path: e.relative_path.clone()?,
                    })
                });
                let next_path = successor
                    .get(&buffer_id)
                    .and_then(|path| workspace_location_of(s, client_id, path))
                    .or(landing_path);
                let params = ViewClosedParams {
                    view_id,
                    buffer_id: Some(buffer_id),
                    // Only as the fallback: a path wins when there is one.
                    next_view_id: next_path
                        .is_none()
                        .then(|| landing_view.or_else(|| next_view_for_client(s, client_id)))
                        .flatten(),
                    next_path,
                };
                Some((
                    session.outbound.clone(),
                    Notification {
                        jsonrpc: JsonRpc,
                        method: ViewClosed::NAME.into(),
                        params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
                    },
                ))
            },
        )
        .collect()
}
