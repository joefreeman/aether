//! `input/*` — text insertion, deletion, indent, comment, surround and case transforms.

use super::*;

pub async fn input_text(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputTextParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    // Composite pre-step: collapse to the requested selection edge before inserting — the same
    // state changes as a `cursor/set`.
    if let Some(edge) = params.at {
        let mut s = state.lock().await;
        let scope = s.motion_scope(client_id, params.buffer_id)?;
        let key = (client_id, params.buffer_id);
        let current = s.cursors.get(&key).copied().unwrap_or_default();
        let pos = motion::resolve_selection_edge(&scope, current.position, current.anchor, edge);
        let collapsed = CursorState {
            position: pos,
            anchor: pos,
            match_bracket: None,
            jumplist_position: None,
        };
        set_cursor(&mut s, key, collapsed);
        s.record_motion(key, current, collapsed);
        s.virtual_col.remove(&key);
        s.clear_tree_selection_history(client_id, params.buffer_id);
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith {
            text: params.text,
            select_pasted: params.select_pasted,
            replace_selection: params.replace_selection,
            park_before: false,
        },
    )
    .await
}

pub async fn input_delete(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(
            apply_edit(
                state,
                client_id,
                params.buffer_id,
                EditKind::DeleteSelection,
            )
            .await?,
        );
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn input_change(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(
            apply_edit(
                state,
                client_id,
                params.buffer_id,
                EditKind::ChangeSelection,
            )
            .await?,
        );
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn input_backspace(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::Backspace).await
}

/// `input/delete_word` — delete one word either side of the cursor (Insert-mode `Alt-Backspace`
/// / `Alt-Delete`).
///
/// The span is exactly what the matching word *motion* would traverse, so what `Alt-Backspace`
/// removes is what `b` would have skipped over — one rule for both, and no way for the delete and
/// the motion to disagree about where a word starts.
pub async fn input_delete_word(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputDeleteWordParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::DeleteWord {
            direction: params.direction,
            boundary: params.boundary,
            count: params.count,
        },
    )
    .await
}

/// `input/tab` — insert one indent step at the cursor (Insert-mode `Tab`).
///
/// The step comes from the buffer's `indent_style`, the same source `Enter`'s smart indent and
/// `input/indent` use, so a file never ends up with tabs from one key and spaces from another.
/// The insert goes through [`input_text`], so undo grouping, viewport pushes and cursor stamping
/// are identical to typing the whitespace by hand.
pub async fn input_tab(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let text = {
        let s = state.lock().await;
        let buf = s
            .try_doc_of(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        let char_idx = motion::pos_to_char(buf, cursor.position);
        crate::indent::step_at(
            &buf.text,
            buf.indent_style,
            char_idx,
            client_tab_width(&s, client_id, params.buffer_id),
        )
    };
    input_text(
        state,
        ctx,
        InputTextParams {
            buffer_id: params.buffer_id,
            text,
            select_pasted: false,
            replace_selection: false,
            at: None,
        },
    )
    .await
}

/// The tab width this client renders `buffer_id` at, for edits whose result has to line up with
/// what's on screen. Borrowed from any of the client's viewports on the buffer (they're all the
/// same in practice); 4 when it has none — an edit through a buffer it isn't viewing.
pub fn client_tab_width(s: &ServerState, client_id: ClientId, buffer_id: BufferId) -> u32 {
    s.viewports
        .values()
        .find(|v| s.view_of(v).binds(buffer_id) && v.client_id == client_id)
        .map(|v| v.tab_width)
        .unwrap_or(4)
}

pub async fn input_delete_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::DeleteLine).await
}

pub async fn input_change_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(state, client_id, params.buffer_id, EditKind::ChangeLine).await
}

pub async fn input_replace_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::input::InputReplaceLineParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceLine { text: params.text },
    )
    .await
}

pub async fn input_surround(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputSurroundParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    // An unrecognized delimiter key is a no-op — echo the current state unchanged.
    let Some((open, close)) = surround::open_close(params.delimiter) else {
        return current_edit_result(state, client_id, params.buffer_id).await;
    };
    let line = matches!(params.target, SurroundTarget::Line);
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::Surround { open, close, line },
    )
    .await
}

pub async fn input_unsurround(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputUnsurroundParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let line = matches!(params.target, SurroundTarget::Line);
    // No-op unless a known delimiter pair hugs the target. Checked up front so we never push a
    // no-op undo entry through `apply_edit`.
    {
        let s = state.lock().await;
        let buf = s
            .try_doc_of(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        let has_pair = if line {
            // A line's own delimiters are its first and last characters, so they are inside the
            // field whenever the line is — no scope test to make.
            line_has_enclosing_pair(buf, cursor.position.line as usize)
        } else {
            has_enclosing_pair(&s.motion_scope(client_id, params.buffer_id)?, &cursor)
        };
        if !has_pair {
            let revision = buf.revision;
            let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
            return Ok(EditResult {
                buffer: params.buffer_id,
                revision,
                cursor,
            });
        }
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::Unsurround { line },
    )
    .await
}

pub async fn input_transform_case(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputTransformCaseParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    // No-op (empty operand, or a transform that changes nothing) short-circuits up front so we
    // never push a no-op undo entry through `apply_edit`.
    let is_noop = {
        let s = state.lock().await;
        let scope = s.motion_scope(client_id, params.buffer_id)?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        resolve_transform_case(&scope, &cursor, params.kind, params.scan_at_cursor).is_none()
    };
    if is_noop {
        return current_edit_result(state, client_id, params.buffer_id).await;
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::TransformCase {
            kind: params.kind,
            scan: params.scan_at_cursor,
        },
    )
    .await
}

pub async fn edit_undo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: UndoRedoParams,
) -> Result<UndoResult, RpcError> {
    undo_redo_counted(state, ctx, params, UndoDirection::Undo).await
}

pub async fn edit_redo(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: UndoRedoParams,
) -> Result<UndoResult, RpcError> {
    undo_redo_counted(state, ctx, params, UndoDirection::Redo).await
}

/// `3u`: step the undo/redo stack `count` times, stopping early once it's exhausted (the
/// `applied: false` result is returned so the client still learns the final state).
async fn undo_redo_counted(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: UndoRedoParams,
    direction: UndoDirection,
) -> Result<UndoResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let r = apply_undo_or_redo(
            state,
            ctx,
            params.buffer_id,
            direction,
            params.collapse_selection,
        )
        .await?;
        let applied = r.applied;
        last = Some(r);
        if !applied {
            break;
        }
    }
    Ok(last.expect("count.max(1) iterations"))
}

pub async fn input_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last =
            Some(apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Indent).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

/// `input/open_line` — the open-line chains (cursor-park, edit, land) composed server-side from the
/// same handlers the clients used to call in sequence, so undo grouping, pushes, and cursor
/// stamping are identical.
pub async fn input_open_line(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputOpenLineParams,
) -> Result<EditResult, RpcError> {
    let line = {
        let s = state.lock().await;
        if !s.buffers.contains_key(&params.buffer_id) {
            return Err(RpcError::buffer_not_found(params.buffer_id));
        }
        s.cursors
            .get(&(ctx.client_id, params.buffer_id))
            .copied()
            .unwrap_or_default()
            .position
            .line
    };
    let park = |col: u32| {
        let target = LogicalPosition { line, col };
        CursorSetParams {
            buffer_id: params.buffer_id,
            position: target,
            anchor: target,
            granularity: Granularity::Char,
        }
    };
    match params.side {
        LineSide::Below => {
            // Park at the line's end, then newline + smart indent; land on the opened line.
            cursor_set(state, ctx, park(u32::MAX)).await?;
            input_newline_and_indent(
                state,
                ctx,
                InputNewlineAndIndentParams {
                    buffer_id: params.buffer_id,
                    park_before: false,
                },
            )
            .await
        }
        LineSide::Above => {
            // Park at col 0, then insert "\n" staying *before* it — the parked coordinates
            // address the new empty line after the insert, so the one edit both pushes the line
            // down and lands the cursor on the opened line. (Previously a post-edit cursor_move
            // stepped back up, but the edit's viewport pushes had already left carrying the
            // pre-step cursor — the same push/response disagreement un-join had.)
            cursor_set(state, ctx, park(0)).await?;
            apply_edit(
                state,
                ctx.client_id,
                params.buffer_id,
                EditKind::ReplaceWith {
                    text: "\n".into(),
                    select_pasted: false,
                    replace_selection: false,
                    park_before: true,
                },
            )
            .await
        }
    }
}

pub async fn input_newline_and_indent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputNewlineAndIndentParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let indent = {
        let s = state.lock().await;
        let buf = s
            .try_doc_of(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        compute_smart_indent(buf, cursor.position)
    };
    let mut text = String::with_capacity(indent.len() + 1);
    text.push('\n');
    text.push_str(&indent);
    // `park_before` (the un-join gesture) rides the edit itself: `apply_edit` inserts before
    // the block and parks the cursor at the '\n' under its one lock, so the cursor in the
    // viewport pushes and the one in the result can't disagree.
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::ReplaceWith {
            text,
            select_pasted: false,
            replace_selection: false,
            park_before: params.park_before,
        },
    )
    .await
}

/// Choose the indent to emit after `\n`. When the buffer's language has an `indents.scm`
/// query (vendored from Helix), runs the tree-sitter indent engine and multiplies its level
/// count by `INDENT_UNIT`. Otherwise falls back to copying the previous non-empty line's
/// leading whitespace.
///
/// The engine alone misses the very common "user just typed `fn foo {` and pressed Enter"
/// case: the parser hasn't seen a closing brace yet, so no `block` node exists and no
/// `@indent` fires. We patch this with a small heuristic floor — `prev_line_levels +
/// opener_bonus` — taken as `max` with the engine's answer. For complete code the engine
/// already produces the right number, so the heuristic is a no-op; for incomplete code it
/// recovers the level the parser couldn't.
fn compute_smart_indent(buf: &Document, cursor_pos: LogicalPosition) -> String {
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
        if n > 0 && line_slice.byte(n - 1) == b'\n' {
            n - 1
        } else {
            n
        }
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
    let trimmed = prefix.trim_end_matches([' ', '\t']);
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
fn previous_line_indent(buf: &Document, line_idx: usize) -> String {
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
    params: ToggleCommentParams,
) -> Result<EditResult, RpcError> {
    apply_toggle_comment(state, ctx, params.buffer_id, params.style, params.target).await
}

/// Toggle comment status on the operand. The style is explicit — the scope is never inferred
/// from the selection's shape (a point cursor is just a 1-char selection):
///
/// - `Line` (`Ctrl-y`): toggle the language's line prefix on every line the selection touches —
///   strip it when every non-blank covered line already starts with it, add it (aligned to the
///   smallest indent so prefixes line up) otherwise. Languages without a line form (markdown,
///   html, css) fall back to a block toggle over the covered lines' content, so the primary
///   key still works there.
/// - `Block` (`Ctrl-Alt-y`): wrap exactly the operand in the language's block tokens — the
///   selection for `SurroundTarget::Selection`, the caret line's content for
///   `SurroundTarget::Line` (Insert mode has no selection). Languages without a block form
///   make this a no-op: falling back to line comments would reintroduce the scope guessing
///   this split removes.
///
/// The block paths *unwrap* instead of wrapping when the cursor sits inside an existing
/// block-comment node (via tree-sitter), when the operand's text exactly equals a wrapped
/// span, or when the wrap tokens hug the operand on either side. A wrap re-selects the wrapped
/// content, which is exactly the state unwrap restores — so toggling twice is a no-op.
async fn apply_toggle_comment(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    style: CommentStyle,
    target: SurroundTarget,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    let (line_tok, block_tok) = buf
        .syntax
        .as_ref()
        .map(|sy| (sy.config.line_comment, sy.config.block_comment))
        .unwrap_or((None, None));
    if line_tok.is_none() && block_tok.is_none() {
        let revision = buf.revision;
        let response = wrap_for_response(&s, client_id, buffer_id, cursor);
        return Ok(EditResult {
            buffer: buffer_id,
            revision,
            cursor: response,
        });
    }

    // Selection / line range.
    let (start, end) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start.line, end.line);
    // Insert mode (`Line` target): post-edit cursors collapse to a point — there's no
    // selection to keep and the edit must not spring one.
    let collapse_selection = target == SurroundTarget::Line;
    // Preserve the selection's orientation across the block paths: a backward selection
    // (cursor before anchor) re-selects backward, so a double toggle (wrap then unwrap)
    // restores the exact pre-toggle cursor state, orientation included.
    let backward =
        (cursor.position.line, cursor.position.col) < (cursor.anchor.line, cursor.anchor.col);
    let oriented = |start_pos: LogicalPosition, end_pos: LogicalPosition| {
        let (anchor, position) = if backward {
            (end_pos, start_pos)
        } else {
            (start_pos, end_pos)
        };
        CursorState {
            position,
            anchor,
            match_bracket: None,
            jumplist_position: None,
        }
    };

    // Phase 1: decide the action.
    let line_strings: Vec<String> = (a..=b)
        .map(|i| buf.text.line(i as usize).chunks().collect())
        .collect();

    enum Plan {
        Noop,
        LineUncomment {
            prefix: &'static str,
        },
        LineComment {
            prefix: &'static str,
            min_indent: usize,
        },
        BlockUnwrap {
            start_char: usize,
            end_char_excl: usize,
            span: String,
            open: &'static str,
            close: &'static str,
        },
        BlockWrap {
            start_char: usize,
            end_char_excl: usize,
            open: &'static str,
            close: &'static str,
        },
    }

    // Unwrap detection for the block paths, given the operand's inclusive endpoints. Primary
    // detector: tree-sitter `comment` ancestor containing the cursor — handles the natural
    // "wrap, then re-toggle to unwrap" gesture. Fallbacks for grammars the detector misses
    // (incomplete parses, or no `comment` node for the wrap — e.g. markdown's embedded HTML
    // comments): the operand's text *exactly* equals a wrapped span, or the operand is the
    // *inner* content of a wrap (the tokens hug it on either side — what a wrap leaves
    // selected, so a double toggle strips its own wrap).
    let detect_block_unwrap = |operand: Option<(LogicalPosition, LogicalPosition)>,
                               open: &'static str,
                               close: &'static str|
     -> Option<(usize, usize, String)> {
        let text_at =
            |from: usize, to: usize| -> String { buf.text.slice(from..to).chunks().collect() };
        if let Some(syntax) = buf.syntax.as_ref() {
            let cursor_byte = buf
                .text
                .char_to_byte(motion::pos_to_char(buf, cursor.position));
            let source: String = buf.text.chunks().collect();
            if let Some((s, e)) = find_enclosing_block_comment(
                &syntax.tree,
                source.as_bytes(),
                cursor_byte,
                open,
                close,
            ) {
                // `find_enclosing_block_comment` works in bytes; the edit machinery below
                // works in chars. Convert, or any multi-byte char earlier in the buffer
                // shifts the strip range and corrupts the text.
                return Some((
                    buf.text.byte_to_char(s),
                    buf.text.byte_to_char(e),
                    source[s..e].to_string(),
                ));
            }
        }
        let (start_pos, end_pos) = operand?;
        let start_char = motion::pos_to_char(buf, start_pos);
        let end_char_excl = motion::pos_to_char(buf, end_pos)
            .saturating_add(1)
            .min(buf.text.len_chars());
        let span = text_at(start_char, end_char_excl);
        if span.starts_with(open) && span.ends_with(close) && span.len() >= open.len() + close.len()
        {
            return Some((start_char, end_char_excl, span));
        }
        // Hugging check — padded form first so the wrap's own `open + " "` wins over a bare
        // `open`. The comment tokens are ASCII, so char counts equal byte lengths.
        let lead = [format!("{open} "), open.to_string()]
            .into_iter()
            .find_map(|cand| {
                let n = cand.chars().count();
                (start_char >= n && text_at(start_char - n, start_char) == cand).then_some(n)
            })?;
        let trail = [format!(" {close}"), close.to_string()]
            .into_iter()
            .find_map(|cand| {
                let n = cand.chars().count();
                (end_char_excl + n <= buf.text.len_chars()
                    && text_at(end_char_excl, end_char_excl + n) == cand)
                    .then_some(n)
            })?;
        let (s, e) = (start_char - lead, end_char_excl + trail);
        Some((s, e, text_at(s, e)))
    };

    // A block toggle over `operand` (inclusive endpoints; `None` = nothing to wrap): unwrap
    // when the detector fires, wrap otherwise.
    let plan_block = |operand: Option<(LogicalPosition, LogicalPosition)>,
                      open: &'static str,
                      close: &'static str|
     -> Plan {
        if let Some((sc, ec, span)) = detect_block_unwrap(operand, open, close) {
            return Plan::BlockUnwrap {
                start_char: sc,
                end_char_excl: ec,
                span,
                open,
                close,
            };
        }
        match operand {
            None => Plan::Noop,
            Some((start_pos, end_pos)) => {
                let sc = motion::pos_to_char(buf, start_pos);
                let ec = motion::pos_to_char(buf, end_pos)
                    .saturating_add(1)
                    .min(buf.text.len_chars());
                if sc == ec {
                    Plan::Noop
                } else {
                    Plan::BlockWrap {
                        start_char: sc,
                        end_char_excl: ec,
                        open,
                        close,
                    }
                }
            }
        }
    };

    let plan = match style {
        CommentStyle::Line => {
            if let (Some(prefix), Some(c)) =
                (line_tok, classify_line_range(&line_strings, line_tok))
            {
                if c.all_commented {
                    Plan::LineUncomment { prefix }
                } else if c.any_nonblank {
                    Plan::LineComment {
                        prefix,
                        min_indent: c.min_indent,
                    }
                } else {
                    Plan::Noop
                }
            } else if let Some((open, close)) = block_tok {
                // No line form: block toggle at line granularity — the covered lines'
                // content, blank edge lines trimmed (wrapping a bare `\n` would merge lines).
                plan_block(lines_content_endpoints(buf, a, b), open, close)
            } else {
                Plan::Noop
            }
        }
        CommentStyle::Block => match block_tok {
            None => Plan::Noop,
            Some((open, close)) => {
                let operand = if target == SurroundTarget::Line {
                    // Insert mode: no selection — the caret line's content, skipping empty
                    // lines (wrapping a bare `\n` would merge the line with the next).
                    current_line_content_endpoints(buf, cursor.position.line)
                } else {
                    Some(motion::ordered(cursor.position, cursor.anchor))
                };
                plan_block(operand, open, close)
            }
        },
    };

    // Phase 2: materialize the edit. Each variant produces (edit_start_char, edit_end_char,
    // replacement_text, new_cursor).
    let edit: Option<(usize, usize, String, CursorState, u32, u32)> = match plan {
        Plan::Noop => None,
        Plan::LineUncomment { prefix } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) = build_line_uncomment(&line_strings, a, prefix);
            let nc =
                shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols, collapse_selection);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::LineComment { prefix, min_indent } => {
            let (start_char, end_char) = line_edit_char_range(buf, a, b);
            let (text, shifts, insert_cols) =
                build_line_comment(&line_strings, a, prefix, min_indent);
            let nc =
                shift_cursor_by_line_map(cursor, a, b, &shifts, &insert_cols, collapse_selection);
            Some((start_char, end_char, text, nc, a, b))
        }
        Plan::BlockUnwrap {
            start_char,
            end_char_excl,
            span,
            open,
            close,
        } => {
            // Strip `open` + optional inner space at the front, optional inner space + `close`
            // at the back. Replace the wrapped span with the inner content; re-select that
            // content.
            let inner_start = open.len();
            let inner_end = span.len() - close.len();
            let mut inner = &span[inner_start..inner_end];
            // Chars stripped *before* the inner content (`open` + an optional space) — the caret
            // shifts left by this much in Insert mode to track its character.
            let mut removed_lead = open.len();
            if inner.starts_with(' ') {
                inner = &inner[1..];
                removed_lead += 1;
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
            // Re-select the uncommented content (Normal mode). In Insert mode (`collapse_selection`)
            // there's no selection to spring: keep the caret on the character it was on by shifting
            // it left past the removed leading delimiter (clamped to the content). Single-line is
            // the only Insert-mode case; a multi-line strip falls back to the content end.
            let nc = if collapse_selection {
                let pos = if inner.contains('\n') {
                    new_position
                } else {
                    let content_end = start_pos.col + inner.chars().count() as u32;
                    let col = cursor
                        .position
                        .col
                        .saturating_sub(removed_lead as u32)
                        .clamp(start_pos.col, content_end);
                    aether_protocol::LogicalPosition {
                        line: start_pos.line,
                        col,
                    }
                };
                CursorState {
                    position: pos,
                    anchor: pos,
                    match_bracket: None,
                    jumplist_position: None,
                }
            } else {
                oriented(start_pos, new_position)
            };
            let last_line = motion::char_to_pos(buf, end_char_excl.saturating_sub(1)).line;
            Some((
                start_char,
                end_char_excl,
                new_text,
                nc,
                a.min(last_line),
                b.max(last_line),
            ))
        }
        Plan::BlockWrap {
            start_char,
            end_char_excl,
            open,
            close,
        } => {
            let selected: String = buf.text.slice(start_char..end_char_excl).chunks().collect();
            let new_text = format!("{open} {selected} {close}");
            // Compute new selection endpoints in (line, col) directly — `char_to_pos` on the
            // pre-edit buffer is wrong for post-edit char indices once the wrap spans lines.
            // Discriminate by whether the *selected text* contains a newline, not by whether
            // start_pos.line == end_pos.line: a selection ending exactly on the `\n` of its
            // line counts as single-line in (line, col) terms but produces multi-line output.
            let start_pos = motion::char_to_pos(buf, start_char);
            let newlines = selected.matches('\n').count() as u32;
            // Re-select the wrapped *content* (Normal mode), leaving the tokens outside the
            // selection — mirroring surround, and the exact state a following unwrap restores,
            // so a double toggle is a no-op. The `open + " "` prefix shifts the content right
            // on its first line; later lines keep their columns.
            let inner_start = aether_protocol::LogicalPosition {
                line: start_pos.line,
                col: start_pos.col + open.len() as u32 + 1,
            };
            let inner_end = {
                let last_byte_idx = selected.len() - 1;
                let prefix = &selected[..last_byte_idx];
                match prefix.rfind('\n') {
                    Some(last_nl) => aether_protocol::LogicalPosition {
                        line: start_pos.line + prefix.matches('\n').count() as u32,
                        col: (last_byte_idx - last_nl - 1) as u32,
                    },
                    None => aether_protocol::LogicalPosition {
                        line: start_pos.line,
                        col: inner_start.col + last_byte_idx as u32,
                    },
                }
            };
            // In Insert mode (`collapse_selection`) there's no selection to spring — a point
            // caret wraps its current line — so keep the caret on the character it was on:
            // the leading `open + " "` shifts everything after the wrap's start rightward, so
            // the caret moves right by that prefix. Single-line is the only Insert-mode case;
            // a multi-line wrap falls back to the content end.
            let nc = if collapse_selection {
                let pos = if newlines == 0 {
                    aether_protocol::LogicalPosition {
                        line: cursor.position.line,
                        col: cursor.position.col + open.len() as u32 + 1,
                    }
                } else {
                    inner_end
                };
                CursorState {
                    position: pos,
                    anchor: pos,
                    match_bracket: None,
                    jumplist_position: None,
                }
            } else {
                oriented(inner_start, inner_end)
            };
            let last_touched_line = start_pos.line + newlines;
            Some((
                start_char,
                end_char_excl,
                new_text,
                nc,
                a.min(start_pos.line),
                b.max(last_touched_line),
            ))
        }
    };

    let Some((start_char, end_char, new_text, new_cursor, edit_first, edit_last_incl)) = edit
    else {
        let revision = buf.revision;
        let response = wrap_for_response(&s, client_id, buffer_id, cursor);
        return Ok(EditResult {
            buffer: buffer_id,
            revision,
            cursor: response,
        });
    };

    let cursors_before = document_cursor_snapshot(&s, buffer_id);

    let was_dirty = s.doc_of(buffer_id).dirty;
    let revision = {
        let mut buf_mut = s.editable_doc(buffer_id)?;
        buf_mut.apply_edit(
            start_char,
            end_char,
            &new_text,
            EditKindTag::Text,
            cursors_before,
        )
    };
    // Re-clamp the new cursor against the post-edit buffer (positions computed above used the
    // pre-edit buffer; if the edit shortened lines, clamp_position keeps them legal).
    let new_cursor = {
        let buf_mut = s.try_doc_of_mut(buffer_id).expect("just checked");
        let mut c = new_cursor;
        c.position = motion::clamp_position(buf_mut, c.position);
        c.anchor = motion::clamp_position(buf_mut, c.anchor);
        c
    };
    set_cursor(&mut s, (client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_last_excl = edit_last_incl + 1;
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
    let pushes: PendingPushes = collect_doc_edit_pushes(&s, buffer_id, edit_first, edit_last_excl);

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
        buffer: buffer_id,
        revision,
        cursor: new_cursor,
    })
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
            if span.starts_with(open.as_bytes())
                && span.ends_with(close.as_bytes())
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
    Some(LineClassify {
        any_nonblank,
        all_commented,
        min_indent: min_indent.unwrap_or(0),
    })
}

/// Inclusive endpoints covering the content of lines `a..=b`: col 0 of the first non-blank
/// covered line through the last content char of the last non-blank one. Blank edge lines are
/// trimmed so a wrap never swallows a bare `\n` (which would merge lines); `None` when every
/// covered line is blank. A single-line range reduces to [`current_line_content_endpoints`].
fn lines_content_endpoints(
    buf: &Document,
    a: u32,
    b: u32,
) -> Option<(
    aether_protocol::LogicalPosition,
    aether_protocol::LogicalPosition,
)> {
    let first = (a..=b).find(|&l| motion::line_byte_len_excl_newline(buf, l) > 0)?;
    let last = (a..=b)
        .rev()
        .find(|&l| motion::line_byte_len_excl_newline(buf, l) > 0)?;
    Some((
        aether_protocol::LogicalPosition {
            line: first,
            col: 0,
        },
        aether_protocol::LogicalPosition {
            line: last,
            col: motion::line_byte_len_excl_newline(buf, last) - 1,
        },
    ))
}

/// Endpoints `(line_start, line_end_inclusive)` for the content of `line_idx`, excluding the
/// trailing newline. Used to give "wrap the current line" a sensible char range when no
/// selection exists in a block-only language. Returns `None` for empty lines so the caller
/// can skip — otherwise a wrap on an empty line would replace its lone `\n` and merge the
/// line with the next.
fn current_line_content_endpoints(
    buf: &Document,
    line_idx: u32,
) -> Option<(
    aether_protocol::LogicalPosition,
    aether_protocol::LogicalPosition,
)> {
    let end_col = motion::line_byte_len_excl_newline(buf, line_idx);
    if end_col == 0 {
        return None;
    }
    Some((
        aether_protocol::LogicalPosition {
            line: line_idx,
            col: 0,
        },
        aether_protocol::LogicalPosition {
            line: line_idx,
            col: end_col - 1,
        },
    ))
}

fn line_edit_char_range(buf: &Document, a: u32, b: u32) -> (usize, usize) {
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
        // Consume the *whole* marker run, not just the bare token: doc comments and banners
        // repeat the token's final char (`///`, `////`, `##`, `%%`), and leaving the surplus
        // behind turns `/// docs` into `/ docs`. Re-commenting adds the plain token back, so
        // a doc comment round-trips to a regular comment — that's the intent (the marker is
        // *all* comment syntax, none of it content). The tokens are ASCII, so byte counts
        // equal char counts.
        let marker_char = prefix
            .chars()
            .next_back()
            .expect("line-comment tokens are non-empty");
        let extra = after_prefix
            .chars()
            .take_while(|&c| c == marker_char)
            .count();
        let after_marker = &after_prefix[extra..];
        let (stripped_tail, removed) = if let Some(after_space) = after_marker.strip_prefix(' ') {
            (after_space, prefix.len() + extra + 1)
        } else {
            (after_marker, prefix.len() + extra)
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
    caret_follows_content: bool,
) -> CursorState {
    // When a selection exists, treat its endpoints asymmetrically so the selection *extends*
    // to cover any prefix we just added (rather than sliding with the content and leaving the
    // new prefix outside the selection). The lower endpoint stays put when it sits exactly at
    // the insert column; the upper endpoint shifts forward to follow the content.
    //
    // In Insert mode (`caret_follows_content`) there's no selection to grow — the lone caret
    // should stay glued to the character it was on, so it shifts forward even when it sits
    // exactly at the insert column (otherwise it'd be left behind, sitting on the new prefix).
    let lower = motion::ordered(cursor.position, cursor.anchor).0;

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
            if is_lower_endpoint && p.col == insert_col && !caret_follows_content {
                p.col
            } else {
                p.col.saturating_add(shift as u32)
            }
        } else {
            let removed = (-shift) as u32;
            let prefix_end = insert_col + removed;
            if p.col >= prefix_end {
                p.col - removed
            } else {
                insert_col
            }
        };
        aether_protocol::LogicalPosition { line: p.line, col }
    };

    // Don't clamp here; positions are post-edit, and the post-edit clamp at the call site
    // handles legality. Clamping against the pre-edit buffer would clip to shorter lines.
    let position_is_lower = lower == cursor.position;
    let position = shift_pos(cursor.position, position_is_lower);
    let anchor_is_lower = lower == cursor.anchor;
    let anchor = shift_pos(cursor.anchor, anchor_is_lower);
    CursorState {
        position,
        anchor,
        match_bracket: None,
        jumplist_position: None,
    }
}

pub async fn input_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: CountedEditParams,
) -> Result<EditResult, RpcError> {
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last =
            Some(apply_indent_or_dedent(state, ctx, params.buffer_id, IndentKind::Dedent).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

/// Resolve what an `AdjustNumber` by `delta` targets: `(start_char, end_char, line, new_text)` in
/// absolute char offsets, or `None` for a no-op. With `scan` (Insert mode) the operand is inferred
/// by scanning the caret's line for the number at/after the cursor (Vim `Ctrl-A`); without it
/// (Normal mode) the operand is exactly the selected chars (a point cursor being the single char
/// under the block). Either way the operand must be a strictly valid integer. Shared by the no-op
/// precheck in `adjust_number` and the edit itself in `apply_edit`.
pub fn resolve_number_edit(
    scope: &crate::cursor::Scope<'_>,
    cursor: &CursorState,
    delta: i64,
    scan: bool,
) -> Option<(usize, usize, u32, String)> {
    let buf = scope.doc();
    let (sc, ec) = if scan {
        // Insert mode: there's no selection, so infer the number by scanning the line. Outward
        // scanning is safe here precisely because there's no selection edge to respect.
        let line = cursor.position.line;
        let line_text: String = buf.text.line(line as usize).chars().collect();
        let (s_col, e_col) = crate::number::find_number(&line_text, cursor.position.col as usize)?;
        (
            motion::pos_to_char(
                buf,
                LogicalPosition {
                    line,
                    col: s_col as u32,
                },
            ),
            motion::pos_to_char(
                buf,
                LogicalPosition {
                    line,
                    col: e_col as u32,
                },
            ),
        )
    } else {
        // Normal mode: the operand is exactly the selected chars (a point cursor being the single
        // char under the block). No scanning — a `-` or extra digits that aren't selected stay
        // out, so the adjustment can never invert by sweeping up a sign.
        current_selection_char_range(buf, cursor)
    };
    // Bounded by the field, not the document. There is no live escape today — the insert-mode scan
    // is line-bounded and the normal-mode operand is the selection, whose anchor `cursor_move` now
    // clamps — but both of those are facts about the *callers*, and this took a `Document`, so
    // nothing here said so. A `Scope` is the same move `resolve_block_edit` and
    // `resolve_transform_case` already made: the check cannot be forgotten because it cannot be
    // skipped.
    let field = scope.byte_range();
    let (sb, eb) = (buf.text.char_to_byte(sc), buf.text.char_to_byte(ec));
    if sb < field.start || eb > field.end {
        return None;
    }
    let selected: String = buf.text.slice(sc..ec).chars().collect();
    crate::number::adjust_exact(&selected, delta)
        .map(|text| (sc, ec, motion::char_to_pos(buf, sc).line, text))
}

/// `Ctrl-a` / `Ctrl-Alt-a`: shift the cursor's number by `delta` in a single edit (so `3` +
/// `Ctrl-a` is one undo step adding 3). Prechecks for a number at/after the cursor so a miss is a
/// clean no-op rather than an empty undo entry, mirroring `input_unsurround`.
pub async fn input_adjust_number(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputAdjustNumberParams,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let delta = i64::from(params.delta);
    let scan = params.scan_at_cursor;
    {
        let s = state.lock().await;
        let buf = s
            .try_doc_of(params.buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
        let cursor = s
            .cursors
            .get(&(client_id, params.buffer_id))
            .copied()
            .unwrap_or_default();
        if resolve_number_edit(
            &s.motion_scope(client_id, params.buffer_id)?,
            &cursor,
            delta,
            scan,
        )
        .is_none()
        {
            let revision = buf.revision;
            let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
            return Ok(EditResult {
                buffer: params.buffer_id,
                revision,
                cursor,
            });
        }
    }
    apply_edit(
        state,
        client_id,
        params.buffer_id,
        EditKind::AdjustNumber { delta, scan },
    )
    .await
}

#[derive(Clone, Copy)]
enum IndentKind {
    Indent,
    Dedent,
}

/// Per-buffer-style soft indent. Selection's line range gets the prefix added (or stripped, on
/// dedent). Cursor and anchor are shifted by the per-line delta — on indent that's always
/// +unit.len; on dedent it's 0/-1/-unit.len depending on what was actually there to strip.
async fn apply_indent_or_dedent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    kind: IndentKind,
) -> Result<EditResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let indent = buf.indent_style.unit();
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();

    let (start, end) = motion::ordered(cursor.position, cursor.anchor);
    let (a, b) = (start.line, end.line);

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
            buffer: buffer_id,
            revision: buf.revision,
            cursor,
        });
    }

    let cursors_before = document_cursor_snapshot(&s, buffer_id);

    let was_dirty = s.doc_of(buffer_id).dirty;
    let (revision, new_cursor) = {
        let mut buf_mut = s.editable_doc(buffer_id)?;
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
            position: motion::clamp_position(&buf_mut, shift_pos(cursor.position)),
            anchor: motion::clamp_position(&buf_mut, shift_pos(cursor.anchor)),
            match_bracket: None,
            jumplist_position: None,
        };
        (revision, new_cursor)
    };
    set_cursor(&mut s, (client_id, buffer_id), new_cursor);
    s.clear_motion_history_for_buffer(buffer_id);
    s.clear_tree_selection_history_for_buffer(buffer_id);
    s.clear_virtual_col_for_buffer(buffer_id);

    let edit_first = a;
    let edit_last_excl = b + 1;
    let mut search_summary_pushes = promote_transient(&mut s, buffer_id);
    search_summary_pushes.extend(refresh_searches_for_buffer(&mut s, buffer_id));
    refresh_viewport_ranges_for_buffer(&mut s, buffer_id);
    let pushes: PendingPushes = collect_doc_edit_pushes(&s, buffer_id, edit_first, edit_last_excl);

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
        buffer: buffer_id,
        revision,
        cursor: new_cursor,
    })
}

pub async fn input_move_lines(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: InputMoveLinesParams,
) -> Result<EditResult, RpcError> {
    // The repeat loop lives server-side.
    let mut last = None;
    for _ in 0..params.count.max(1) {
        last = Some(input_move_lines_once(state, ctx, &params).await?);
    }
    Ok(last.expect("count.max(1) iterations"))
}

/// Candidate index of the hunk in `current_abs_path`'s file nearest at-or-after `cursor_line`, falling back
/// to that file's last hunk when the cursor sits past them all. `None` when the file has no changes
/// (the picker then opens at the top rather than jumping to an unrelated file). Used by
/// `picker/view`'s `center_on_cursor` to land the Git-changes picker on "where you are".
/// The patch row the cursor is **on**, else the first one after it, else the last.
///
/// The same "on or after, else the end" rule [`find_nearest_git_change`] uses, but over the whole
/// patch rather than within one file — a patch is a single scope. Every line of a change block is a
/// changed line, so the block's extent is `added + removed` (a placeholder occupies one line and
/// counts neither side, hence the floor).
pub fn find_nearest_patch_change(
    cands: &[picker_state::GitChangeCandidate],
    cursor_line: u32,
) -> Option<usize> {
    cands
        .iter()
        .position(|c| cursor_line < c.line + (c.added + c.removed).max(1))
        .or_else(|| cands.len().checked_sub(1))
}

pub fn find_nearest_git_change(
    cands: &[picker_state::GitChangeCandidate],
    current_abs_path: &str,
    cursor_line: u32,
) -> Option<usize> {
    // The file's hunks are a contiguous run in anchor order. Pick the first at-or-after the cursor;
    // if none, the last hunk of the file (the cursor is below every change).
    let mut last_in_file: Option<usize> = None;
    for (i, c) in cands.iter().enumerate() {
        if c.abs_path != current_abs_path {
            continue;
        }
        if c.line >= cursor_line {
            return Some(i);
        }
        last_in_file = Some(i);
    }
    last_in_file
}
