//! `cursor/*` — cursor motion, selection, tree-select expand/contract, and motion undo/redo.

use super::*;

pub async fn cursor_move(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorMoveParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    // Visual motions need viewport state (wrap mode + width). Look it up and dispatch to the
    // dedicated resolver; everything else goes through `resolve_motion` which only needs the
    // buffer.
    let virtual_col_in = s.virtual_col.get(&key).copied();
    // `Some(col)` → set virtual col to `col`; `None` → clear it. Only `VisualLine` preserves it.
    let mut new_virtual_col: Option<u32> = None;
    // Set by `o`/`Alt-o` to land the target's identifier selected (anchor at the name start).
    let mut nav_anchor: Option<LogicalPosition> = None;
    let new_pos = match &params.motion {
        Motion::VisualLine {
            viewport_id,
            direction,
            count,
        } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            let (pos, target_vcol) = motion::resolve_visual_line(
                buf,
                vp.wrap_geometry(),
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
            motion::resolve_visual_line_start(buf, vp.wrap_geometry(), current.position)
        }
        Motion::VisualLineEnd { viewport_id } => {
            let vp = s.viewports.get(viewport_id).ok_or_else(|| {
                RpcError::new(
                    aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND,
                    format!("unknown viewport_id: {viewport_id}"),
                )
            })?;
            motion::resolve_visual_line_end(buf, vp.wrap_geometry(), current.position)
        }
        Motion::LogicalLine {
            direction,
            count,
            preserve_col,
        } => {
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
        // Selection-edge motions read the whole selection (anchor + cursor), which
        // `resolve_motion` doesn't see — dispatch to the dedicated resolver.
        Motion::SelectionEdge { edge } => {
            motion::resolve_selection_edge(buf, current.position, current.anchor, *edge)
        }
        // Navigation-unit motions (`o`) walk only the LSP document-symbol outline (the same tree
        // `Space o` shows). With no outline yet — still loading, or no language server — the slice
        // is empty and the motion is a no-op; it never falls back to a different source, so `o`
        // behaves the same before and after symbols load. `Next`/`Prev` also return an anchor so
        // the target's identifier lands *selected* (see `nav_anchor`).
        Motion::NextNavigationUnit { .. }
        | Motion::PrevNavigationUnit { .. }
        | Motion::EndOfNavigationUnit
        | Motion::StartOfNavigationUnit => {
            let symbols = s
                .document_symbols
                .get(&params.buffer_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let (pos, anchor) = motion::resolve_navigation_motion(
                buf,
                symbols,
                current.position,
                current.anchor,
                &params.motion,
                params.extend_selection,
            );
            nav_anchor = anchor;
            pos
        }
        // `p` / `Alt-p` (first non-blank of the next/previous line). Unlike the plain motions, an
        // extending `Shift-p`/`Shift-Alt-p` re-anchors on a direction reversal across the pivot (see
        // `extend_anchor`), so flipping direction grows the selection from the old cursor instead of
        // collapsing it across the anchor. Routed through `nav_anchor` so it overrides the default
        // keep-anchor path below.
        Motion::LogicalLineFirstNonblank { .. } => {
            let pos = motion::resolve_motion(buf, current.position, &params.motion);
            if params.extend_selection {
                nav_anchor = Some(extend_anchor(&current, pos));
            }
            pos
        }
        _ => motion::resolve_motion(buf, current.position, &params.motion),
    };
    // A navigation motion that supplies its own anchor wins: `o`/`Alt-o` select the identifier, and
    // `Shift-o`/`Shift-Alt-o` grow the selection to include it (anchor pinned to the kept edge).
    // Otherwise extend keeps the current anchor (which may already equal position, i.e. a point) and
    // a plain move collapses to a 1-char point. The data model always has an anchor, so "no
    // selection" means `anchor == position`.
    let new_anchor = nav_anchor.unwrap_or(if params.extend_selection {
        current.anchor
    } else {
        new_pos
    });

    let new_state = CursorState {
        position: new_pos,
        anchor: new_anchor,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, new_state);
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
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

/// `w` / `Alt-w` — select a word. Sets both anchor and cursor (so it can't go through
/// `cursor/move`, which only moves the cursor and derives the anchor); the per-press logic and
/// the `count` repeat loop live in [`motion::resolve_select_word`] and here respectively.
pub async fn cursor_select_word(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSelectWordParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let original = s.cursors.get(&key).copied().unwrap_or_default();

    // The repeat loop lives server-side (`3w` = one round-trip).
    let mut working = original;
    for _ in 0..params.count.max(1) {
        let (position, anchor) = motion::resolve_select_word(
            buf,
            working.position,
            working.anchor,
            params.boundary,
            params.extend,
        );
        working = CursorState {
            position,
            anchor,
            match_bracket: None,
            jumplist_position: None,
        };
    }
    let new_state = working;

    set_cursor(&mut s, key, new_state);
    s.record_motion(key, original, new_state);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    s.virtual_col.remove(&key);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
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
/// Forward grows the *bottom-most* edge of the selection downward; backward grows the *top-most*
/// edge upward. This means edge-extension stays orientation-independent of which end the cursor
/// sits on — useful after `cursor/swap_anchor`. The cursor stays at the end it was already on;
/// the anchor occupies the other end.
///
/// First-press, point-cursor asymmetry: when there's no selection, Forward selects the cursor's
/// line, while Backward (extend or not) selects the line *above* the cursor. That keeps the two
/// bindings distinct on the very first press (otherwise both would just select the current line)
/// and matches a "go up" mental model for Backward. Subsequent presses behave the same as
/// before: Backward + extend then widens upward from there.
pub async fn cursor_select_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSelectLineParams,
) -> Result<CursorState, RpcError> {
    // The repeat loop lives server-side (`3x` = one round-trip).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(cursor_select_line_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_select_line_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorSelectLineParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let cur = current.position;

    // Top / bottom edges of the current selection, normalized so we can reason about "extend
    // the bottom down" independent of which end the cursor sits on. For a point cursor
    // (anchor == position) both edges land on the cursor.
    let (top_edge, bottom_edge) = if (current.anchor.line, current.anchor.col) < (cur.line, cur.col)
    {
        (current.anchor, cur)
    } else {
        (cur, current.anchor)
    };
    let has_range = !current.is_point();
    let cursor_was_at_top = has_range && cur == top_edge;

    // Advance the relevant edge only when the selection already spans whole lines; otherwise snap
    // it without advancing. A point cursor (anchor == position) on an empty line is trivially
    // whole — its only char is the newline at col 0, so the point already selects the line. So the
    // edge advances past it (plain `x`/`Alt-x` step to the next/previous line rather than getting
    // stuck), and — when extending — it counts as a real range so `Shift-x`/`Alt-Shift-x` grow
    // *over* the empty line instead of jumping past it (the `|| already_whole` in the match).
    // Backward on any point cursor (extend or not) also advances upward, so Alt-x / Alt-Shift-x
    // jump to the line above on the first press (see the doc comment).
    let bottom_len = motion::line_byte_len_excl_newline(buf, bottom_edge.line);
    let already_whole = if has_range {
        top_edge.col == 0 && bottom_edge.col >= bottom_len
    } else {
        bottom_len == 0 && cur.col == 0
    };
    let advance_top_for_backward = already_whole || !has_range;
    let new_top = if advance_top_for_backward && params.direction == Direction::Backward {
        top_edge.line.saturating_sub(1)
    } else {
        top_edge.line
    };
    let new_bottom = if already_whole && params.direction == Direction::Forward {
        bottom_edge.line.saturating_add(1)
    } else {
        bottom_edge.line
    };
    // Extend (grow the span) only when Shift is held *and* there's already a whole-line span to
    // grow from — a real range, or an empty line whose whole-line form is a point. Otherwise
    // collapse to a single line: snap the current line, or step to the next/previous one.
    let (top_line, bottom_line) = match (
        params.extend && (has_range || already_whole),
        params.direction,
    ) {
        (true, _) => (new_top, new_bottom),
        (false, Direction::Forward) => (new_bottom, new_bottom),
        (false, Direction::Backward) => (new_top, new_top),
    };

    let last_line = (buf.text.len_lines() as u32).saturating_sub(1);
    let top_line = top_line.min(last_line);
    let bottom_line = bottom_line.min(last_line);
    let top_pos = LogicalPosition {
        line: top_line,
        col: 0,
    };
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
    let new_state = CursorState {
        position: cursor_pos,
        anchor: anchor_pos,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
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
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    // `forward_only` (`Alt-r`) normalizes orientation instead of toggling: only a backward
    // selection (cursor before anchor) swaps; anything else returns completely untouched — no
    // motion-history entry, no virtual-col or tree-history reset.
    let backward =
        (current.position.line, current.position.col) < (current.anchor.line, current.anchor.col);
    if params.forward_only && !backward {
        return Ok(wrap_for_response(&s, client_id, params.buffer_id, current));
    }
    // Swap anchor and position. For a point cursor (anchor == position) this is a no-op.
    let new_state = CursorState {
        position: current.anchor,
        anchor: current.position,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(&mut s, key, new_state);
    s.record_motion(key, current, new_state);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let response = wrap_for_response(&s, client_id, params.buffer_id, new_state);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(response)
}

pub async fn cursor_select_all(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSelectAllParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    // Anchor at the buffer start, cursor at the end of the last line — clamping an out-of-range
    // position resolves the buffer end in the system's column units (the whole-line / forward
    // normal form, per CLAUDE.md).
    let position = motion::clamp_position(
        buf,
        LogicalPosition {
            line: u32::MAX,
            col: u32::MAX,
        },
    );
    let result = CursorState {
        position,
        anchor: LogicalPosition { line: 0, col: 0 },
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
    Ok(response)
}

pub async fn cursor_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorSetParams,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let position = motion::clamp_position(buf, params.position);
    let anchor = motion::clamp_position(buf, params.anchor);
    let (position, anchor) = motion::snap_selection(buf, position, anchor, params.granularity);
    let result = CursorState {
        position,
        anchor,
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
    Ok(response)
}

/// Rewind one step on this client's per-buffer motion history. Independent of `input/undo`.
pub async fn cursor_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    // The repeat loop lives server-side, stopping once the history is exhausted (the
    // `applied: false` result is returned so the client still learns the final state).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_undo_once(state, ctx, &params).await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_undo_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.undo.is_empty() {
        return Ok(CursorUndoResult {
            applied: false,
            cursor: current,
        });
    }
    let prev = history.undo.pop_back().expect("just checked non-empty");
    history.redo.push(current);
    while history.redo.len() > MOTION_HISTORY_CAP {
        history.redo.remove(0);
    }

    set_cursor(&mut s, key, prev);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let prev = wrap_for_response(&s, client_id, params.buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult {
        applied: true,
        cursor: prev,
    })
}

pub async fn cursor_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    // The repeat loop lives server-side, stopping once the history is exhausted (the
    // `applied: false` result is returned so the client still learns the final state).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = cursor_redo_once(state, ctx, &params).await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_redo_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CursorUndoParams,
) -> Result<CursorUndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    let key = (client_id, params.buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();

    let history = s.motion_history.entry(key).or_default();
    if history.redo.is_empty() {
        return Ok(CursorUndoResult {
            applied: false,
            cursor: current,
        });
    }
    let next = history.redo.pop().expect("just checked non-empty");
    history.undo.push_back(current);
    while history.undo.len() > MOTION_HISTORY_CAP {
        history.undo.pop_front();
    }

    set_cursor(&mut s, key, next);
    s.virtual_col.remove(&key);
    s.clear_tree_selection_history(client_id, params.buffer_id);
    let search_update = collect_cursor_search_update(&mut s, client_id, params.buffer_id);
    let next = wrap_for_response(&s, client_id, params.buffer_id, next);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(CursorUndoResult {
        applied: true,
        cursor: next,
    })
}

// ---- cursor/expand and cursor/contract ---------------------------------------------------------

pub async fn cursor_tree_select(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CursorTreeSelectParams,
) -> Result<CursorState, RpcError> {
    // Repeat server-side, stopping once the cursor stops changing (top of the tree /
    // single node — repeated presses are a no-op, not an error).
    let mut last: Option<CursorState> = None;
    for _ in 0..params.count.max(1) {
        let r = match params.direction {
            TreeSelectDirection::Expand => cursor_expand_once(state, ctx, params.buffer_id).await?,
            TreeSelectDirection::Contract => {
                cursor_contract_once(state, ctx, params.buffer_id).await?
            }
        };
        if last == Some(r) {
            break;
        }
        last = Some(r);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn cursor_expand_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let key = (client_id, buffer_id);
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
    let mut node = root
        .descendant_for_byte_range(start_byte, end_byte_excl)
        .unwrap_or(root);
    while node.start_byte() == start_byte && node.end_byte() == end_byte_excl {
        match node.parent() {
            Some(p) => node = p,
            None => return Ok(current), // already at the root
        }
    }

    let new_start_char = buf.text.byte_to_char(node.start_byte());
    let new_end_char_excl = buf
        .text
        .byte_to_char(node.end_byte())
        .max(new_start_char + 1);
    let new_last_char = new_end_char_excl.saturating_sub(1).max(new_start_char);
    let anchor = motion::char_to_pos(buf, new_start_char);
    let position = motion::char_to_pos(buf, new_last_char);
    let new_cursor = CursorState {
        position,
        anchor,
        match_bracket: None,
        jumplist_position: None,
    };

    set_cursor(&mut s, key, new_cursor);
    s.record_motion(key, current, new_cursor);
    s.virtual_col.remove(&key);
    s.tree_selection_history
        .entry(key)
        .or_default()
        .push(current);
    let search_update = collect_cursor_search_update(&mut s, client_id, buffer_id);
    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(new_cursor)
}

async fn cursor_contract_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
) -> Result<CursorState, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&buffer_id) {
        return Err(RpcError::buffer_not_found(buffer_id));
    }
    let key = (client_id, buffer_id);
    let prev = s
        .tree_selection_history
        .get_mut(&key)
        .and_then(|stack| stack.pop());
    let Some(prev) = prev else {
        // Nothing to contract back to.
        let cur = s.cursors.get(&key).copied().unwrap_or_default();
        return Ok(wrap_for_response(&s, client_id, buffer_id, cur));
    };
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    set_cursor(&mut s, key, prev);
    s.record_motion(key, current, prev);
    s.virtual_col.remove(&key);
    let search_update = collect_cursor_search_update(&mut s, client_id, buffer_id);
    let prev = wrap_for_response(&s, client_id, buffer_id, prev);
    drop(s);
    if let Some((sender, notif)) = search_update {
        let _ = sender.send(notif).await;
    }
    Ok(prev)
}

/// Char range `[start, end_excl)` covered by the cursor's current selection. Collapsed cursors
/// (no anchor) yield a 1-char range so byte conversion produces a non-empty span.
pub fn current_selection_char_range(buf: &Document, cursor: &CursorState) -> (usize, usize) {
    let (lo_pos, hi_pos) = motion::ordered(cursor.position, cursor.anchor);
    let total = buf.text.len_chars();
    let lo = motion::pos_to_char(buf, lo_pos).min(total);
    let hi_inclusive = motion::pos_to_char(buf, hi_pos).min(total);
    (
        lo,
        (hi_inclusive + 1).min(total).max(lo + 1).min(total.max(lo)),
    )
}

/// The resolved operand for a case transform: the char range `[start_char, end_char)` to replace,
/// its line span, the replacement text, and whether the operand was scanned from the caret (which
/// decides whether the post-edit cursor collapses or re-selects).
pub struct TransformEdit {
    pub start_char: usize,
    pub end_char: usize,
    pub first_line: u32,
    pub last_line: u32,
    pub new_text: String,
    pub scanned: bool,
}

/// Resolve a case transform against the current cursor, or `None` for a no-op (empty operand, or
/// a transform that leaves the text unchanged — e.g. no letters in range). With `scan` (Insert
/// mode) the operand is the identifier (word run of alphanumeric/`_`) under the caret; otherwise
/// (Normal mode) it's exactly the selection — a point cursor being the single char under the
/// block. Shared by the no-op precheck and `apply_edit`, so the two always agree.
pub fn resolve_transform_case(
    buf: &Document,
    cursor: &CursorState,
    kind: CaseKind,
    scan: bool,
) -> Option<TransformEdit> {
    let total = buf.text.len_chars();
    let (start_char, end_char, first_line, last_line) = if scan {
        let (start_pos, end_pos) = motion::word_run(buf, cursor.position);
        let sc = motion::pos_to_char(buf, start_pos);
        let ec = motion::pos_to_char(buf, end_pos)
            .saturating_add(1)
            .min(total);
        (sc, ec, start_pos.line, end_pos.line)
    } else {
        let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
        let sc = motion::pos_to_char(buf, lo);
        let ec = motion::pos_to_char(buf, hi).saturating_add(1).min(total);
        (sc, ec, lo.line, hi.line)
    };
    if start_char >= end_char {
        return None;
    }
    let original: String = buf.text.slice(start_char..end_char).chars().collect();
    let new_text = case::transform(kind, &original);
    if new_text == original {
        return None;
    }
    Some(TransformEdit {
        start_char,
        end_char,
        first_line,
        last_line,
        new_text,
        scanned: scan,
    })
}

/// Whether the single chars immediately outside each end of the selection form a known delimiter
/// pair — the precondition unsurround strips on. `current_selection_char_range` gives the
/// selection as `[sc, ec)`, so the hugging chars are at `sc - 1` and `ec`; both must exist.
pub fn has_enclosing_pair(buf: &Document, cursor: &CursorState) -> bool {
    let (sc, ec) = current_selection_char_range(buf, cursor);
    if sc < 1 || ec >= buf.text.len_chars() {
        return false;
    }
    surround::matching_pair(buf.text.char(sc - 1), buf.text.char(ec))
}

/// Char range `[start, end)` of a line's content, excluding the trailing newline — the span a
/// line-scoped surround/unsurround wraps or strips.
pub fn line_content_char_range(buf: &Document, line: usize) -> (usize, usize) {
    let start = buf.text.line_to_char(line);
    let line_slice = buf.text.line(line);
    let len_chars = line_slice.len_chars();
    let has_trailing_nl = len_chars > 0 && line_slice.char(len_chars - 1) == '\n';
    let content_chars = if has_trailing_nl {
        len_chars - 1
    } else {
        len_chars
    };
    (start, start + content_chars)
}

/// Whether the cursor line's content begins and ends with a known delimiter pair — the precondition
/// line-scoped unsurround strips on. Needs at least two content chars (the two delimiters).
pub fn line_has_enclosing_pair(buf: &Document, line: usize) -> bool {
    let (sc, ec) = line_content_char_range(buf, line);
    if ec < sc + 2 {
        return false;
    }
    surround::matching_pair(buf.text.char(sc), buf.text.char(ec - 1))
}

/// Echo the buffer's current revision and the client's cursor without editing — the `EditResult`
/// an edit RPC returns when it resolves to a no-op (unknown surround delimiter, no enclosing pair).
pub async fn current_edit_result(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Result<EditResult, RpcError> {
    let s = state.lock().await;
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let revision = buf.revision;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let cursor = wrap_for_response(&s, client_id, buffer_id, cursor);
    Ok(EditResult { revision, cursor })
}
