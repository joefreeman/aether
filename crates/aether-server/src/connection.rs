//! Per-connection task: WebSocket framing, JSON-RPC frame dispatch.

use crate::error::RpcError;
use crate::handlers::{self, ConnectionCtx};
use crate::state::SharedState;
use aether_protocol::buffer::{BufferClose, BufferCopy, BufferCut, BufferOpen, BufferSave};
use aether_protocol::cursor::{
    CursorContract, CursorExpand, CursorMove, CursorRedo, CursorSelectLine, CursorSet,
    CursorSwapAnchor, CursorUndo,
};
use aether_protocol::directory::{DirectoryCreate, DirectoryList};
use aether_protocol::envelope::{
    ErrorObject, ErrorResponse, JsonRpc, Notification, Request, Response, RpcMethod,
};
use aether_protocol::handshake::ClientHello;
use aether_protocol::input::{
    InputBackspace, InputDedent, InputDelete, InputIndent, InputJoinLines, InputMoveLines,
    InputNewlineAndIndent, InputRedo, InputText, InputToggleComment, InputUndo,
};
use aether_protocol::picker::{PickerHide, PickerQuery, PickerSelect, PickerView};
use aether_protocol::search::{SearchClear, SearchNext, SearchPrev, SearchSet};
use aether_protocol::viewport::{
    ViewportResize, ViewportScroll, ViewportSetWrap, ViewportSubscribe, ViewportUnsubscribe,
};
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const OUTBOUND_CHANNEL_CAPACITY: usize = 64;

pub async fn handle(stream: TcpStream, state: SharedState) -> anyhow::Result<()> {
    let peer = stream.peer_addr().ok();
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .with_context(|| format!("WebSocket handshake from {peer:?}"))?;
    tracing::debug!(?peer, "WebSocket established");

    let (mut writer, mut reader) = ws.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Notification>(OUTBOUND_CHANNEL_CAPACITY);
    let mut ctx = ConnectionCtx {
        client_id: None,
        outbound_tx,
    };

    loop {
        tokio::select! {
            incoming = reader.next() => {
                let Some(msg) = incoming else { break };
                let msg = msg?;
                match msg {
                    Message::Text(text) => {
                        if let Some(reply) = process_text(&text, &state, &mut ctx).await {
                            writer.send(Message::text(reply)).await?;
                        }
                    }
                    Message::Binary(_) => {
                        tracing::warn!("ignoring unexpected binary frame");
                    }
                    Message::Close(_) => break,
                    Message::Ping(p) => writer.send(Message::Pong(p)).await?,
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            push = outbound_rx.recv() => {
                let Some(notif) = push else { continue };
                let json = serde_json::to_string(&notif)?;
                writer.send(Message::text(json)).await?;
            }
        }
    }

    if let Some(client_id) = ctx.client_id {
        let mut s = state.lock().await;
        s.clients.remove(&client_id);
        s.drop_viewports_for_client(client_id);
        s.drop_cursors_for_client(client_id);
        s.drop_motion_history_for_client(client_id);
        s.drop_virtual_col_for_client(client_id);
        s.drop_searches_for_client(client_id);
        s.drop_tree_selection_history_for_client(client_id);
        s.drop_last_scroll_for_client(client_id);
        s.drop_pickers_for_client(client_id);
        s.drop_mru_for_client(client_id);
        tracing::debug!(%client_id, "client session removed");
    }
    Ok(())
}

async fn process_text(text: &str, state: &SharedState, ctx: &mut ConnectionCtx) -> Option<String> {
    let request: Request = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse incoming frame as JSON-RPC request");
            return None;
        }
    };

    let id = request.id;
    let method = request.method.clone();
    let params = request.params.unwrap_or(Value::Null);

    let result = dispatch(state, ctx, &method, params).await;

    let envelope = match result {
        Ok(value) => serde_json::to_string(&Response {
            jsonrpc: JsonRpc,
            id,
            result: value,
        }),
        Err(err) => {
            tracing::debug!(%method, code = err.code, msg = %err.message, "request returned error");
            serde_json::to_string(&ErrorResponse {
                jsonrpc: JsonRpc,
                id,
                error: err.into(),
            })
        }
    };

    match envelope {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize response envelope");
            Some(internal_error_envelope(id))
        }
    }
}

fn internal_error_envelope(id: u64) -> String {
    let er = ErrorResponse {
        jsonrpc: JsonRpc,
        id,
        error: ErrorObject {
            code: aether_protocol::error::ErrorCode::INTERNAL_ERROR.code(),
            message: "response serialization failed".into(),
            data: None,
        },
    };
    serde_json::to_string(&er).unwrap_or_else(|_| String::from("{}"))
}

/// One-line per-method dispatch. Each arm: deserialize params, call handler, serialize result.
async fn dispatch(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    macro_rules! run {
        ($method_ty:ty, $handler:path) => {{
            let p: <$method_ty as RpcMethod>::Params = serde_json::from_value(params)?;
            let r = $handler(state, ctx, p).await?;
            serde_json::to_value(r).map_err(RpcError::internal)
        }};
    }

    match method {
        ClientHello::NAME => run!(ClientHello, handlers::client_hello),
        BufferOpen::NAME => run!(BufferOpen, handlers::buffer_open),
        BufferSave::NAME => run!(BufferSave, handlers::buffer_save),
        BufferClose::NAME => run!(BufferClose, handlers::buffer_close),
        DirectoryList::NAME => run!(DirectoryList, handlers::directory_list),
        DirectoryCreate::NAME => run!(DirectoryCreate, handlers::directory_create),
        SearchSet::NAME => run!(SearchSet, handlers::search_set),
        SearchClear::NAME => run!(SearchClear, handlers::search_clear),
        SearchNext::NAME => run!(SearchNext, handlers::search_next),
        SearchPrev::NAME => run!(SearchPrev, handlers::search_prev),
        BufferCopy::NAME => run!(BufferCopy, handlers::buffer_copy),
        BufferCut::NAME => run!(BufferCut, handlers::buffer_cut),
        ViewportSubscribe::NAME => run!(ViewportSubscribe, handlers::viewport_subscribe),
        ViewportResize::NAME => run!(ViewportResize, handlers::viewport_resize),
        ViewportScroll::NAME => run!(ViewportScroll, handlers::viewport_scroll),
        ViewportSetWrap::NAME => run!(ViewportSetWrap, handlers::viewport_set_wrap),
        ViewportUnsubscribe::NAME => run!(ViewportUnsubscribe, handlers::viewport_unsubscribe),
        CursorMove::NAME => run!(CursorMove, handlers::cursor_move),
        CursorSet::NAME => run!(CursorSet, handlers::cursor_set),
        CursorSelectLine::NAME => run!(CursorSelectLine, handlers::cursor_select_line),
        CursorSwapAnchor::NAME => run!(CursorSwapAnchor, handlers::cursor_swap_anchor),
        CursorUndo::NAME => run!(CursorUndo, handlers::cursor_undo),
        CursorExpand::NAME => run!(CursorExpand, handlers::cursor_expand),
        CursorContract::NAME => run!(CursorContract, handlers::cursor_contract),
        CursorRedo::NAME => run!(CursorRedo, handlers::cursor_redo),
        InputText::NAME => run!(InputText, handlers::input_text),
        InputDelete::NAME => run!(InputDelete, handlers::input_delete),
        InputBackspace::NAME => run!(InputBackspace, handlers::input_backspace),
        InputUndo::NAME => run!(InputUndo, handlers::input_undo),
        InputRedo::NAME => run!(InputRedo, handlers::input_redo),
        InputJoinLines::NAME => run!(InputJoinLines, handlers::input_join_lines),
        InputMoveLines::NAME => run!(InputMoveLines, handlers::input_move_lines),
        InputIndent::NAME => run!(InputIndent, handlers::input_indent),
        InputDedent::NAME => run!(InputDedent, handlers::input_dedent),
        InputNewlineAndIndent::NAME => {
            run!(InputNewlineAndIndent, handlers::input_newline_and_indent)
        }
        InputToggleComment::NAME => run!(InputToggleComment, handlers::input_toggle_comment),
        PickerView::NAME => run!(PickerView, handlers::picker_view),
        PickerQuery::NAME => run!(PickerQuery, handlers::picker_query),
        PickerSelect::NAME => run!(PickerSelect, handlers::picker_select),
        PickerHide::NAME => run!(PickerHide, handlers::picker_hide),
        other => Err(RpcError::method_not_found(other)),
    }
}
