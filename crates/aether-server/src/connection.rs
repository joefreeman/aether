//! Per-connection task: WebSocket framing, JSON-RPC frame dispatch.
//!
//! Authentication: the client passes `?token=<>&client_version=<>` on the WebSocket URL.
//! Token mismatch fails the upgrade with HTTP 401 *before* any JSON-RPC traffic flows; valid
//! tokens get an allocated `ClientId` immediately, so handlers can rely on it being set.
//!
//! Project selection: a connected client has no buffer-level capabilities until it calls
//! `project/activate`. The only RPCs that work without an active project are `project/list` and
//! `project/activate` itself; everything else returns `NO_ACTIVE_PROJECT`.

use crate::error::RpcError;
use crate::handlers::{self, ConnectionCtx};
use crate::state::{ClientSession, SharedState};
use aether_protocol::buffer::{
    BufferClose, BufferCopy, BufferCut, BufferOpen, BufferReload, BufferSave,
};
use aether_protocol::cursor::{
    CursorContract, CursorExpand, CursorMove, CursorRedo, CursorSelectLine, CursorSet,
    CursorSwapAnchor, CursorUndo,
};
use aether_protocol::directory::{DirectoryCreate, DirectoryList};
use aether_protocol::git::{
    GitBlameLine, GitCommitInfo, GitNavigateHunk, GitSetDiffBase, GitSetDiffView,
};
use aether_protocol::nav::{NavBack, NavForward, NavGoto, NavRecord};
use aether_protocol::lsp::{
    LspFormat, LspGotoDefinition, LspHover, LspNavigateDiagnostic, LspRestartServer,
    LspServerStatusList,
};
use aether_protocol::envelope::{
    ErrorObject, ErrorResponse, JsonRpc, Notification, Request, Response, RpcMethod,
};
use aether_protocol::input::{
    InputBackspace, InputChangeLine, InputDedent, InputDelete, InputDeleteLine, InputIndent,
    InputJoinLines, InputMoveLines, InputNewlineAndIndent, InputRedo, InputReplaceLine,
    InputSurround, InputText, InputToggleComment, InputUndo, InputUnsurround,
};
use aether_protocol::path::PathDelete;
use aether_protocol::picker::{
    PickerGrepFileJump, PickerGrepNavigate, PickerHide, PickerQuery, PickerSelect, PickerView,
};
use aether_protocol::project::{
    ProjectActivate, ProjectAddRoot, ProjectCreate, ProjectDelete, ProjectList, ProjectRemoveRoot,
    ProjectRename,
};
use aether_protocol::search::{SearchClear, SearchNext, SearchPrev, SearchSet};
use aether_protocol::viewport::{
    ViewportResize, ViewportScroll, ViewportScrollToRow, ViewportSetWrap, ViewportSubscribe,
    ViewportUnsubscribe,
};
use aether_protocol::ClientId;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse as HsErr, Request as HsReq, Response as HsResp};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const OUTBOUND_CHANNEL_CAPACITY: usize = 64;

/// Extracted from the WebSocket upgrade request's query string.
#[derive(Default)]
struct ConnectQuery {
    client_version: Option<String>,
}

impl ConnectQuery {
    /// Parse `?client_version=...` (and tolerate missing/extra params). URL-decoding is
    /// intentionally minimal — this value is produced by our own clients.
    fn parse(query: &str) -> Self {
        let mut out = Self::default();
        for kv in query.split('&') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            if k == "client_version" {
                out.client_version = Some(v.to_string());
            }
        }
        out
    }
}

pub async fn handle(stream: TcpStream, state: SharedState) -> anyhow::Result<()> {
    let peer = stream.peer_addr().ok();

    // Authorization is by loopback identity, not a token. The server binds `127.0.0.1`, so off-host
    // traffic can't reach it; the remaining browser threat is a malicious site connecting (or, via
    // DNS rebinding, reading our page). We defend with two header checks:
    //   * `Host` must name our loopback authority — a rebound request carries the attacker's host.
    //   * `Origin`, if present, must be our loopback origin. Browsers set `Origin` honestly and
    //     can't forge it cross-site; the native TUI sends none, which is allowed.
    let mut query = ConnectQuery::default();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &HsReq, resp: HsResp| -> Result<HsResp, HsErr> {
            query = ConnectQuery::parse(req.uri().query().unwrap_or(""));
            let headers = req.headers();
            let host_ok = headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .is_some_and(crate::http::is_loopback_authority);
            let origin_ok = match headers.get("origin") {
                None => true,
                Some(o) => o
                    .to_str()
                    .is_ok_and(crate::http::is_loopback_authority),
            };
            if host_ok && origin_ok {
                Ok(resp)
            } else {
                tracing::warn!(?peer, host_ok, origin_ok, "rejecting connection: non-loopback host/origin");
                let mut err = HsErr::new(Some("forbidden".into()));
                *err.status_mut() = StatusCode::FORBIDDEN;
                Err(err)
            }
        },
    )
    .await
    .with_context(|| format!("WebSocket handshake from {peer:?}"))?;
    let client_version = query.client_version.clone().unwrap_or_default();
    tracing::debug!(?peer, %client_version, "WebSocket established");

    let (mut writer, mut reader) = ws.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Notification>(OUTBOUND_CHANNEL_CAPACITY);

    let client_id: ClientId = Uuid::new_v4();
    {
        let mut s = state.lock().await;
        s.clients.insert(
            client_id,
            ClientSession {
                client_id,
                outbound: outbound_tx.clone(),
                active_project: None,
            },
        );
    }
    tracing::info!(%client_id, %client_version, "client registered");

    // `outbound_tx` stays in scope so the channel has a live sender for the lifetime of the
    // connection task — handlers push through the cloned sender we stashed on the session.
    let _outbound_tx = outbound_tx;
    let mut ctx = ConnectionCtx { client_id };

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

    {
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
        s.drop_nav_history_for_client(client_id);
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
        ProjectList::NAME => run!(ProjectList, handlers::project_list),
        ProjectActivate::NAME => run!(ProjectActivate, handlers::project_activate),
        ProjectCreate::NAME => run!(ProjectCreate, handlers::project_create),
        ProjectAddRoot::NAME => run!(ProjectAddRoot, handlers::project_add_root),
        ProjectRemoveRoot::NAME => run!(ProjectRemoveRoot, handlers::project_remove_root),
        ProjectRename::NAME => run!(ProjectRename, handlers::project_rename),
        ProjectDelete::NAME => run!(ProjectDelete, handlers::project_delete),
        PathDelete::NAME => run!(PathDelete, handlers::path_delete),
        BufferOpen::NAME => run!(BufferOpen, handlers::buffer_open),
        BufferSave::NAME => run!(BufferSave, handlers::buffer_save),
        BufferReload::NAME => run!(BufferReload, handlers::buffer_reload),
        BufferClose::NAME => run!(BufferClose, handlers::buffer_close),
        NavRecord::NAME => run!(NavRecord, handlers::nav_record),
        NavBack::NAME => run!(NavBack, handlers::nav_back),
        NavForward::NAME => run!(NavForward, handlers::nav_forward),
        NavGoto::NAME => run!(NavGoto, handlers::nav_goto),
        SearchSet::NAME => run!(SearchSet, handlers::search_set),
        SearchClear::NAME => run!(SearchClear, handlers::search_clear),
        SearchNext::NAME => run!(SearchNext, handlers::search_next),
        SearchPrev::NAME => run!(SearchPrev, handlers::search_prev),
        BufferCopy::NAME => run!(BufferCopy, handlers::buffer_copy),
        BufferCut::NAME => run!(BufferCut, handlers::buffer_cut),
        ViewportSubscribe::NAME => run!(ViewportSubscribe, handlers::viewport_subscribe),
        ViewportResize::NAME => run!(ViewportResize, handlers::viewport_resize),
        ViewportScroll::NAME => run!(ViewportScroll, handlers::viewport_scroll),
        ViewportScrollToRow::NAME => run!(ViewportScrollToRow, handlers::viewport_scroll_to_row),
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
        InputDeleteLine::NAME => run!(InputDeleteLine, handlers::input_delete_line),
        InputChangeLine::NAME => run!(InputChangeLine, handlers::input_change_line),
        InputReplaceLine::NAME => run!(InputReplaceLine, handlers::input_replace_line),
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
        InputSurround::NAME => run!(InputSurround, handlers::input_surround),
        InputUnsurround::NAME => run!(InputUnsurround, handlers::input_unsurround),
        PickerView::NAME => run!(PickerView, handlers::picker_view),
        PickerQuery::NAME => run!(PickerQuery, handlers::picker_query),
        PickerSelect::NAME => run!(PickerSelect, handlers::picker_select),
        PickerHide::NAME => run!(PickerHide, handlers::picker_hide),
        PickerGrepNavigate::NAME => run!(PickerGrepNavigate, handlers::picker_grep_navigate),
        PickerGrepFileJump::NAME => run!(PickerGrepFileJump, handlers::picker_grep_file_jump),
        DirectoryList::NAME => run!(DirectoryList, handlers::directory_list),
        DirectoryCreate::NAME => run!(DirectoryCreate, handlers::directory_create),
        GitBlameLine::NAME => run!(GitBlameLine, handlers::git_blame_line),
        GitCommitInfo::NAME => run!(GitCommitInfo, handlers::git_commit_info),
        GitSetDiffView::NAME => run!(GitSetDiffView, handlers::git_set_diff_view),
        GitSetDiffBase::NAME => run!(GitSetDiffBase, handlers::git_set_diff_base),
        GitNavigateHunk::NAME => run!(GitNavigateHunk, handlers::git_navigate_hunk),
        LspServerStatusList::NAME => run!(LspServerStatusList, handlers::lsp_server_status),
        LspRestartServer::NAME => run!(LspRestartServer, handlers::lsp_restart_server),
        LspHover::NAME => run!(LspHover, handlers::lsp_hover),
        LspGotoDefinition::NAME => run!(LspGotoDefinition, handlers::lsp_goto_definition),
        LspNavigateDiagnostic::NAME => run!(LspNavigateDiagnostic, handlers::lsp_navigate_diagnostic),
        LspFormat::NAME => run!(LspFormat, handlers::lsp_format),
        other => Err(RpcError::method_not_found(other)),
    }
}
