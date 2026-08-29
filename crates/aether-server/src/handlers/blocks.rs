//! Block edits for the markdown reading view — structural commands resolved against the shared `aether-markdown` parse.

use super::*;

/// Resolve a [`BlockOp`] against the buffer's current text: the shared `aether-markdown`
/// parse turns the selection's byte range into one replacement. `Err` is a refusal (quiet or
/// reasoned); the paired `String` is `delete_block`'s clipboard payload. The paragraph-unit
/// move skips the parse entirely — it is blank-line geometry, any file type.
pub fn resolve_block_edit(
    buf: &Document,
    cursor: &CursorState,
    op: &BlockOp,
) -> Result<(aether_markdown::edit::BlockEdit, Option<String>), aether_markdown::edit::Refusal> {
    use aether_markdown::edit as md;
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
        return md::resolve_move_paragraph(&text, min, max, *down).map(|e| (e, None));
    }
    let blocks = aether_markdown::parse(&text);
    let elements = aether_markdown::elements(&blocks);
    match op {
        BlockOp::Move { down, .. } => {
            md::resolve_move_block(&text, &blocks, &elements, min, max, *down).map(|e| (e, None))
        }
        BlockOp::DeleteBlock => {
            md::resolve_delete(&text, &blocks, &elements, min, max).map(|(e, clip)| (e, Some(clip)))
        }
        BlockOp::PasteBlock {
            text: clip,
            replace,
        } => md::resolve_paste(&text, &blocks, &elements, min, max, clip, *replace)
            .map(|e| (e, None)),
        BlockOp::Depth { deeper } => {
            md::resolve_depth(&text, &blocks, &elements, min, max, *deeper).map(|e| (e, None))
        }
        BlockOp::ToggleTask { set } => {
            md::resolve_toggle_task(&text, &elements, b, *set).map(|e| (e, None))
        }
        BlockOp::Open { above } => {
            md::resolve_open(&text, &blocks, &elements, min, max, *above).map(|e| (e, None))
        }
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
        if let Err(refusal) = resolve_block_edit(buf, &cursor, &op) {
            let reason = match refusal {
                aether_markdown::edit::Refusal::Quiet => None,
                aether_markdown::edit::Refusal::Why(r) => Some(r.to_string()),
            };
            let cursor = wrap_for_response(&s, client_id, buffer_id, cursor);
            return Ok(BlockEditResult {
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
        applied: outcome.applied,
        reason: None,
        revision: r.revision,
        cursor: r.cursor,
        text: outcome.clipboard,
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
