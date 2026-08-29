//! The shared edit pipeline: `apply_edit` and everything that hangs off it — undo/redo,
//! cursor clamping, and the buffer-changed / lines-changed push plumbing.
//!
//! Every module that mutates a document routes through here, which is why so much of it is
//! `pub(super)`. It also still holds `input/join_lines` and `input/move_lines`, whose handlers
//! are inseparable from the multi-step apply they drive.

use super::*;

pub async fn input_move_lines_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &InputMoveLinesParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Phase 1: read state and compute the edit while holding the lock.
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    // Selection's line range: the lines the user wants to move.
    let (start_pos, end_pos) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start_pos.line, end_pos.line);

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
    let cursors_before = document_cursor_snapshot(&s, buffer_id);

    let was_dirty = s.doc_of(buffer_id).dirty;
    let (revision, new_cursor) = {
        let mut buf_mut = s.editable_doc(buffer_id)?;
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
            position: motion::clamp_position(&buf_mut, shift(cursor.position)),
            anchor: motion::clamp_position(&buf_mut, shift(cursor.anchor)),
            match_bracket: None,
            jumplist_position: None,
        };
        (revision, new_cursor)
    };
    set_cursor(&mut s, (client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Affected line range for viewport notifications.
    let (edit_first, edit_last_excl) = match params.direction {
        VerticalDirection::Down => (a, b + 2),
        VerticalDirection::Up => (a - 1, b + 1),
    };

    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.doc_of(buffer_id).line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    let pushes: PendingPushes =
        collect_doc_edit_pushes(&s, buffer_id, revision, edit_first, edit_last_excl);

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);
    // LSP: full-document sync.
    notify_lsp_change(&mut s, buffer_id);

    let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
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
    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
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
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    // The repeat loop lives server-side (`3J` = one round-trip).
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(input_join_lines_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

async fn input_join_lines_once(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: &CountedEditParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let buffer_id = params.buffer_id;

    // Figure out which line(s) we're joining. If the cursor has a selection that spans multiple
    // lines, join all of them. Otherwise, join the cursor's line with the one below.
    let (first_line, last_line) = {
        let s = state.lock().await;
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        let (a, b) = motion::ordered(cursor.position, cursor.anchor);
        let buf = s
            .try_doc_of(buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        let line_count = buf.line_count();
        let first = a.line;
        // If single line, join with the line below it. If multi-line selection, join through
        // last selected line.
        let last = if a.line == b.line {
            a.line.saturating_add(1)
        } else {
            b.line
        };
        let last = last.min(line_count.saturating_sub(1));
        (first, last)
    };

    if first_line >= last_line {
        // Nothing to join (we're on the last line).
        let s = state.lock().await;
        let buf = s.doc_of(buffer_id);
        return Ok(EditResult {
            revision: buf.revision,
            cursor: s
                .cursors
                .get(&(client_id, buffer_id))
                .copied()
                .unwrap_or_default(),
        });
    }

    // Compute the joined range, in char offsets. For each pair of consecutive lines, the range
    // to replace is `[end_of_trailing_ws_on_line_i, first_non_ws_on_line_i+1)` — replaced with
    // a single space. We do them in a single sweep on the rope.
    let s = state.lock().await;
    let buf = s.doc_of(buffer_id);

    // Build the full replacement: concatenate the lines, dropping each continuation's leading
    // whitespace (its indent) — nothing is inserted between them. Join deletes exactly what
    // un-join (`input/newline_and_indent`) inserts, "\n" + indent, so the pair mirrors; trailing
    // whitespace stays (a real space before the break becomes the separator), and a wanted
    // separator is one keystroke at the parked cursor.
    let mut joined = String::new();
    // Offset (in chars, within `joined`) of the last seam — the first char that came from the
    // final joined line. The cursor parks there, so un-join (newline before the cursor) reverses
    // the join in place.
    let mut last_seam = 0;
    for line_idx in first_line..=last_line {
        let line_slice = buf.text.line(line_idx as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        if line_idx == first_line {
            joined.push_str(&text);
        } else {
            last_seam = joined.chars().count();
            joined.push_str(text.trim_start());
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

    let cursors_before = {
        let s = state.lock().await;
        document_cursor_snapshot(&s, buffer_id)
    };

    let (revision, new_cursor, was_dirty) = {
        let mut s = state.lock().await;
        let was_dirty = s.doc_of(buffer_id).dirty;
        let mut buf = s.editable_doc(buffer_id)?;
        let revision = buf.apply_edit(
            first_char,
            last_line_end_char,
            &joined,
            EditKindTag::Text,
            cursors_before,
        );
        // Park the cursor on the (last) seam — the first char the join pulled up — not past the
        // joined text: `Ctrl-Alt-g` (newline before the cursor) is then the exact inverse, and a
        // separator can be typed straight in.
        let new_cursor_char = first_char + last_seam;
        let new_pos = motion::char_to_pos(&buf, new_cursor_char);
        let new_cursor = CursorState {
            position: new_pos,
            anchor: new_pos,
            match_bracket: None,
            jumplist_position: None,
        };
        set_cursor(&mut s, (client_id, buffer_id), new_cursor);
        s.clear_motion_history_for_buffer(buffer_id);
        s.clear_tree_selection_history_for_buffer(buffer_id);
        s.clear_virtual_col_for_buffer(buffer_id);
        (revision, new_cursor, was_dirty)
    };

    // Push viewport/lines_changed for affected viewports (we changed multiple lines).
    let (pushes, search_summary_pushes, picker_pushes, new_cursor): (Vec<_>, Vec<_>, Vec<_>, _) = {
        let mut s = state.lock().await;
        let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
        search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
        let new_line_count = s.doc_of(buffer_id).line_count();
        refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
        let pushes = collect_doc_lines_changed_pushes(&s, buffer_id, revision);
        let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);
        // LSP: full-document sync.
        notify_lsp_change(&mut s, buffer_id);
        let new_cursor = wrap_for_response(&s, client_id, buffer_id, new_cursor);
        (pushes, search_summary_pushes, picker_pushes, new_cursor)
    };

    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult {
        revision,
        cursor: new_cursor,
    })
}

#[derive(Clone, Copy)]
pub enum UndoDirection {
    Undo,
    Redo,
}

/// Snapshot every `(client, buffer)` cursor across all buffers attached to `buffer_id`'s document.
/// This is what an undo entry stores — keyed by buffer, not just client, so undo can restore
/// cursors on sibling buffers (the same document open in another workspace) too.
pub fn document_cursor_snapshot(
    s: &ServerState,
    buffer_id: BufferId,
) -> HashMap<(ClientId, BufferId), CursorState> {
    let doc_id = s.buffers[&buffer_id].document;
    let attached: std::collections::HashSet<BufferId> =
        s.buffers_of_document(doc_id).into_iter().collect();
    s.cursors
        .iter()
        .filter(|((_, b), _)| attached.contains(b))
        .map(|(k, cs)| (*k, *cs))
        .collect()
}

pub async fn apply_undo_or_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    direction: UndoDirection,
    collapse_selection: bool,
) -> Result<UndoResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;

    // Snapshot current cursors so the *other* direction's stack can restore them later.
    let current_cursors = document_cursor_snapshot(&s, buffer_id);

    let was_dirty = s.try_doc_of(buffer_id).map(|b| b.dirty).unwrap_or(false);
    let outcome = {
        let mut buf = s.editable_doc(buffer_id)?;
        match direction {
            UndoDirection::Undo => buf.undo(current_cursors),
            UndoDirection::Redo => buf.redo(current_cursors),
        }
    };

    let Some(outcome) = outcome else {
        // Nothing to undo/redo. Echo current cursor and revision back.
        let buf = s.try_doc_of(buffer_id).expect("just checked");
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        return Ok(UndoResult {
            revision: buf.revision,
            applied: false,
            cursor,
        });
    };

    let buf = s.try_doc_of(buffer_id).expect("just modified");
    let revision = buf.revision;

    // Restore cursors from the snapshot, clamped to valid positions in the restored rope. Keys
    // span every buffer attached to the document — sibling buffers' cursors restore too.
    let mut new_cursors: HashMap<(ClientId, BufferId), CursorState> = HashMap::new();
    for (key, cursor) in &outcome.restored_cursors {
        new_cursors.insert(*key, clamp_cursor(buf, *cursor));
    }
    // Cursors on the document's buffers that weren't in the snapshot: just clamp their current
    // position to the new buffer bounds.
    let attached: std::collections::HashSet<BufferId> = s
        .buffers_of_document(s.buffers[&buffer_id].document)
        .into_iter()
        .collect();
    let existing_keys: Vec<(ClientId, BufferId)> = s
        .cursors
        .keys()
        .filter(|(_, b)| attached.contains(b))
        .copied()
        .collect();
    for key in existing_keys {
        if let std::collections::hash_map::Entry::Vacant(e) = new_cursors.entry(key) {
            if let Some(cursor) = s.cursors.get(&key).copied() {
                e.insert(clamp_cursor(buf, cursor));
            }
        }
    }
    // In Insert mode the client requests the restored selection be dropped — undo would otherwise
    // re-select the undone text, breaking the no-selection-in-Insert invariant. Only the requesting
    // client's cursor is collapsed; the flag reflects *its* mode, not other clients'.
    if collapse_selection {
        if let Some(cursor) = new_cursors.get_mut(&(client_id, buffer_id)) {
            cursor.anchor = cursor.position;
        }
    }
    for (key, cursor) in &new_cursors {
        set_cursor(&mut s, *key, *cursor);
    }
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);
    let undoing_cursor = new_cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_else(CursorState::default);

    // Push the full visible window to every viewport on this buffer — the rope was swapped
    // wholesale, so we can't be surgical about it.
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    let new_line_count = s.doc_of(buffer_id).line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);
    // LSP: the rope was swapped wholesale — tell the server so its diagnostics aren't stale.
    notify_lsp_change(&mut s, buffer_id);
    let pushes: PendingPushes = collect_doc_lines_changed_pushes(&s, buffer_id, revision);

    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

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

    Ok(UndoResult {
        revision,
        applied: true,
        cursor: undoing_cursor,
    })
}

fn clamp_cursor(buf: &Document, cursor: CursorState) -> CursorState {
    let position = motion::clamp_position(buf, cursor.position);
    let anchor = motion::clamp_position(buf, cursor.anchor);
    CursorState {
        position,
        anchor,
        match_bracket: None,
        jumplist_position: None,
    }
}

/// Populate `match_bracket` on a cursor that's about to cross the wire. Looks up the bracket
/// pair (if any) at the cursor's position and stamps it onto the state. `match_bracket` is
/// never stored in `state.cursors`; it's purely a derived per-response field that drives the
/// client's match-bracket highlight overlay.
fn with_match_bracket(buf: &Document, mut cursor: CursorState) -> CursorState {
    let Some(syntax) = buf.syntax.as_ref() else {
        return cursor;
    };
    let byte = buf
        .text
        .char_to_byte(motion::pos_to_char(buf, cursor.position));
    if let Some((open, close)) = crate::brackets::find_match_bracket(&syntax.tree, byte) {
        let open_pos = motion::char_to_pos(buf, buf.text.byte_to_char(open));
        let close_pos = motion::char_to_pos(buf, buf.text.byte_to_char(close));
        cursor.match_bracket = Some((open_pos, close_pos));
    }
    cursor
}

/// Populate `jumplist_position` on a cursor that's about to cross the wire. The cursor counts
/// as "on" an entry when it sits exactly where a jump to that entry lands: the selection covers
/// exactly the entry's span (`anchor` at its first char, `position` at its last,
/// orientation-agnostic) for a spanned entry, or a point cursor at the entry's position for a
/// point entry — same strictness as `match_index_for_cursor` uses to gate the in-buffer `A/B`
/// counter. Any motion that grows, shrinks, or shifts the selection drops the indicator on the
/// next response.
///
/// A *whole-target* entry (captured from the Files or Buffers picker) has no position to match
/// against, so the weaker rule applies: being in its buffer at all is being on it. The counter then
/// reads as "file k of N", which is what a captured file list means, and it survives moving around
/// inside the file rather than blinking out on the first motion.
fn with_jumplist_position(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    mut cursor: CursorState,
) -> CursorState {
    let Some(list) = s.jumplist(client_id) else {
        return cursor;
    };
    if list.entries.is_empty() {
        return cursor;
    }
    let Some(buf) = s.try_doc_of(buffer_id) else {
        return cursor;
    };
    let current_abs = buf
        .canonical_path
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let location = crate::jumplist::location_of(current_abs.as_deref(), buffer_id);
    // Compare in char-index space so multi-byte content stays on char boundaries (mirrors
    // `match_index_for_cursor`). Entry coordinates may be stale after edits; `pos_to_char`
    // clamps, same acceptance as jumping to a stale entry.
    let anchor_char = motion::pos_to_char(buf, cursor.anchor);
    let pos_char = motion::pos_to_char(buf, cursor.position);
    let sel_start_char = anchor_char.min(pos_char);
    let sel_end_char = anchor_char.max(pos_char);
    let total = list.entries.len() as u32;
    if let Some(idx) = list.entries.iter().position(|e| {
        if !e.matches_location(location) {
            return false;
        }
        let (Some(start), Some(position)) = (e.start(), e.position) else {
            return true; // whole-target: the buffer alone identifies it
        };
        let e_start_char = motion::pos_to_char(buf, start);
        let e_end_char = motion::pos_to_char(buf, position).max(e_start_char);
        sel_start_char == e_start_char && sel_end_char == e_end_char
    }) {
        cursor.jumplist_position = Some(JumplistPosition {
            current: (idx as u32).saturating_add(1),
            total,
        });
    }
    cursor
}

/// Same as `with_match_bracket` but starts from a `ServerState`: a one-liner for the many
/// handlers that need to populate the field just before returning. Safe if the buffer was
/// already dropped (returns the cursor unchanged). Also stamps `jumplist_position` if the client
/// has a jumplist and the cursor is on one of its entries.
pub fn wrap_for_response(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    cursor: CursorState,
) -> CursorState {
    let with_brackets = s
        .try_doc_of(buffer_id)
        .map(|buf| with_match_bracket(buf, cursor))
        .unwrap_or(cursor);
    with_jumplist_position(s, client_id, buffer_id, with_brackets)
}

/// A structural block edit — resolved inside `apply_edit`'s lock by `resolve_block_edit`, against
/// the same `aether-markdown` parse the reading view renders from. Selection-relative: the params
/// carry no positions.
#[derive(Debug, Clone)]
pub enum BlockOp {
    Move { down: bool, unit: BlockUnit },
    DeleteBlock,
    PasteBlock { text: String, replace: bool },
    Depth { deeper: bool },
    ToggleTask { set: Option<bool> },
    Open { above: bool },
}

pub enum EditKind {
    /// One pre-resolved structural replacement (see [`BlockOp`]); refusals and the no-op
    /// guard are handled before the splice, like the number/transform edits.
    BlockEdit { op: BlockOp },
    /// Insert `text` at the cursor. With `replace_selection` the selection is replaced with
    /// `text` — a point cursor being the 1-char selection under the Normal-mode block, matching
    /// `DeleteSelection`. Without it, a point cursor (Insert-mode typing, paste-before) is a
    /// pure insert at `position` — no chars are replaced — and a range still replaces the
    /// selection (legacy paste-replace behaviour). When `select_pasted` is true and the
    /// inserted text is non-empty, the post-edit cursor selects the inserted text.
    ///
    /// `park_before` (the un-join gesture; open-above's "\n" insert): always a pure insert at
    /// `position` — an extended selection collapses rather than being replaced — and the cursor
    /// stays at the insertion point (its pre-edit coordinates, which now address the first
    /// inserted char) instead of advancing past the text. Lives here rather than as handler
    /// post-processing so the one `apply_edit` lock computes the final cursor before the
    /// viewport pushes are built — a post-hoc overwrite races the cursor embedded in those
    /// pushes.
    ReplaceWith {
        text: String,
        select_pasted: bool,
        replace_selection: bool,
        park_before: bool,
    },
    /// Delete the current inclusive selection. For a point cursor this deletes the 1 char at
    /// `position`. Used by Normal-mode `Ctrl-d` / `Delete` / `Ctrl-c`, and by Insert-mode
    /// `Delete` (forward).
    DeleteSelection,
    /// Change the current selection (Normal-mode `Ctrl-e`): same range as `DeleteSelection`,
    /// except a whole-line selection (the line-oriented normal form — anchor at col 0, cursor on
    /// the trailing newline) keeps its final newline, leaving one empty line to type into rather
    /// than joining onto the next line. The client enters Insert mode after the edit.
    ChangeSelection,
    /// Delete the char immediately before `cursor.position` and leave the cursor there. Used
    /// by Insert-mode `Backspace` — there's no meaningful selection in Insert mode and "delete
    /// the previous char" is its own gesture.
    Backspace,
    /// Delete from `cursor.position` to where a `count`-word motion in `direction` would land,
    /// leaving the cursor at the span's start. Insert-mode `Alt-Backspace` / `Alt-Delete`.
    /// Unlike [`EditKind::Backspace`] this is a plain motion span: no tab-stop snapping, and it
    /// crosses the line boundary when the motion does.
    DeleteWord {
        direction: Direction,
        boundary: WordBoundary,
        count: u32,
    },
    /// Delete the cursor's whole line — content and trailing newline. Insert-mode `Ctrl-d`.
    DeleteLine,
    /// Blank the cursor's line — content only, newline preserved. Insert-mode `Ctrl-e`.
    ChangeLine,
    /// Replace the cursor's line (content + newline) with `text`. Insert-mode `Ctrl-r`.
    ReplaceLine { text: String },
    /// Wrap the surround target with `open`…`close` (`Ctrl-s <delim>`). Modeled as a single replace
    /// of the target range with `open + <target text> + close` so it's one undo step. `line` selects
    /// the target: false → the selection (post-edit cursor re-selects the wrapped text), true → the
    /// cursor line's content (post-edit cursor collapses to a point past the close).
    Surround { open: char, close: char, line: bool },
    /// Strip the pair of chars hugging the surround target (`Ctrl-Alt-s`), replacing the outer range
    /// with the inner text. `line` matches `Surround`. `input_unsurround` guarantees a valid pair
    /// exists before issuing this — the no-op case never reaches here.
    Unsurround { line: bool },
    /// Shift the integer at/after the cursor by `delta` (`Ctrl-a` / `Ctrl-Alt-a`). The number is
    /// scanned from the selection's leading edge (or the point cursor) within its line and replaced
    /// in place; the post-edit cursor selects the whole result, so the selection tracks the digit
    /// count and repeated presses stay on the number. `input_increment_number` guarantees a number
    /// exists before issuing this — the no-op case never reaches here.
    AdjustNumber { delta: i64, scan: bool },
    /// Recase the operand (`Ctrl-r <key>`). The operand range + replacement text are resolved by
    /// `resolve_transform_case`: with `scan` (Insert mode) it recases the identifier under the
    /// caret (post-edit cursor collapses past the result); otherwise it recases exactly the
    /// selection — a point being the single char under the block — which stays selected, so
    /// transforms can be chained. `input_transform_case` prechecks for a no-op, so this resolves
    /// `Some` in practice; a stale `None` at apply time returns early, untouched.
    TransformCase { kind: CaseKind, scan: bool },
}

/// Where the cursor lands after an edit.
pub enum PostEdit {
    /// Collapse to a point just past the inserted text. The default for typing, deletes, and
    /// line-replace.
    PointAfter,
    /// Select the inserted text minus `lead` chars at the front and `trail` at the back. Paste uses
    /// `(0, 0)` to select everything; selection-surround uses `(1, 1)` to skip the delimiters.
    Select { lead: usize, trail: usize },
    /// Collapse to a point at this absolute char offset (clamped to the edited line's content).
    /// Line surround/unsurround use this to keep the caret on the same character it was on before
    /// the delimiters were inserted/removed around it.
    PointAt(usize),
    /// Land the selection at these absolute *byte* offsets of the post-edit document — the
    /// block edits' landing, computed by the resolver against the resulting text (bytes
    /// convert to positions only after the splice, on the new rope). `anchor == cursor`
    /// collapses.
    SelectAtBytes { anchor: usize, cursor: usize },
}

pub async fn apply_edit(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    edit: EditKind,
) -> Result<EditResult, RpcError> {
    apply_edit_reporting(state, client_id, buffer_id, edit, &mut None).await
}

/// [`apply_edit`], reporting what an [`EditKind::BlockEdit`] resolved to under this lock (see
/// [`BlockOutcome`]). Left untouched for every other edit kind.
pub async fn apply_edit_reporting(
    state: &SharedState,
    client_id: ClientId,
    buffer_id: BufferId,
    edit: EditKind,
    outcome: &mut Option<BlockOutcome>,
) -> Result<EditResult, RpcError> {
    // Phase 1: hold the lock for the whole edit; gather notification senders before dropping it.
    let mut s = state.lock().await;

    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    // Refuse before resolving the edit rather than after: a virtual buffer holds a revision's
    // content and there is nothing an edit against it could mean. This is the early-out, not the
    // guarantee — `ServerState::editable_doc` is what makes the refusal unskippable, here and in
    // the handlers that compute their own ranges instead of coming through this one.
    if buf.read_only() {
        return Err(RpcError::read_only_buffer(buffer_id));
    }
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    // Resolve the target number once for `AdjustNumber` so the range and the replacement text below
    // agree. `(start_char, end_char, line, new_text)` — absolute char offsets. `input_*_number`
    // prechecks that a number exists, so this is `Some` in practice; a stale `None` is caught by
    // the no-op guard below.
    let number_edit = match &edit {
        EditKind::AdjustNumber { delta, scan } => resolve_number_edit(buf, &cursor, *delta, *scan),
        _ => None,
    };

    // Likewise resolve the case-transform operand once so the range and replacement agree.
    // `input_transform_case` prechecks for a no-op, so this is `Some` in practice; `None` — a
    // stale precheck verdict, or `Randomize` re-rolling a tiny operand back to the original on
    // the fresh entropy it draws here — is caught by the no-op guard below.
    let transform_edit = match &edit {
        EditKind::TransformCase { kind, scan } => {
            resolve_transform_case(buf, &cursor, *kind, *scan)
        }
        _ => None,
    };

    // And the block edits: the handler's precheck (for refusal UX) already ran under an
    // earlier lock; this resolution — same parse, this lock — is the one the splice uses. A
    // refusal here is a stale precheck verdict, caught by the no-op guard.
    let block_edit = match &edit {
        EditKind::BlockEdit { op } => Some(resolve_block_edit(buf, &cursor, op).ok()),
        _ => None,
    };
    // This — not a revision delta — is what the RPC reports back: it is the verdict of the
    // resolution the splice below actually uses.
    if let Some(resolved) = &block_edit {
        *outcome = Some(BlockOutcome {
            applied: resolved.is_some(),
            clipboard: resolved.as_ref().and_then(|(_, clip)| clip.clone()),
        });
    }

    // A resolved no-op stops here, before any buffer mutation. Running it through the splice
    // below would bump the revision, clear the redo stack, and (outside the undo group window)
    // push a rope-snapshot undo entry — all for a zero-change edit — and `PostEdit::PointAfter`
    // would collapse a selection operand. Reached when a handler's precheck verdict went stale
    // (another client or a watcher reload edited the buffer between the precheck's lock and this
    // one) or when `Randomize` re-rolls a tiny operand back to the original.
    let resolved_noop = match &edit {
        EditKind::AdjustNumber { .. } => number_edit.is_none(),
        EditKind::TransformCase { .. } => transform_edit.is_none(),
        EditKind::BlockEdit { .. } => matches!(block_edit, Some(None)),
        _ => false,
    };
    if resolved_noop {
        let revision = buf.revision;
        let cursor = wrap_for_response(&s, client_id, buffer_id, cursor);
        return Ok(EditResult { revision, cursor });
    }

    // Compute the char range to replace and the affected line range. The range_is_inclusive
    // flag (selection mode) extends end_char by 1 to cover the cursor's char under the block.
    struct EditRange {
        start_char: usize,
        end_char: usize,
        first_line: u32,
        last_line: u32,
    }
    let range: EditRange = match &edit {
        EditKind::BlockEdit { .. } => match block_edit.as_ref().and_then(|b| b.as_ref()) {
            Some((be, _)) => {
                let sc = buf
                    .text
                    .byte_to_char(be.range.start.min(buf.text.len_bytes()));
                let ec = buf
                    .text
                    .byte_to_char(be.range.end.min(buf.text.len_bytes()));
                let lo = motion::char_to_pos(buf, sc);
                let hi = motion::char_to_pos(buf, ec.saturating_sub(1).max(sc));
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
            None => unreachable!("resolved no-op edits return early"),
        },
        EditKind::ReplaceWith {
            replace_selection,
            park_before,
            ..
        } => {
            // Without `replace_selection`, a point cursor is a genuine caret (Insert-mode
            // typing, paste-before) — pure insert. With it, the point is the 1-char selection
            // under the Normal-mode block and falls through to the selection-replace path.
            // `park_before` is always a pure insert: the un-join breaks *before* the block and
            // never consumes text, so an extended selection collapses instead of being replaced.
            if *park_before || (cursor.is_point() && !replace_selection) {
                // Pure insert at the point — no chars replaced.
                let c = motion::pos_to_char(buf, cursor.position);
                EditRange {
                    start_char: c,
                    end_char: c,
                    first_line: cursor.position.line,
                    last_line: cursor.position.line,
                }
            } else {
                let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
                let sc = motion::pos_to_char(buf, lo);
                let ec = motion::pos_to_char(buf, hi)
                    .saturating_add(1)
                    .min(buf.text.len_chars());
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
        EditKind::DeleteSelection => {
            let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
            let sc = motion::pos_to_char(buf, lo);
            let ec = motion::pos_to_char(buf, hi)
                .saturating_add(1)
                .min(buf.text.len_chars());
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::ChangeSelection => {
            let (lo, hi) = motion::ordered(cursor.position, cursor.anchor);
            let sc = motion::pos_to_char(buf, lo);
            let mut ec = motion::pos_to_char(buf, hi)
                .saturating_add(1)
                .min(buf.text.len_chars());
            // Whole-line selection (the line-oriented normal form): it starts at col 0 and its
            // cursor sits on a line's trailing newline — `pos_to_char` clamps col to the line's
            // content, so `ec - 1` is exactly that newline char. A *change* over whole lines
            // should leave one empty line to type into rather than deleting the final newline and
            // joining onto the next line, so drop it from the range. Multi-line whole-line
            // selections collapse to a single empty line for free: only the last line's newline is
            // at `ec`, and the interior newlines are deleted along with the content.
            if lo.col == 0 && ec > sc && buf.text.char(ec - 1) == '\n' {
                ec -= 1;
            }
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::Backspace => {
            // Inside a line's leading whitespace this steps back to the previous tab stop, so one
            // `Backspace` undoes one `input/tab`; everywhere else it stays a single char. The span
            // never crosses the line start, so the backward motion can't run onto the line above.
            let count = crate::indent::backspace_span(
                &buf.text,
                buf.indent_style,
                motion::pos_to_char(buf, cursor.position),
                client_tab_width(&s, client_id, buffer_id),
            ) as u32;
            let prev = motion::resolve_motion(
                buf,
                cursor.position,
                &Motion::Char {
                    direction: Direction::Backward,
                    count,
                },
            );
            let (lo, hi) = motion::ordered(cursor.position, prev);
            let sc = motion::pos_to_char(buf, lo);
            let ec = motion::pos_to_char(buf, hi);
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::DeleteWord {
            direction,
            boundary,
            count,
        } => {
            let target = motion::resolve_motion(
                buf,
                cursor.position,
                &Motion::Word {
                    direction: *direction,
                    count: (*count).max(1),
                    boundary: *boundary,
                },
            );
            let (lo, hi) = motion::ordered(cursor.position, target);
            EditRange {
                start_char: motion::pos_to_char(buf, lo),
                end_char: motion::pos_to_char(buf, hi),
                first_line: lo.line,
                last_line: hi.line,
            }
        }
        EditKind::DeleteLine | EditKind::ReplaceLine { .. } => {
            let line = cursor.position.line as usize;
            let total_lines = buf.text.len_lines();
            let sc = buf.text.line_to_char(line);
            let ec = if line + 1 < total_lines {
                buf.text.line_to_char(line + 1)
            } else {
                buf.text.len_chars()
            };
            EditRange {
                start_char: sc,
                end_char: ec,
                first_line: line as u32,
                last_line: line as u32,
            }
        }
        EditKind::ChangeLine => {
            let line = cursor.position.line as usize;
            let sc = buf.text.line_to_char(line);
            // Char count excluding the trailing newline, if any.
            let line_slice = buf.text.line(line);
            let len_chars = line_slice.len_chars();
            let has_trailing_nl = len_chars > 0 && line_slice.char(len_chars - 1) == '\n';
            let content_chars = if has_trailing_nl {
                len_chars - 1
            } else {
                len_chars
            };
            EditRange {
                start_char: sc,
                end_char: sc + content_chars,
                first_line: line as u32,
                last_line: line as u32,
            }
        }
        EditKind::Surround { line, .. } => {
            // The open/close chars are prepended/appended via insert_text below; the range here is
            // just the text being wrapped. Line target → the line's content; selection target →
            // the selection's char span [start, end).
            if *line {
                let l = cursor.position.line as usize;
                let (sc, ec) = line_content_char_range(buf, l);
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: l as u32,
                    last_line: l as u32,
                }
            } else {
                let (sc, ec) = current_selection_char_range(buf, &cursor);
                let lo = motion::char_to_pos(buf, sc);
                let hi = motion::char_to_pos(buf, ec.saturating_sub(1).max(sc));
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
        EditKind::Unsurround { line } => {
            // The range covers the delimiters plus the text between them; insert_text below drops
            // the first and last chars. `input_unsurround` has verified a real pair sits at those
            // ends, so the arithmetic can't underflow/overflow the buffer. Line target → the line's
            // full content (delimiters are its first/last chars); selection target → the selection
            // grown one char at each end to swallow the hugging delimiters.
            if *line {
                let l = cursor.position.line as usize;
                let (sc, ec) = line_content_char_range(buf, l);
                EditRange {
                    start_char: sc,
                    end_char: ec,
                    first_line: l as u32,
                    last_line: l as u32,
                }
            } else {
                let (sc, ec) = current_selection_char_range(buf, &cursor);
                let outer_start = sc - 1;
                let outer_end = ec + 1;
                let lo = motion::char_to_pos(buf, outer_start);
                let hi = motion::char_to_pos(buf, outer_end.saturating_sub(1));
                EditRange {
                    start_char: outer_start,
                    end_char: outer_end,
                    first_line: lo.line,
                    last_line: hi.line,
                }
            }
        }
        EditKind::AdjustNumber { .. } => match &number_edit {
            Some((sc, ec, line, _)) => EditRange {
                start_char: *sc,
                end_char: *ec,
                first_line: *line,
                last_line: *line,
            },
            None => unreachable!("resolved no-op edits return early"),
        },
        EditKind::TransformCase { .. } => match &transform_edit {
            Some(te) => EditRange {
                start_char: te.start_char,
                end_char: te.end_char,
                first_line: te.first_line,
                last_line: te.last_line,
            },
            None => unreachable!("resolved no-op edits return early"),
        },
    };
    // `insert_text` is what gets written over `[start_char, end_char)`; `post_edit` decides where
    // the cursor lands (see `PostEdit`).
    let (insert_text, post_edit): (Cow<str>, PostEdit) = match &edit {
        EditKind::BlockEdit { .. } => match block_edit.as_ref().and_then(|b| b.as_ref()) {
            Some((be, _)) => (
                Cow::Owned(be.text.clone()),
                PostEdit::SelectAtBytes {
                    anchor: be.anchor,
                    cursor: be.cursor,
                },
            ),
            None => unreachable!("resolved no-op edits return early"),
        },
        EditKind::ReplaceWith {
            text,
            select_pasted,
            park_before,
            ..
        } => (
            Cow::Borrowed(text.as_str()),
            if *select_pasted {
                PostEdit::Select { lead: 0, trail: 0 }
            } else if *park_before {
                // Stay at the insertion point — for the un-join, the '\n' just inserted there.
                PostEdit::PointAt(range.start_char)
            } else {
                PostEdit::PointAfter
            },
        ),
        EditKind::ReplaceLine { text } => (Cow::Borrowed(text.as_str()), PostEdit::PointAfter),
        EditKind::DeleteSelection
        | EditKind::ChangeSelection
        | EditKind::Backspace
        | EditKind::DeleteWord { .. }
        | EditKind::DeleteLine
        | EditKind::ChangeLine => (Cow::Borrowed(""), PostEdit::PointAfter),
        EditKind::Surround { open, close, line } => {
            let inner: String = buf
                .text
                .slice(range.start_char..range.end_char)
                .chars()
                .collect();
            let mut wrapped =
                String::with_capacity(inner.len() + open.len_utf8() + close.len_utf8());
            wrapped.push(*open);
            wrapped.push_str(&inner);
            wrapped.push(*close);
            // Selection target re-selects the inner text (skip the 1-char delimiters). Line target
            // keeps the caret on the same char: the open delimiter is inserted before it, so shift
            // the pre-edit caret right by one.
            let post = if *line {
                PostEdit::PointAt(motion::pos_to_char(buf, cursor.position) + 1)
            } else {
                PostEdit::Select { lead: 1, trail: 1 }
            };
            (Cow::Owned(wrapped), post)
        }
        EditKind::Unsurround { line } => {
            // The inner text is everything between the stripped delimiters — the outer range minus
            // one char at each end — for both targets.
            let inner: String = buf
                .text
                .slice(range.start_char + 1..range.end_char - 1)
                .chars()
                .collect();
            // Selection target re-selects the inner text. Line target keeps the caret on the same
            // char: the open delimiter before it is removed, so shift the pre-edit caret left by one
            // (clamped to the line content start below).
            let post = if *line {
                PostEdit::PointAt(motion::pos_to_char(buf, cursor.position).saturating_sub(1))
            } else {
                PostEdit::Select { lead: 0, trail: 0 }
            };
            (Cow::Owned(inner), post)
        }
        EditKind::AdjustNumber { scan, .. } => match &number_edit {
            // Normal mode (`!scan`): keep the whole re-rendered number selected. `Select` spans the
            // entire replacement, so the selection tracks the digit count automatically (`9` → `10`
            // grows, `100` → `99` shrinks) and repeated presses stay on the number. Insert mode
            // (`scan`) collapses past the result — it has no selection and must not spring one.
            Some((_, _, _, text)) => {
                let post = if *scan {
                    PostEdit::PointAfter
                } else {
                    PostEdit::Select { lead: 0, trail: 0 }
                };
                (Cow::Owned(text.clone()), post)
            }
            None => unreachable!("resolved no-op edits return early"),
        },
        EditKind::TransformCase { .. } => match &transform_edit {
            // A selection operand stays selected (chain transforms / see what changed); a
            // scanned operand collapses past the result so Insert mode keeps its `anchor ==
            // position` invariant rather than springing a selection.
            Some(te) => {
                let post = if te.scanned {
                    PostEdit::PointAfter
                } else {
                    PostEdit::Select { lead: 0, trail: 0 }
                };
                (Cow::Owned(te.new_text.clone()), post)
            }
            None => unreachable!("resolved no-op edits return early"),
        },
    };

    let start_char = range.start_char;
    let end_char = range.end_char;
    let old_first_line = range.first_line;
    let old_last_line = range.last_line;
    let kind_tag = match &edit {
        EditKind::ReplaceWith { .. }
        | EditKind::ReplaceLine { .. }
        | EditKind::AdjustNumber { .. } => EditKindTag::Text,
        EditKind::BlockEdit { op } => match op {
            BlockOp::DeleteBlock => EditKindTag::Delete,
            _ => EditKindTag::Text,
        },
        EditKind::DeleteSelection
        | EditKind::ChangeSelection
        | EditKind::Backspace
        | EditKind::DeleteWord { .. }
        | EditKind::DeleteLine
        | EditKind::ChangeLine => EditKindTag::Delete,
        EditKind::Surround { .. } | EditKind::Unsurround { .. } => EditKindTag::Surround,
        EditKind::TransformCase { .. } => EditKindTag::Transform,
    };

    // Snapshot all per-client cursors on this buffer so the undo entry can restore them.
    let cursors_before = document_cursor_snapshot(&s, buffer_id);

    // Mutate the buffer (rope edit + incremental reparse + undo-group bookkeeping).
    let mut buf_mut = s.editable_doc(buffer_id)?;
    let was_dirty = buf_mut.dirty;
    let revision = buf_mut.apply_edit(start_char, end_char, &insert_text, kind_tag, cursors_before);

    // Compute the cursor's new position.
    let inserted_char_count = insert_text.chars().count();
    // A `Select` span only holds if it leaves a non-empty range (lead + trail < inserted count);
    // otherwise it degrades to a point just past the insert.
    let selection = match post_edit {
        PostEdit::Select { lead, trail } if lead + trail < inserted_char_count => {
            Some((lead, trail))
        }
        _ => None,
    };
    let new_cursor_state = if let PostEdit::SelectAtBytes { anchor, cursor } = post_edit {
        // The block edits' landing: absolute bytes of the post-edit document, converted on
        // the new rope (they were computed against the resulting text).
        let clamp = |b: usize| buf_mut.text.byte_to_char(b.min(buf_mut.text.len_bytes()));
        let anchor_pos = motion::char_to_pos(&buf_mut, clamp(anchor));
        let position_pos = motion::char_to_pos(&buf_mut, clamp(cursor));
        CursorState {
            position: position_pos,
            anchor: anchor_pos,
            match_bracket: None,
            jumplist_position: None,
        }
    } else if let Some((lead, trail)) = selection {
        // Select the inserted span. Block cursor on its last char.
        let anchor_char = start_char + lead;
        let last_char = start_char + inserted_char_count - 1 - trail;
        let anchor_pos = motion::char_to_pos(&buf_mut, anchor_char);
        let position_pos = motion::char_to_pos(&buf_mut, last_char);
        CursorState {
            position: position_pos,
            anchor: anchor_pos,
            match_bracket: None,
            jumplist_position: None,
        }
    } else {
        // Point cursor. `PointAt` keeps the caret on the same char (clamped to the edited line's
        // content); everything else lands just past the inserted text.
        let point_char = match post_edit {
            PostEdit::PointAt(c) => c.clamp(start_char, buf_mut.text.len_chars()),
            _ => start_char + inserted_char_count,
        };
        let post_pos = motion::char_to_pos(&buf_mut, point_char);
        CursorState {
            position: post_pos,
            anchor: post_pos,
            match_bracket: None,
            jumplist_position: None,
        }
    };
    set_cursor(&mut s, (client_id, buffer_id), new_cursor_state);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    // Recompute every active search on this buffer so the embedded `search_matches` in the
    // line-render data we're about to send out reflects the post-edit text.
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));

    // Recompute every viewport's pushed range against the new line count, so a mutation that
    // *grew* the buffer (e.g. typing a newline) extends the window to cover the new lines.
    let new_line_count = s.doc_of(buffer_id).line_count();
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id, new_line_count);

    // Collect notifications for all viewports whose pushed range intersects the edit.
    let edit_first = old_first_line;
    let edit_last_excl = old_last_line.saturating_add(1);
    let pushes: PendingPushes =
        collect_doc_edit_pushes(&s, buffer_id, revision, edit_first, edit_last_excl);

    // Re-push any open Buffers pickers only when the dirty flag flipped (typically the first
    // edit after a save). The picker row renders dirty + display only, so per-keystroke edits
    // mid-burst don't need pushes.
    let picker_pushes = maybe_refresh_dirty(&mut s, buffer_id, was_dirty);

    // LSP: full-document sync.
    notify_lsp_change(&mut s, buffer_id);

    let new_cursor_state = wrap_for_response(&s, client_id, buffer_id, new_cursor_state);
    drop(s);

    for (sender, notif) in pushes {
        // If the receiver's gone, the client's connection has dropped; not our problem.
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in search_summary_pushes {
        let _ = sender.send(notif).await;
    }
    for (sender, notif) in picker_pushes {
        let _ = sender.send(notif).await;
    }

    Ok(EditResult {
        revision,
        cursor: new_cursor_state,
    })
}

pub fn ranges_overlap(a_start: u32, a_end_excl: u32, b_start: u32, b_end_excl: u32) -> bool {
    a_start < b_end_excl && b_start < a_end_excl
}

/// The edit-push range gate's alternative: queue a revision-only `buffer/changed` for a viewport
/// the edit didn't intersect, so a whole-document consumer (the markdown reading view) still hears
/// about every mutation without a window render. Clients that draw the pushed window ignore the
/// notification.
pub fn push_buffer_changed(
    s: &ServerState,
    vp: &Viewport,
    buffer_id: BufferId,
    revision: Revision,
    pushes: &mut PendingPushes,
) {
    let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
        return;
    };
    pushes.push((
        sender,
        Notification {
            jsonrpc: JsonRpc,
            method: BufferChanged::NAME.into(),
            params: serde_json::to_value(BufferChangedParams {
                buffer_id,
                revision,
            })
            .expect("infallible"),
        },
    ));
}

/// Workspace-switcher candidates: every persisted workspace (`names`, read from disk by the caller)
/// plus every *live* ephemeral workspace currently in memory. The ephemeral entries carry their id
/// as `name` — the client renders an ephemeral id as "(no workspace)". They appear only while they
/// hold a buffer, since an ephemeral workspace is auto-removed once its last buffer closes.
pub fn workspace_candidates(
    s: &ServerState,
    names: &[String],
) -> Vec<picker_state::WorkspaceCandidate> {
    // Reorder the alphabetical disk listing into most-recently-activated-first using the persisted
    // session stamps. Never-activated workspaces stay at the end in alphabetical order (see
    // `sort_names_by_recency`). Disabled (left alphabetical) when sessions aren't persisted — tests
    // and embeddings with no `sessions_path` — or if the file can't be read.
    let mut names: Vec<String> = names.to_vec();
    if let Some(path) = &s.sessions_path {
        if let Ok(sessions) = crate::config::load_workspace_sessions_at(path) {
            crate::config::sort_names_by_recency(&mut names, &sessions);
        }
    }
    // One row per workspace. There is no second tier: a workspace bound to a worktree is still that
    // one workspace, on different roots.
    let mut out: Vec<picker_state::WorkspaceCandidate> = names
        .iter()
        .map(|name| picker_state::WorkspaceCandidate {
            unsaved_buffers: s.unsaved_buffer_count(name),
            name: name.clone(),
        })
        .collect();
    let mut ephemeral: Vec<String> = s
        .workspaces
        .values()
        .filter(|p| p.is_ephemeral())
        .map(|p| p.id.clone())
        .collect();
    ephemeral.sort();
    for id in ephemeral {
        out.push(picker_state::WorkspaceCandidate {
            unsaved_buffers: s.unsaved_buffer_count(&id),
            name: id,
        });
    }
    out
}

/// The authoritative cursor to ride a `viewport/lines_changed` push for this viewport's client,
/// decorated the same way RPC responses are (`wrap_for_response`). `None` when the client has no
/// cursor on the buffer — the client then keeps its local state.
pub fn lines_changed_cursor(s: &ServerState, vp: &Viewport) -> Option<CursorState> {
    let cursor = s.cursors.get(&(vp.client_id, vp.buffer_id)).copied()?;
    Some(wrap_for_response(s, vp.client_id, vp.buffer_id, cursor))
}

/// Full-window `viewport/lines_changed` pushes for every viewport on any buffer of `buffer_id`'s
/// document — the post-mutation broadcast for whole-document changes (undo/redo, format, revert,
/// reload), where the rope was swapped wholesale. Decorations (search, hunks, diagnostics, git
/// status) resolve per *viewport's* buffer, so each workspace's view renders its own overlays over
/// the shared content. Range-gated `viewport/lines_changed` pushes for every viewport on any buffer
/// of `buffer_id`'s document, after an in-place edit touching logical lines `[edit_first,
/// edit_last_excl)`. A viewport whose pushed range misses the edit gets a revision-only
/// `buffer/changed` instead — whole-document consumers still need the change signal. Decorations
/// resolve per *viewport's* buffer, so each workspace's view renders its own overlays.
pub fn collect_doc_edit_pushes(
    s: &ServerState,
    buffer_id: BufferId,
    revision: Revision,
    edit_first: u32,
    edit_last_excl: u32,
) -> PendingPushes {
    let mut pushes: PendingPushes = Vec::new();
    let Some(doc) = s.try_doc_of(buffer_id) else {
        return pushes;
    };
    let attached = s.doc_siblings(buffer_id);
    for vp in s.viewports.values() {
        if !attached.contains(&vp.buffer_id) {
            continue;
        }
        if !vp.diff_view
            && !ranges_overlap(
                vp.first_logical_line,
                vp.last_logical_line_exclusive,
                edit_first,
                edit_last_excl,
            )
        {
            push_buffer_changed(s, vp, vp.buffer_id, revision, &mut pushes);
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, vp.buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                doc,
                vp,
                revision,
                search,
                buffer_both_hunks(s, vp.buffer_id),
                buffer_conflicts(s, vp.buffer_id),
                buffer_diagnostics(s, vp.buffer_id),
                buffer_git_status(s, vp.buffer_id),
                lines_changed_cursor(s, vp),
            ),
        ));
    }
    pushes
}

/// Clamp every cursor on any buffer of `buffer_id`'s document into the current rope — the
/// post-whole-document-replacement fixup (format, revert, reload, undo/redo fallback).
pub fn clamp_doc_cursors(s: &mut ServerState, buffer_id: BufferId) {
    let attached = s.doc_siblings(buffer_id);
    let keys: Vec<(ClientId, BufferId)> = s
        .cursors
        .keys()
        .filter(|(_, b)| attached.contains(b))
        .copied()
        .collect();
    for key in keys {
        if let Some(cur) = s.cursors.get(&key).copied() {
            let clamped = clamp_cursor(s.doc_of(key.1), cur);
            set_cursor(s, key, clamped);
        }
    }
}

pub fn collect_doc_lines_changed_pushes(
    s: &ServerState,
    buffer_id: BufferId,
    revision: Revision,
) -> PendingPushes {
    let mut pushes: PendingPushes = Vec::new();
    let Some(doc) = s.try_doc_of(buffer_id) else {
        return pushes;
    };
    let attached = s.doc_siblings(buffer_id);
    for vp in s.viewports.values() {
        if !attached.contains(&vp.buffer_id) {
            continue;
        }
        let Some(sender) = s.clients.get(&vp.client_id).map(|c| c.outbound.clone()) else {
            continue;
        };
        let search = s.searches.get(&(vp.client_id, vp.buffer_id));
        pushes.push((
            sender,
            build_lines_changed_notif(
                doc,
                vp,
                revision,
                search,
                buffer_both_hunks(s, vp.buffer_id),
                buffer_conflicts(s, vp.buffer_id),
                buffer_diagnostics(s, vp.buffer_id),
                buffer_git_status(s, vp.buffer_id),
                lines_changed_cursor(s, vp),
            ),
        ));
    }
    pushes
}

#[allow(clippy::too_many_arguments)] // one notification builder, 12 call sites
pub fn build_lines_changed_notif(
    buffer: &Document,
    vp: &Viewport,
    revision: Revision,
    search: Option<&SearchEntry>,
    hunks: &[crate::git::DiffHunk],
    conflicts: &[crate::git::ConflictRegion],
    diagnostics: &[crate::lsp::diagnostics::BufferDiagnostic],
    git_status: Option<GitBufferStatus>,
    cursor: Option<CursorState>,
) -> Notification {
    let line_count = buffer.line_count();
    let new_first = vp.first_logical_line.min(line_count);
    let new_last_excl = vp
        .last_logical_line_exclusive
        .min(line_count)
        .max(new_first);
    let window = render_window(
        buffer,
        new_first,
        new_last_excl,
        vp.wrap_geometry(),
        vp.rows,
        WindowDecorations {
            search,
            // Post-edit / async broadcast path: a sneak session can't coexist with an edit by the
            // same client, so labels never ride this render. They reappear on the next sneak/update.
            sneak: None,
            diff_view: vp.diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status,
        },
    );
    let params = ViewportLinesChangedParams {
        viewport_id: vp.id,
        revision,
        range: LogicalLineRange {
            start_logical_line: vp.first_logical_line,
            end_logical_line_exclusive: vp.last_logical_line_exclusive,
        },
        total_visual_rows: window.total_visual_rows,
        first_visual_row: window.first_visual_row,
        max_line_width: window.max_line_width,
        replacement_lines: window.lines,
        line_count,
        max_scroll_logical_line: window.max_scroll_logical_line,
        git_status: window.git_status,
        cursor,
    };
    Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: serde_json::to_value(params).expect("infallible"),
    }
}
