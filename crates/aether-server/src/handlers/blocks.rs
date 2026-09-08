//! Block edits for the markdown reading view — structural commands resolved against the shared `aether-markdown` parse.

use super::*;

/// Resolve a [`BlockOp`] against the buffer's current text: the shared `aether-markdown`
/// parse turns the selection's byte range into one replacement. `Err` is a refusal (quiet or
/// reasoned); the paired `String` is `delete_block`'s clipboard payload. The paragraph-unit
/// move skips the parse entirely — it is blank-line geometry, any file type.
pub fn resolve_block_edit(
    // The **scope**, not the document, and that is the whole guarantee. Every op here resolves
    // against `buf.text.to_string()` — the entire file — so a paragraph move in a patch view would
    // happily swap the cursor's paragraph with one the view never drew, and a task toggle would
    // reach a checkbox outside the hunk. Both are *mutations* leaving the element.
    //
    // Taking a `Scope` makes the check unskippable rather than remembered: there is no way to call
    // this without one, and the result is bounded before it is returned. Requiring the caller to
    // check would be one more thing to forget, in the one place where forgetting rewrites text the
    // user cannot see.
    scope: &crate::cursor::Scope,
    cursor: &CursorState,
    op: &BlockOp,
) -> Result<(aether_markdown::edit::BlockEdit, Option<String>), aether_markdown::edit::Refusal> {
    use aether_markdown::edit as md;
    let buf = scope.doc();
    let text = buf.text.to_string();
    let to_byte =
        |p: LogicalPosition| -> u32 { buf.text.char_to_byte(motion::pos_to_char(buf, p)) as u32 };
    let (a, b) = (to_byte(cursor.anchor), to_byte(cursor.position));
    let (min, max) = (a.min(b), a.max(b));
    if let BlockOp::Move {
        down,
        unit: BlockUnit::Paragraph,
    } = op
    {
        // The paragraph move skips the markdown parse (it is blank-line geometry, any file type)
        // and so returns before `bounded` below — it needs its own check, and this is the op the
        // census ranked second: `Ctrl-Alt-j` swapping the cursor's paragraph with one outside the
        // hunk.
        let field = scope.byte_range();
        return md::resolve_move_paragraph(&text, min, max, *down)
            .and_then(|e| {
                if e.range.start < field.start || e.range.end > field.end {
                    Err(md::Refusal::Quiet)
                } else {
                    Ok(e)
                }
            })
            .map(|e| (e, None));
    }
    let blocks = aether_markdown::parse(&text);
    let elements = aether_markdown::stops(&blocks);
    let bounded = |r: Result<
        (aether_markdown::edit::BlockEdit, Option<String>),
        aether_markdown::edit::Refusal,
    >| {
        r.and_then(|(edit, clip)| {
            let field = scope.byte_range();
            if edit.range.start < field.start || edit.range.end > field.end {
                // Reaches outside the window: a quiet refusal, the same answer the boundary cases
                // already give (the first block moving up, depth past the ladder's end).
                Err(aether_markdown::edit::Refusal::Quiet)
            } else {
                Ok((edit, clip))
            }
        })
    };
    match op {
        BlockOp::Move { down, .. } => bounded(
            md::resolve_move_block(&text, &blocks, &elements, min, max, *down).map(|e| (e, None)),
        ),
        BlockOp::DeleteBlock => bounded(
            md::resolve_delete(&text, &blocks, &elements, min, max)
                .map(|(e, clip)| (e, Some(clip))),
        ),
        BlockOp::PasteBlock {
            text: clip,
            replace,
        } => bounded(
            md::resolve_paste(&text, &blocks, &elements, min, max, clip, *replace)
                .map(|e| (e, None)),
        ),
        BlockOp::Depth { deeper } => bounded(
            md::resolve_depth(&text, &blocks, &elements, min, max, *deeper).map(|e| (e, None)),
        ),
        BlockOp::ToggleTask { set } => {
            bounded(md::resolve_toggle_task(&text, &elements, b, *set).map(|e| (e, None)))
        }
        BlockOp::Open { above } => bounded(
            md::resolve_open(&text, &blocks, &elements, min, max, *above).map(|e| (e, None)),
        ),
    }
}

/// What a block edit actually did, reported out of [`apply_edit`]: the verdict of the
/// resolution that ran *under the write lock*, not the handler's precheck. `applied` is false
/// when the precheck went stale and the re-resolution refused; `clipboard` is the text this
/// edit really removed. Inferring either from a revision delta would misreport whenever a
/// concurrent edit moved the revision in the same window.
#[derive(Debug, Default)]
pub struct BlockOutcome {
    pub applied: bool,
    pub clipboard: Option<String>,
}

/// Shared driver for the block-edit RPCs. The precheck under its own lock exists purely for the
/// refusal UX — only a refusal *here* carries the reason to toast — while what actually happened
/// is reported by `apply_edit`'s own resolution, which re-resolves under the write lock (the
/// number/transform edits' pattern). So a stale precheck degrades to a no-op, and a concurrent
/// edit landing between the two locks can't be mistaken for this one applying.
async fn block_edit_rpc(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    op: BlockOp,
) -> Result<BlockEditResult, RpcError> {
    let client_id = ctx.client_id;
    {
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
        let scope = s.motion_scope(client_id, buffer_id)?;
        if let Err(refusal) = resolve_block_edit(&scope, &cursor, &op) {
            let reason = match refusal {
                aether_markdown::edit::Refusal::Quiet => None,
                aether_markdown::edit::Refusal::Why(r) => Some(r.to_string()),
            };
            let cursor = wrap_for_response(&s, client_id, buffer_id, cursor);
            return Ok(BlockEditResult {
                buffer: buffer_id,
                applied: false,
                reason,
                revision,
                cursor,
                text: None,
            });
        }
    }
    let mut outcome = None;
    let r = apply_edit_reporting(
        state,
        client_id,
        buffer_id,
        EditKind::BlockEdit { op },
        &mut outcome,
    )
    .await?;
    let outcome = outcome.unwrap_or_default();
    Ok(BlockEditResult {
        buffer: buffer_id,
        applied: outcome.applied,
        reason: None,
        revision: r.revision,
        cursor: r.cursor,
        text: outcome.clipboard,
    })
}

/// The Markdown source of what the reading cursor has — [`aether_protocol::input::ElementSource`].
///
/// Bounded by the scope like every op here, for the same reason though this one writes nothing:
/// a prose element inside a composed view must not hand back text the view never drew. Clamped
/// rather than refused — a read has nothing to undo, and an empty answer is the honest one.
pub fn resolve_source(scope: &crate::cursor::Scope, cursor: &CursorState) -> String {
    let buf = scope.doc();
    let text = buf.text.to_string();
    let to_byte =
        |p: LogicalPosition| -> u32 { buf.text.char_to_byte(motion::pos_to_char(buf, p)) as u32 };
    let field = scope.byte_range();
    let clamp = |x: usize| x.clamp(field.start, field.end);
    if !cursor.is_point() {
        let (a, b) = (to_byte(cursor.anchor), to_byte(cursor.position));
        let (min, max) = (a.min(b), a.max(b));
        // Inclusive on both ends: the cursor's own char belongs to the range, and in whole-line
        // normal form that char is the terminating newline.
        let end = max as usize
            + text[max as usize..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        return text
            .get(clamp(min as usize)..clamp(end))
            .unwrap_or("")
            .to_string();
    }
    let blocks = aether_markdown::parse(&text);
    let stops = aether_markdown::stops(&blocks);
    let Some(idx) = aether_markdown::element_at(&stops, to_byte(cursor.position)) else {
        return String::new();
    };
    let span = stops[idx].span();
    text.get(clamp(span.start as usize)..clamp(span.end as usize))
        .unwrap_or("")
        .trim_end()
        .to_string()
}

/// `x` / `Alt-x`: the block selection after one step, as `(position, anchor)`.
///
/// The client's state machine moved here whole. It reads the selection it already has — whether a
/// partial range must snap whole before advancing, and which edge a plain press collapses to — and
/// that reading needs the block boundaries under the current bytes, which needs the text.
///
/// Scope-local throughout: the parse is of the **element's** slice, so its byte offsets are the
/// element's own and no answer can name a place outside it. The bound is the coordinate system
/// rather than a check afterwards.
pub fn resolve_select_block(
    scope: &crate::cursor::Scope,
    cursor: &CursorState,
    forward: bool,
    extend: bool,
    count: u32,
) -> Option<(LogicalPosition, LogicalPosition)> {
    use crate::cursor::{byte_of_local, char_of_local};
    let text = scope.text().to_string();
    let stops = aether_markdown::stops(&aether_markdown::parse(&text));
    let byte = |p: LogicalPosition| byte_of_local(&text, scope.char_of(p));
    let (a, p) = (byte(cursor.anchor), byte(cursor.position));
    let (min, max) = (a.min(p), a.max(p));
    let cursor_at_top = !cursor.is_point() && p < a;
    let (mut top, mut bottom) =
        aether_markdown::edit::selection_block_range(&text, &stops, min, max)?;
    let (ts, bs) = (stops[top].span(), stops[bottom].span());
    // Whether the selection already covers those blocks whole, at both ends.
    let mut whole = !cursor.is_point() && min <= ts.start && max + 1 >= bs.end;
    let mut fresh = cursor.is_point();
    let step = |idx: usize, fwd: bool| {
        aether_markdown::step_element(&stops, idx, fwd, aether_markdown::Stop::is_block)
    };
    for _ in 0..count.max(1) {
        if fresh {
            if !forward {
                // `Alt-x`'s first press selects the block *above*, saturating at the top.
                top = step(top, false).unwrap_or(top);
            }
            bottom = top;
            (fresh, whole) = (false, true);
            continue;
        }
        if !whole {
            // Snap before advancing: extend keeps the (now whole) range, a plain press collapses
            // to the direction's edge block.
            if !extend {
                if forward {
                    top = bottom;
                } else {
                    bottom = top;
                }
            }
            whole = true;
            continue;
        }
        if forward {
            let next = step(bottom, true).unwrap_or(bottom);
            if !extend {
                top = next;
            }
            bottom = next;
        } else {
            let prev = step(top, false).unwrap_or(top);
            if !extend {
                bottom = prev;
            }
            top = prev;
        }
    }
    let pos_of = |b: u32| scope.pos_of(char_of_local(&text, b));
    let edges = |idx: usize| {
        let s = stops[idx].span();
        // `end - 1` is the span's last byte: on the last content line whether or not the parser's
        // span takes in the trailing newline.
        (
            pos_of(s.start),
            pos_of(s.end.saturating_sub(1).max(s.start)),
        )
    };
    let (top_first, _) = edges(top);
    let (_, bottom_last) = edges(bottom);
    Some(if cursor_at_top {
        (top_first, bottom_last)
    } else {
        (bottom_last, top_first)
    })
}

/// `Ctrl-e`: the focused block(s) from their start to their last content char, as
/// `(position, anchor)`.
pub fn resolve_block_content(
    scope: &crate::cursor::Scope,
    cursor: &CursorState,
) -> Option<(LogicalPosition, LogicalPosition)> {
    use crate::cursor::{byte_of_local, char_of_local};
    let text = scope.text().to_string();
    let stops = aether_markdown::stops(&aether_markdown::parse(&text));
    let byte = |p: LogicalPosition| byte_of_local(&text, scope.char_of(p));
    let (a, p) = (byte(cursor.anchor), byte(cursor.position));
    let (top, bottom) =
        aether_markdown::edit::selection_block_range(&text, &stops, a.min(p), a.max(p))?;
    let start = stops[top].span().start;
    let end = aether_markdown::edit::block_content_end(&text, stops[bottom].span()).max(start);
    let pos_of = |b: u32| scope.pos_of(char_of_local(&text, b));
    Some((pos_of(end), pos_of(start)))
}

pub async fn element_select_block(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SelectBlockParams,
) -> Result<CursorState, RpcError> {
    let forward = params.direction == VerticalDirection::Down;
    let resolved = {
        let s = state.lock().await;
        let cursor = read_cursor(&s, ctx.client_id, params.buffer_id)?;
        let scope = s.motion_scope(ctx.client_id, params.buffer_id)?;
        match resolve_select_block(&scope, &cursor, forward, params.extend, params.count) {
            Some(pair) => pair,
            // Nothing to select: a document with no blocks. The cursor stands.
            None => {
                return Ok(wrap_for_response(
                    &s,
                    ctx.client_id,
                    params.buffer_id,
                    cursor,
                ))
            }
        }
    };
    apply_read_selection(state, ctx, params.buffer_id, resolved, Granularity::Line).await
}

pub async fn element_block_content(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<CursorState, RpcError> {
    let resolved = {
        let s = state.lock().await;
        let cursor = read_cursor(&s, ctx.client_id, params.buffer_id)?;
        let scope = s.motion_scope(ctx.client_id, params.buffer_id)?;
        match resolve_block_content(&scope, &cursor) {
            Some(pair) => pair,
            None => {
                return Ok(wrap_for_response(
                    &s,
                    ctx.client_id,
                    params.buffer_id,
                    cursor,
                ))
            }
        }
    };
    apply_read_selection(state, ctx, params.buffer_id, resolved, Granularity::Char).await
}

/// This client's cursor in `buffer_id`, or the default when it has none yet.
fn read_cursor(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Result<CursorState, RpcError> {
    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    Ok(s.cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default())
}

/// Apply a resolved reading selection through the ordinary cursor-set path, so the motion history,
/// the virtual column, the tree-selection reset and the search counter all update exactly as they
/// do for any other selection. Resolving here and applying there is what keeps this from being a
/// second way to move the cursor.
async fn apply_read_selection(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    buffer_id: BufferId,
    (position, anchor): (LogicalPosition, LogicalPosition),
    granularity: Granularity,
) -> Result<CursorState, RpcError> {
    super::cursor_set(
        state,
        ctx,
        CursorSetParams {
            buffer_id,
            position,
            anchor,
            granularity,
        },
    )
    .await
}

pub async fn element_source(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<ElementSourceResult, RpcError> {
    let client_id = ctx.client_id;
    let s = state.lock().await;
    s.try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let cursor = s
        .cursors
        .get(&(client_id, params.buffer_id))
        .copied()
        .unwrap_or_default();
    let scope = s.motion_scope(client_id, params.buffer_id)?;
    Ok(ElementSourceResult {
        text: resolve_source(&scope, &cursor),
    })
}

pub async fn input_move_block(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: MoveBlockParams,
) -> Result<BlockEditResult, RpcError> {
    let op = BlockOp::Move {
        down: params.direction == VerticalDirection::Down,
        unit: params.unit,
    };
    block_edit_rpc(state, ctx, params.buffer_id, op).await
}

pub async fn input_delete_block(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BufferOnlyParams,
) -> Result<BlockEditResult, RpcError> {
    block_edit_rpc(state, ctx, params.buffer_id, BlockOp::DeleteBlock).await
}

pub async fn input_paste_block(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: PasteBlockParams,
) -> Result<BlockEditResult, RpcError> {
    let op = BlockOp::PasteBlock {
        text: params.text,
        replace: params.replace,
    };
    block_edit_rpc(state, ctx, params.buffer_id, op).await
}

pub async fn input_block_depth(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: BlockDepthParams,
) -> Result<BlockEditResult, RpcError> {
    let op = BlockOp::Depth {
        deeper: params.deeper,
    };
    block_edit_rpc(state, ctx, params.buffer_id, op).await
}

pub async fn input_open_block(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: OpenBlockParams,
) -> Result<BlockEditResult, RpcError> {
    let op = BlockOp::Open {
        above: params.above,
    };
    block_edit_rpc(state, ctx, params.buffer_id, op).await
}

pub async fn input_toggle_task(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ToggleTaskParams,
) -> Result<BlockEditResult, RpcError> {
    let op = BlockOp::ToggleTask { set: params.set };
    block_edit_rpc(state, ctx, params.buffer_id, op).await
}
