//! `sneak/*` — the `s`/`S` word-jump: candidate collection, label assignment, and selection.

use super::*;
use aether_protocol::coords::ViewLine;

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

/// `search/step` — step `count` matches in `params.direction`, handling the composite params:
/// optional query revive first (skipping the step when it has no matches — same early-out the
/// clients used), then `count` steps.
pub async fn search_step(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchStepParams,
) -> Result<SearchNavResult, RpcError> {
    let direction = params.direction;
    if let Some(query) = params.set_query.clone() {
        let set = search_set(
            state,
            ctx,
            SearchSetParams {
                buffer_id: params.buffer_id,
                query,
                anchor: None,
                extend: false,
                from_selection: false,
                options: params.options,
            },
        )
        .await?;
        if set.summary.total == 0 {
            return Ok(SearchNavResult {
                cursor: set.cursor,
                summary: set.summary,
            });
        }
    }
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(search_navigate(state, ctx, params.buffer_id, direction, params.extend).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn search_navigate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: Direction,
    extend: bool,
) -> Result<SearchNavResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let key = (client_id, buffer_id);
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let Some(entry) = s.searches.get(&key) else {
        // No active search — return a zero-summary with the current cursor untouched.
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: SearchSummary {
                buffer_id,
                total: 0,
                truncated: false,
                current_index: 0,
            },
        });
    };
    if entry.matches.is_empty() {
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(SearchNavResult {
            cursor,
            summary: summary_for(buf, entry, buffer_id, &cursor),
        });
    }

    // Find the next/prev match relative to the selection's *far edge in the travel direction* — the
    // right end going forward, the left end going backward. This is the cursor/head in the normal
    // case (a selection oriented the way you're travelling), so navigation proceeds from where the
    // cursor is. But using the far edge rather than the head directly means a direction reversal off
    // a match (e.g. `n` then `Alt-n`) steps to the adjacent match instead of re-selecting the one
    // you're on, and a plain `n`/`prev` after a multi-match `Shift`-extend steps off the *whole*
    // selection instead of landing back inside it. Extend uses the same reference, so both paths
    // keep making progress for free.
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let reference = match direction {
        Direction::Forward => selection_end(&current),
        Direction::Backward => selection_start(&current),
    };
    // Find the match strictly past the reference in the travel direction. If there isn't one we
    // wrap to the far end — and remember that we wrapped, so an extend can reset instead of growing
    // across the boundary (see the orientation block below).
    let found = match direction {
        Direction::Forward => entry
            .matches
            .iter()
            .copied()
            .find(|(start, _)| pos_tuple(*start) > pos_tuple(reference)),
        Direction::Backward => entry
            .matches
            .iter()
            .rev()
            .copied()
            .find(|(start, _)| pos_tuple(*start) < pos_tuple(reference)),
    };
    let wrapped = found.is_none();
    let target = found.or_else(|| match direction {
        Direction::Forward => entry.matches.first().copied(),
        Direction::Backward => entry.matches.last().copied(),
    });
    let Some((start, end_excl)) = target else {
        return Ok(SearchNavResult {
            cursor: current,
            summary: summary_for(buf, entry, buffer_id, &current),
        });
    };

    // Resolve the match's char bounds. We compute the inclusive end here (one char before the
    // exclusive end) using char-index arithmetic, mirroring how `Char` motion does it — that way
    // multi-byte matches stay on char boundaries.
    let start_char = motion::pos_to_char(buf, start);
    let end_char_excl = motion::pos_to_char(buf, end_excl);
    let last_char = end_char_excl.saturating_sub(1).max(start_char);
    // Non-extend re-selects just the match, oriented by travel direction: going forward the anchor
    // sits at the start and the head leads on the last char; going backward they swap so the head
    // leads on the start char (cursor before anchor). The orientation comes purely from `direction`,
    // so a wrap doesn't flip it — a forward `n` that wraps end→start stays forward-oriented, a
    // backward `prev` that wraps start→end stays backward-oriented. Either way the leftmost end is
    // still the match start, so the `selection_start` reference above keeps making progress.
    //
    // Extend pins the anchor and lands the head on the match's near edge in the travel direction —
    // the last char going forward (so the selection covers through the match), the first char going
    // back — re-anchoring via `extend_anchor` so reversing direction grows the selection instead of
    // discarding the span already covered on the far side. A wrap is the exception: growing the
    // anchor across the document boundary would engulf the whole span from the wrapped match through
    // the old position, so on wrap we fall through to the non-extend reset and select just the
    // target match, letting the user start a fresh selection from the far end.
    let start_pos = motion::char_to_pos(buf, start_char);
    let last_pos = motion::char_to_pos(buf, last_char);
    let (anchor_pos, position) = if extend && !wrapped {
        let head = match direction {
            Direction::Forward => last_pos,
            Direction::Backward => start_pos,
        };
        (extend_anchor(&current, head), head)
    } else {
        match direction {
            Direction::Forward => (start_pos, last_pos),
            Direction::Backward => (last_pos, start_pos),
        }
    };
    let new_cursor = CursorState {
        position,
        anchor: anchor_pos,
        match_bracket: None,
        jumplist_position: None,
    };
    let prev_cursor = s.cursors.get(&key).copied().unwrap_or_default();
    set_cursor(&mut s, key, new_cursor);
    s.record_motion(key, prev_cursor, new_cursor);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, buffer_id);
    let buf_ref = s.doc_of(buffer_id);
    let summary = {
        let entry_ref = s.searches.get(&key).expect("active search just confirmed");
        summary_for(buf_ref, entry_ref, buffer_id, &new_cursor)
    };
    let entry_mut = s
        .searches
        .get_mut(&key)
        .expect("active search just confirmed");
    entry_mut.last_pushed_index = summary.current_index;
    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    Ok(SearchNavResult {
        cursor: new_cursor,
        summary,
    })
}

fn selection_start(c: &CursorState) -> LogicalPosition {
    if pos_tuple(c.anchor) < pos_tuple(c.position) {
        c.anchor
    } else {
        c.position
    }
}

fn selection_end(c: &CursorState) -> LogicalPosition {
    if pos_tuple(c.anchor) > pos_tuple(c.position) {
        c.anchor
    } else {
        c.position
    }
}

/// New anchor for an *extending* move whose cursor lands on `head`. Normally the anchor is kept, so
/// the selection is the usual `[anchor, head]`. But when `head` falls on the opposite side of the
/// anchor from the *current* cursor — a direction reversal across the pivot — keeping the anchor
/// would throw away the span the selection already covered on the old side. In that case we
/// re-anchor to the previous cursor position so the move grows the selection rather than collapsing
/// it across the pivot. The decision is based purely on where `head` lands relative to the current
/// selection, so it's independent of which binding (next/prev, etc.) drove the move.
pub fn extend_anchor(current: &CursorState, head: LogicalPosition) -> LogicalPosition {
    let a = pos_tuple(current.anchor);
    let c = pos_tuple(current.position);
    let h = pos_tuple(head);
    let crosses_pivot = (c < a && h > a) || (c > a && h < a);
    if crosses_pivot {
        current.position
    } else {
        current.anchor
    }
}

pub fn pos_tuple(p: LogicalPosition) -> (u32, u32) {
    (p.line, p.col)
}

/// Compute the `SearchSummary` for the given entry and cursor.
pub fn summary_for(
    buf: &Document,
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
/// (`anchor == m.start` *and* `cursor == last char of m`), or `0` if no match matches.
/// Single-char matches: the cursor's selection collapses to a 1-char point, and we match it
/// against the match's single char. Comparing both endpoints means the counter only shows
/// when the user is genuinely "on" a match — extending or shrinking the selection drops it.
fn match_index_for_cursor(buf: &Document, entry: &SearchEntry, cursor: &CursorState) -> u32 {
    // The counter reflects the match the cursor *head* sits on: a match is "current" when the head
    // falls anywhere within it. This keeps the index live across all the ways a head lands on a
    // match — `/` and `?` entry, `n`/`Alt-n` re-selection, and `Shift-n`/`Shift-Alt-n` extension
    // (where the selection spans several matches but the head rests on one). It's orientation-
    // agnostic by construction, since only the head matters, not which end is the anchor.
    let pos_char = motion::pos_to_char(buf, cursor.position);
    entry
        .matches
        .iter()
        .position(|(start, end_excl)| {
            let m_start_char = motion::pos_to_char(buf, *start);
            let m_end_char = motion::pos_to_char(buf, *end_excl);
            let m_last_char = m_end_char.saturating_sub(1);
            pos_char >= m_start_char && pos_char <= m_last_char
        })
        .map(|i| (i as u32).saturating_add(1))
        .unwrap_or(0)
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
    let buf = match s.try_doc_of(buffer_id) {
        Some(b) => b,
        None => return pushes,
    };
    let revision = buf.revision;
    for vp in s.viewports.values() {
        if vp.client_id != client_id || !vp.binds(buffer_id) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        // The **view's** length, not the buffer's — see `build_lines_changed_notif`.
        let view_lines = ViewLayout::of(&vp.elements, |id| s.doc_of(id).line_count()).line_count();
        let new_first = vp.first_view_line.min(ViewLine(view_lines));
        let new_last_excl = vp
            .last_view_line_exclusive
            .min(ViewLine(view_lines))
            .max(new_first);
        let window = render_window(
            s,
            client_id,
            vp.view_id,
            &vp.elements,
            vp.focused,
            new_first,
            new_last_excl,
            vp.wrap_geometry(),
            vp.rows,
            vp.diff_view,
            SneakLabels::Shown,
        );
        let params = ViewportLinesChangedParams {
            viewport_id: vp.id,
            buffer: buffer_id,
            revision,
            range: LogicalLineRange {
                start_view_line: vp.first_view_line,
                end_view_line_exclusive: vp.last_view_line_exclusive,
            },
            total_visual_rows: window.total_visual_rows,
            first_visual_row: window.first_visual_row,
            max_line_width: window.max_line_width,
            root: window.root,
            view_line_count: window.view_line_count,
            max_scroll_view_line: window.max_scroll_view_line,
            git_status: window.git_status,
            cursor: lines_changed_cursor(s, vp),
        };
        pushes.push((
            sender,
            Notification {
                jsonrpc: JsonRpc,
                method: ViewportLinesChanged::NAME.into(),
                params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            },
        ));
    }
    pushes
}

/// After a cursor change for `(client_id, buffer_id)`, build a `search/state_changed`
/// notification with the recomputed `current_index` — but only when a search is active *and*
/// the index actually changed since the last push. The cursor counts as "on" a match whenever its
/// head sits within one (see `match_index_for_cursor`), so the counter stays live as the cursor
/// moves on and off matches, including while a `?`/`Shift-n` selection spans several of them.
pub fn collect_cursor_search_update(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<(mpsc::Sender<Notification>, Notification)> {
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let buf = s.try_doc_of(buffer_id)?;
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
/// buffer. Used by save, reload, and the file-watcher — mutations bump the buffer's `revision`
/// (which clients already learn from `viewport/lines_changed`) and the client derives `dirty`
/// as `revision != saved_revision`, so this notification is only needed when `saved_revision`
/// changes or when the external-change flags flip.
pub(crate) fn collect_buffer_state_pushes(s: &ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut pushes = Vec::new();
    // Fan out per attached buffer: the document state (saved revision, external flags, path) is
    // shared, but each sibling's viewers hear it under their own buffer id — and with that
    // buffer's own transient flag.
    for id in s.doc_siblings(buffer_id) {
        let Some(buf) = s.try_doc_of(id) else {
            continue;
        };
        let transient = s.buffers.get(&id).map(|b| b.transient).unwrap_or(false);
        let params = BufferStateParams {
            buffer_id: id,
            saved_revision: buf.saved_revision(),
            saved_at_unix_ms: buf.last_modified_unix_ms,
            externally_modified: buf.externally_modified,
            externally_deleted: buf.externally_deleted,
            transient,
            // Lets a save-as rename follow to every other client viewing this shared buffer.
            path: buf.canonical_path.as_ref().map(|p| p.display().to_string()),
        };
        let json = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
        let mut clients: std::collections::HashSet<ClientId> = std::collections::HashSet::new();
        for vp in s.viewports.values() {
            if vp.shows(id) {
                clients.insert(vp.client_id);
            }
        }
        pushes.extend(clients.into_iter().filter_map(|cid| {
            let session = s.clients.get(&cid)?;
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferState::NAME.into(),
                    params: json.clone(),
                },
            ))
        }));
    }
    pushes
}

/// Promote a transient buffer to permanent. Called from every buffer-mutation handler (the
/// first edit is what makes a previewed buffer worth keeping) and from `buffer/save`. Returns
/// the `buffer/state` pushes telling viewers the flag flipped; empty when the buffer wasn't
/// transient (the common case) or doesn't exist.
pub fn promote_transient(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    match s.buffers.get_mut(&buffer_id) {
        Some(buf) if buf.transient => {
            buf.transient = false;
            collect_buffer_state_pushes(s, buffer_id)
        }
        _ => Vec::new(),
    }
}

/// Apply a `buffer/open { transient }` intent to an *existing* buffer: `Some(false)` pins
/// (promotes) it; `Some(true)` / `None` leave it alone — an open never demotes a permanent
/// buffer to transient. Returns the promotion's `buffer/state` pushes (usually empty).
pub fn pin_buffer_if_requested(
    s: &mut ServerState,
    buffer_id: BufferId,
    transient: Option<bool>,
) -> PendingPushes {
    if transient == Some(false) {
        promote_transient(s, buffer_id)
    } else {
        Vec::new()
    }
}

/// Recompute every active search on this buffer after a mutation. Returns the pushes (search
/// summary notifications) to be sent after dropping the lock. The line-level highlight refresh
/// happens via the existing `viewport/lines_changed` flow (since `render_window` reads the
/// freshly-recomputed entries).
pub fn refresh_searches_for_buffer(s: &mut ServerState, buffer_id: BufferId) -> PendingPushes {
    let mut pushes = Vec::new();
    if !s.buffers.contains_key(&buffer_id) {
        return pushes;
    }
    // Fan out over the document: a mutation through one buffer shifts byte positions for every
    // sibling buffer sharing the content too.
    let attached = s.doc_siblings(buffer_id);
    // A mutation shifts byte positions, so any symbol-highlight set is now stale. Drop it (and its
    // debounce generation, which invalidates any in-flight refresh); the client re-requests
    // highlights when its cursor lands after the edit. The re-rendered windows the mutation already
    // pushes read `searches` directly, so they correctly show no symbol highlights until then.
    s.symbol_highlights
        .retain(|(_, b), _| !attached.contains(b));
    s.symbol_highlight_gen
        .retain(|(_, b), _| !attached.contains(b));
    let keys: Vec<(ClientId, BufferId)> = s
        .searches
        .keys()
        .filter(|(_, b)| attached.contains(b))
        .copied()
        .collect();
    for key in keys {
        let query = s.searches[&key].query.clone();
        let options = s.searches[&key].options;
        // Per key, because the scope is per *client*: two clients on one buffer can have focus in
        // different elements, and each one's matches are its own element's.
        let Ok(scope) = s.motion_scope(key.0, key.1) else {
            continue;
        };
        let buf = scope.doc();
        let mut entry = match compute_search_entry(&scope, &query, &options) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let cursor = s.cursors.get(&key).copied().unwrap_or_default();
        let summary = summary_for(buf, &entry, key.1, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        if let Some(sender) = s.clients.get(&key.0).map(|c| c.outbound.clone()) {
            pushes.push((
                sender,
                Notification {
                    jsonrpc: JsonRpc,
                    method: SearchStateChanged::NAME.into(),
                    params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
                },
            ));
        }
    }
    pushes
}

/// Convert a buffer-wide byte offset to a `(line, col_bytes)` position.
pub fn byte_to_logical(buf: &Document, byte_idx: usize) -> aether_protocol::LogicalPosition {
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

/// The buffer a client should land on after its current one is closed: the top of its active
/// workspace's MRU, else any remaining buffer in that workspace, else the most-recently-used *dormant*
/// buffer (a session-restored file `buffer/open` materializes by id), else `None` (caller opens a
/// scratch). The dormant fallback means closing your last live buffer after a session restore drops
/// you back onto a restored file rather than a blank scratch. Shared by `buffer/close` and the
/// deletion paths so the requesting client and any other clients that were viewing the buffer
/// resolve their next buffer identically.
pub fn next_buffer_for_client(s: &ServerState, client_id: ClientId) -> Option<BufferId> {
    let workspace_name = s.active_workspace(client_id).map(|p| p.id.clone());
    s.active_workspace(client_id)
        .and_then(|p| p.mru_buffers.front().copied())
        .or_else(|| {
            workspace_name.as_deref().and_then(|name| {
                s.buffer_workspaces
                    .iter()
                    .find(|(_, pname)| pname.as_str() == name)
                    .map(|(id, _)| *id)
            })
        })
        .or_else(|| {
            workspace_name
                .as_deref()
                .and_then(|name| s.first_dormant_id(name))
        })
}

/// `(client, buffer)` pairs for every client *other than* `except` affected by closing
/// `buffer_ids`: clients with a viewport on one (the push hands them a successor to switch to),
/// plus clients whose active workspace context holds one in its MRU — those may have it as their
/// *tether*, which must exit even while the client is viewing something else. Capture this BEFORE
/// tearing the buffers down — teardown drops the viewports and MRU entries this reads. At most one
/// entry per `(client, buffer)` pair; non-matching pushes are ignored client-side, so the broad
/// audience is safe.
pub fn clients_affected_by_close(
    s: &ServerState,
    buffer_ids: &[BufferId],
    except: ClientId,
) -> Vec<(ClientId, BufferId)> {
    let targets: std::collections::HashSet<BufferId> = buffer_ids.iter().copied().collect();
    let mut seen: std::collections::HashSet<(ClientId, BufferId)> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    for vp in s.viewports.values() {
        if vp.client_id != except
            && targets.contains(&vp.buffer_id())
            && seen.insert((vp.client_id, vp.buffer_id()))
        {
            out.push((vp.client_id, vp.buffer_id()));
        }
    }
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
        for id in ws.mru_buffers.iter().filter(|id| targets.contains(id)) {
            if seen.insert((client_id, *id)) {
                out.push((client_id, *id));
            }
        }
    }
    out
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

/// Build the `buffer/closed` pushes for the clients captured by [`clients_affected_by_close`],
/// telling each which buffer to switch to. Call AFTER teardown so each next-buffer reflects the
/// settled MRU. Clients that have since disconnected are skipped.
pub fn buffer_closed_pushes(s: &ServerState, affected: &[(ClientId, BufferId)]) -> PendingPushes {
    buffer_closed_pushes_with(s, affected, &Default::default())
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

/// [`buffer_closed_pushes`], with a per-buffer successor override given as a **path**.
///
/// A worktree rebind closes each buffer and reopens it at the same *relative* path on the new tree,
/// so it knows something the generic rule can't: which file replaces which. Handed back as a path
/// rather than an id, because the id it could offer — a reserved dormant entry — is not stable:
/// the initiating client activates straight after the rebind, and a landing buffer on that same
/// file materialises the entry under a different id, leaving whoever opens second asking for one
/// that no longer exists. `buffer/open` on a path already open returns the existing buffer, so both
/// clients converge whichever order they arrive in.
pub fn buffer_closed_pushes_with(
    s: &ServerState,
    affected: &[(ClientId, BufferId)],
    successor: &std::collections::HashMap<BufferId, std::path::PathBuf>,
) -> PendingPushes {
    affected
        .iter()
        .filter_map(|&(client_id, buffer_id)| {
            let session = s.clients.get(&client_id)?;
            let next_path = successor
                .get(&buffer_id)
                .and_then(|path| workspace_location_of(s, client_id, path));
            let params = BufferClosedParams {
                buffer_id,
                // Only as the fallback: a path wins when there is one.
                next_buffer_id: next_path
                    .is_none()
                    .then(|| next_buffer_for_client(s, client_id))
                    .flatten(),
                next_path,
            };
            Some((
                session.outbound.clone(),
                Notification {
                    jsonrpc: JsonRpc,
                    method: BufferClosed::NAME.into(),
                    params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
                },
            ))
        })
        .collect()
}
