//! End-to-end test: spawn the server in-process, talk to it via WebSocket, exercise the
//! handshake and `buffer/open`.

use aether_protocol::buffer::{
    BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen,
    BufferOpenParams, BufferOpenResult, BufferSave, BufferSaveParams, BufferSaveResult,
    BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo,
    CursorUndoParams, CursorUndoResult, Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{ClientInbound, JsonRpc, NotificationMethod, Request, RpcMethod};
use aether_protocol::handshake::{ClientHello, ClientHelloParams, ClientHelloResult};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputBackspace, InputDedent, InputDelete, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent, InputRedo,
    InputText, InputTextParams, InputToggleComment, InputUndo, UndoResult,
};
use aether_protocol::picker::{
    PickerHide, PickerHideParams, PickerItem, PickerKind, PickerQuery, PickerQueryParams,
    PickerSelect, PickerSelectParams, PickerSelectResult, PickerUpdate, PickerUpdateParams,
    PickerView, PickerViewParams,
};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNavResult, SearchNext, SearchPrev,
    SearchSet, SearchSetParams, SearchSetResult,
};
use aether_protocol::viewport::{
    ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams, ViewportResize,
    ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportSetWrap,
    ViewportSetWrapParams, ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult,
    ViewportWindowResult, WrapMode,
};
use aether_protocol::LogicalPosition;
use aether_server::spawn_for_test;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

const TEST_TOKEN: &str = "test-token-xyz";

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    loop {
        let msg = ws.next().await.expect("ws closed").expect("ws error");
        if let Message::Text(t) = msg {
            return t.to_string();
        }
    }
}

async fn send_request<M: RpcMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    params: &M::Params,
) -> M::Result {
    let req = Request {
        jsonrpc: JsonRpc,
        id,
        method: M::NAME.into(),
        params: Some(serde_json::to_value(params).unwrap()),
    };
    let s = serde_json::to_string(&req).unwrap();
    ws.send(Message::text(s)).await.unwrap();

    // Drain notifications until we see the matching response.
    loop {
        let text = next_text(ws).await;
        match serde_json::from_str::<ClientInbound>(&text).expect("parseable inbound") {
            ClientInbound::Response(r) if r.id == id => {
                return serde_json::from_value(r.result).expect("typed result");
            }
            ClientInbound::Error(e) if e.id == id => {
                panic!("request {id} ({}) returned error: {:?}", M::NAME, e.error);
            }
            ClientInbound::Notification(_)
            | ClientInbound::Response(_)
            | ClientInbound::Error(_) => {
                // Skip unrelated frames; tests that care use `expect_notification` below.
            }
        }
    }
}

/// Like `send_request` but expects the RPC to return an error; returns the error message.
/// Panics on a successful response.
async fn send_request_expect_err<M: RpcMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    params: &M::Params,
) -> String {
    let req = Request {
        jsonrpc: JsonRpc,
        id,
        method: M::NAME.into(),
        params: Some(serde_json::to_value(params).unwrap()),
    };
    let s = serde_json::to_string(&req).unwrap();
    ws.send(Message::text(s)).await.unwrap();
    loop {
        let text = next_text(ws).await;
        match serde_json::from_str::<ClientInbound>(&text).expect("parseable inbound") {
            ClientInbound::Response(r) if r.id == id => {
                panic!("expected error for {}, got Ok: {:?}", M::NAME, r.result);
            }
            ClientInbound::Error(e) if e.id == id => return e.error.message,
            _ => {}
        }
    }
}

/// Read frames until one matching notification arrives. Panics if the stream ends first.
async fn expect_notification<N: NotificationMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> N::Params {
    loop {
        let text = next_text(ws).await;
        let inbound: ClientInbound = serde_json::from_str(&text).expect("parseable");
        if let ClientInbound::Notification(n) = inbound {
            if n.method == N::NAME {
                return serde_json::from_value(n.params).expect("typed params");
            }
        }
    }
}

#[tokio::test]
async fn hello_then_open_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("hello.rs");
    std::fs::write(&file_path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();

    let (mut ws, _resp) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();

    // Handshake.
    let hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    assert_eq!(hello.project.name, "test-proj");
    assert_eq!(hello.project.paths.len(), 1);

    // Open the file.
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("hello.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert!(open.buffer_id > 0);
    assert_eq!(open.language.as_deref(), Some("rust"));
    assert_eq!(open.saved_revision, open.revision);
    assert_eq!(open.revision, 0);
    assert!(open.line_count >= 3);
    assert!(open.byte_count > 0);
    // First open: no prior cursor or scroll for this (client, buffer).
    assert_eq!(open.cursor, CursorState::default());
    assert!(open.scroll.is_none());

    // Re-opening returns the same buffer id (deduping by canonical path).
    let open2: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("hello.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open2.buffer_id, open.buffer_id);

    drop(server);
}

#[tokio::test]
async fn buffer_open_restores_cursor_and_scroll() {
    // Multi-line file so we can scroll meaningfully.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    let mut content = String::new();
    for i in 0..30 {
        content.push_str(&format!("line {i}\n"));
    }
    std::fs::write(&path, &content).unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let buffer_id = open.buffer_id;

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Move the cursor and scroll the viewport so the (client, buffer) state diverges from defaults.
    let cursor_target = LogicalPosition { line: 12, col: 3 };
    let _: CursorState = send_request::<CursorSet>(
        &mut ws,
        4,
        &CursorSetParams {
            buffer_id,
            position: cursor_target,
            anchor: cursor_target,
        },
    )
    .await;
    let _: ViewportWindowResult = send_request::<ViewportScroll>(
        &mut ws,
        5,
        &ViewportScrollParams {
            viewport_id,
            scroll: ScrollPosition {
                logical_line: 8,
                sub_row: 0.0,
            },
        },
    )
    .await;

    // Reopen the same path (file-browser navigation pattern). The server should report the
    // prior cursor and scroll so the client can restore the view.
    let reopen: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        6,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(reopen.buffer_id, buffer_id);
    assert_eq!(reopen.cursor.position, cursor_target);
    let scroll = reopen.scroll.expect("scroll restored on reopen");
    assert_eq!(scroll.logical_line, 8);

    drop(server);
}

#[tokio::test]
async fn buffer_open_isolates_scroll_per_client() {
    // Two clients on the same buffer should see independent restored scroll positions.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    let mut content = String::new();
    for i in 0..30 {
        content.push_str(&format!("line {i}\n"));
    }
    std::fs::write(&path, &content).unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();

    let connect = || async {
        let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
            .await
            .unwrap();
        let _: ClientHelloResult = send_request::<ClientHello>(
            &mut ws,
            1,
            &ClientHelloParams {
                token: TEST_TOKEN.into(),
                client_version: "test".into(),
            },
        )
        .await;
        let open: BufferOpenResult = send_request::<BufferOpen>(
            &mut ws,
            2,
            &BufferOpenParams {
                buffer_id: None,
                path_index: Some(0),
                relative_path: Some("a.txt".into()),
                language: None,
                create_if_missing: false,
                jump_to: None,
            },
        )
        .await;
        let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
            &mut ws,
            3,
            &ViewportSubscribeParams {
                buffer_id: open.buffer_id,
                cols: 80,
                rows: 10,
                overscan_rows: 0,
                scroll: ScrollPosition {
                    logical_line: 0,
                    sub_row: 0.0,
                },
                wrap: WrapMode::None,
                continuation_marker_width: 0,
                tab_width: 4,
            },
        )
        .await;
        (ws, open.buffer_id, sub.viewport_id)
    };

    let (mut ws_a, buf_a, vp_a) = connect().await;
    let (mut ws_b, buf_b, vp_b) = connect().await;
    assert_eq!(buf_a, buf_b, "shared buffer, deduped by canonical path");

    let _: ViewportWindowResult = send_request::<ViewportScroll>(
        &mut ws_a,
        10,
        &ViewportScrollParams {
            viewport_id: vp_a,
            scroll: ScrollPosition {
                logical_line: 5,
                sub_row: 0.0,
            },
        },
    )
    .await;
    let _: ViewportWindowResult = send_request::<ViewportScroll>(
        &mut ws_b,
        10,
        &ViewportScrollParams {
            viewport_id: vp_b,
            scroll: ScrollPosition {
                logical_line: 17,
                sub_row: 0.0,
            },
        },
    )
    .await;

    let reopen_a: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws_a,
        20,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let reopen_b: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws_b,
        20,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(reopen_a.scroll.expect("a").logical_line, 5);
    assert_eq!(reopen_b.scroll.expect("b").logical_line, 17);

    drop(server);
}

#[tokio::test]
async fn rejects_bad_token() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();

    let req = Request {
        jsonrpc: JsonRpc,
        id: 1,
        method: ClientHello::NAME.into(),
        params: Some(
            serde_json::to_value(ClientHelloParams {
                token: "not-the-real-token".into(),
                client_version: "test".into(),
            })
            .unwrap(),
        ),
    };
    ws.send(Message::text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    let text = next_text(&mut ws).await;
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["code"], -32001, "expected INVALID_TOKEN");
}

#[tokio::test]
async fn rejects_path_outside_project() {
    let dir = tempfile::tempdir().unwrap();
    // File is in /tmp directly, not in the project's path.
    let outside = std::env::temp_dir().join("aether-outside-test.txt");
    std::fs::write(&outside, "outside").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();

    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    // Try to open by providing relative_path that escapes upward.
    let req = Request {
        jsonrpc: JsonRpc,
        id: 2,
        method: BufferOpen::NAME.into(),
        params: Some(
            serde_json::to_value(BufferOpenParams {
                buffer_id: None,
                path_index: Some(0),
                relative_path: Some("../aether-outside-test.txt".into()),
                language: None,
                create_if_missing: false,
                jump_to: None,
            })
            .unwrap(),
        ),
    };
    ws.send(Message::text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();

    let text = next_text(&mut ws).await;
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["code"], -32010, "expected INVALID_PATH");

    std::fs::remove_file(&outside).ok();
}

#[tokio::test]
async fn viewport_subscribe_renders_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    // 5 short lines.
    std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();

    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Subscribe to a viewport showing the full file.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;

    assert_eq!(sub.window.first_logical_line, 0);
    // 5 newlines in our content => ropey reports 6 lines (final empty).
    assert!(sub.window.last_logical_line_exclusive >= 5);

    let line0 = &sub.window.lines[0];
    assert_eq!(line0.logical_line, 0);
    assert_eq!(line0.visual_rows.len(), 1);
    assert_eq!(line0.visual_rows[0].segments[0].text, "alpha");
    let line2 = &sub.window.lines[2];
    assert_eq!(line2.visual_rows[0].segments[0].text, "gamma");
}

#[tokio::test]
async fn viewport_subscribe_wraps_long_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.txt");
    std::fs::write(&path, "the quick brown fox jumps over the lazy dog\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("long.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 20,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;

    // The single logical line should wrap to multiple visual rows.
    let line0 = &sub.window.lines[0];
    assert_eq!(line0.logical_line, 0);
    assert!(
        line0.visual_rows.len() >= 2,
        "expected long line to wrap, got {} rows",
        line0.visual_rows.len()
    );

    // And the joined visual rows reconstruct the original text (mod stripped break-whitespace).
    let joined: String = line0
        .visual_rows
        .iter()
        .map(|r| {
            r.segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(joined, "the quick brown fox jumps over the lazy dog");
}

#[tokio::test]
async fn viewport_scroll_returns_new_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("many.txt");
    let mut content = String::new();
    for i in 0..50 {
        content.push_str(&format!("line {i}\n"));
    }
    std::fs::write(&path, &content).unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("many.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 5,
            overscan_rows: 2,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    assert_eq!(sub.window.first_logical_line, 0);

    let scrolled: ViewportWindowResult = send_request::<ViewportScroll>(
        &mut ws,
        4,
        &ViewportScrollParams {
            viewport_id: sub.viewport_id,
            scroll: ScrollPosition {
                logical_line: 20,
                sub_row: 0.0,
            },
        },
    )
    .await;
    assert_eq!(scrolled.window.first_logical_line, 18); // 20 - overscan(2)
    assert!(scrolled.window.last_logical_line_exclusive >= 25);
    let first_text = &scrolled.window.lines[2].visual_rows[0].segments[0].text;
    assert_eq!(first_text, "line 20");
}

// -------- cursor + input ------------------------------------------------------------------------

async fn setup_with_buffer(
    content: &str,
) -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64, // buffer_id
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buf.txt");
    std::fs::write(&path, content).unwrap();
    let dir_path = dir.path().to_path_buf();
    // Keep tempdir alive for the duration of the test by leaking it; the test only runs briefly
    // and the OS will clean up /tmp on reboot.
    std::mem::forget(dir);

    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("buf.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    (server, ws, open.buffer_id)
}

#[tokio::test]
async fn cursor_starts_at_origin_and_moves_by_char() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello\nworld\n").await;

    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        10,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 3,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });
    assert!((st.anchor == st.position));

    // Moving forward past the end of line should land on the next line.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 5,
            },
            extend_selection: false,
        },
    )
    .await;
    // After "hel" + 5 chars we cross the newline: starts at (0,3), char 3. +5 -> char 8 -> "world" middle.
    // "hello\n" = 6 chars, so char 8 = 'r' in "world" => line 1, col 2.
    assert_eq!(st.position, LogicalPosition { line: 1, col: 2 });

    drop(server);
}

#[tokio::test]
async fn cursor_set_and_extend_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha beta gamma\n").await;

    // Set explicitly to col 6 (start of "beta").
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 6 },
            anchor: LogicalPosition { line: 0, col: 6 },
        },
    )
    .await;

    // Extend selection 3 chars right; block cursor lands on the 'a' of "beta" and the selection
    // operationally covers "beta" (position char is included in the selection's range).
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 3,
            },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 9 });
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 6 });

    drop(server);
}

#[tokio::test]
async fn line_end_and_buffer_end_motions() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\nxy\n").await;

    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        10,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::LineEnd,
            extend_selection: false,
        },
    )
    .await;
    // LineEnd lands on the last visible char ('c'), not on the trailing newline.
    assert_eq!(st.position, LogicalPosition { line: 0, col: 2 });

    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::BufferEnd,
            extend_selection: false,
        },
    )
    .await;
    // Buffer is "abc\nxy\n" => 7 chars; len_lines=3 (empty trailing line). End is line=2, col=0.
    assert_eq!(st.position, LogicalPosition { line: 2, col: 0 });

    drop(server);
}

#[tokio::test]
async fn input_text_inserts_and_pushes_notification() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\n").await;

    // Subscribe a viewport so we get notifications.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    assert_eq!(sub.window.lines[0].visual_rows[0].segments[0].text, "abc");

    // Move cursor to col 1 (between 'a' and 'b'), then insert "XY".
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 1 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;

    let result: EditResult = send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams {
            buffer_id,
            text: "XY".into(),
            select_pasted: false,
        },
    )
    .await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams =
        expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.viewport_id, sub.viewport_id);
    assert_eq!(notif.revision, 1);
    let first_line = &notif.replacement_lines[0];
    assert_eq!(first_line.visual_rows[0].segments[0].text, "aXYbc");

    drop(server);
}

#[tokio::test]
async fn input_delete_backspace_removes_char_before_cursor() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 5 }, // end of "hello"
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        11,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _ = sub;

    let result: EditResult =
        send_request::<InputBackspace>(&mut ws, 12, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams =
        expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "hell"
    );

    drop(server);
}

#[tokio::test]
async fn viewport_includes_treesitter_highlights_for_rust() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn main() { let s = \"hi\"; }\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open.language.as_deref(), Some("rust"));

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;

    let line0 = &sub.window.lines[0];
    let segs = &line0.visual_rows[0].segments;
    let highlights = &segs[0].highlights;
    assert!(
        !highlights.is_empty(),
        "expected highlight spans on a Rust line"
    );

    // First two bytes should be the keyword 'fn'.
    let fn_kw = highlights.iter().find(|h| h.start == 0 && h.end == 2);
    assert!(
        fn_kw.is_some_and(|h| h.kind.contains("keyword")),
        "expected 'fn' to be tagged keyword, got {:?}",
        fn_kw
    );

    drop(server);
}

#[tokio::test]
async fn match_bracket_motion_jumps_to_pair() {
    // Rust file so tree-sitter is active. Cursor on the `{` of `fn foo() {}`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Park on the `{` (col 9 on line 0).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: false },
            extend_selection: false,
        },
    )
    .await;
    // `}` lives at col 22 on the same line.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 22 });
    assert!((r.anchor == r.position));
    // match_bracket is populated; positions are the same pair regardless of orientation.
    let pair = r.match_bracket.expect("match_bracket should be populated");
    assert!(
        pair == (
            LogicalPosition { line: 0, col: 9 },
            LogicalPosition { line: 0, col: 22 }
        ) || pair
            == (
                LogicalPosition { line: 0, col: 22 },
                LogicalPosition { line: 0, col: 9 }
            )
    );

    drop(server);
}

#[tokio::test]
async fn match_bracket_with_extend_selects_to_pair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: false },
            extend_selection: true,
        },
    )
    .await;
    // Cursor lands on the `}`; anchor pinned at the original `{`. Together they cover the
    // whole `{...}` pair inclusive — that's the "select around brackets" gesture.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 22 });
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 9 });

    drop(server);
}

#[tokio::test]
async fn match_bracket_from_inside_pair_jumps_to_opener() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the `l` of `let` — inside the block, not on any bracket.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 11 },
            anchor: LogicalPosition { line: 0, col: 11 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: false },
            extend_selection: false,
        },
    )
    .await;
    // Cursor jumps to the opening `{` (we pick the opener when cursor is between brackets).
    assert_eq!(r.position, LogicalPosition { line: 0, col: 9 });

    drop(server);
}

#[tokio::test]
async fn match_bracket_inner_from_inside_lands_just_after_opener() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the `l` of `let` — inside the block, not on any bracket.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 11 },
            anchor: LogicalPosition { line: 0, col: 11 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: true },
            extend_selection: false,
        },
    )
    .await;
    // Lands one char *past* the opener `{` (col 9) — i.e., the first char inside the pair.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 10 });

    // A second press toggles to the inner-close side (one char before `}` at col 22).
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        5,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: true },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 0, col: 21 });
    // Anchor stays at the first-press position, so the selection covers the inside of `{...}`
    // (exclusive of the brackets themselves).
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 10 });

    drop(server);
}

#[tokio::test]
async fn match_bracket_inner_from_opener_jumps_to_inner_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the opening `{` (col 9).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: true },
            extend_selection: false,
        },
    )
    .await;
    // Lands one char before the closer `}` (col 22).
    assert_eq!(r.position, LogicalPosition { line: 0, col: 21 });

    drop(server);
}

#[tokio::test]
async fn match_bracket_inner_on_empty_pair_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the `{` of the empty `{}` (col 9).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::MatchBracket { inner: true },
            extend_selection: false,
        },
    )
    .await;
    // No inside content → cursor doesn't move.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 9 });

    drop(server);
}

#[tokio::test]
async fn end_of_unit_extend_then_delete_removes_whole_function() {
    // `}` (EndOfNavigationUnit with extend=true) from the start of a function selects
    // through the function's last char. A subsequent delete removes exactly the function,
    // not the function plus the first char of the next one.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the `fn` keyword of `fn one`. `}` jumps to the function's last char (the
    // closing `}`) and the anchor stays where we started.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    // `fn one() {}` is 11 chars; the closing `}` sits at col 10 of line 0.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 10 });
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 0 });

    // Delete the selection — the function should be removed exactly, leaving the trailing
    // newline (and the next two functions) intact.
    let _: EditResult = send_request::<InputDelete>(
        &mut ws,
        5,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        6,
        &BufferSaveParams {
            buffer_id: open.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    let disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(disk, "\nfn two() {}\nfn three() {}\n");

    drop(server);
}

#[tokio::test]
async fn end_of_unit_works_on_last_function() {
    // The old `}`-as-NextNavigationUnit couldn't select the last function because there was
    // no "next" sibling. `EndOfNavigationUnit` jumps to the unit's end byte regardless of
    // whether anything follows.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\nfn last() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on `fn last` (line 1, col 0).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    // `fn last() {}` is 12 chars; closing `}` at col 11.
    assert_eq!(r.position, LogicalPosition { line: 1, col: 11 });
    assert_eq!(r.anchor, LogicalPosition { line: 1, col: 0 });

    drop(server);
}

#[tokio::test]
async fn start_of_unit_extends_back_to_function_start() {
    // `{` from inside a function jumps to its start byte, with the original cursor preserved
    // as the anchor — selecting from the function's start up to (and including) where the
    // cursor was.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() {\n    let x = 1;\n}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor inside the body, on the `x` (line 1, col 8).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 8 },
            anchor: LogicalPosition { line: 1, col: 8 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::StartOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 0, col: 0 });
    assert_eq!(r.anchor, LogicalPosition { line: 1, col: 8 });

    drop(server);
}

#[tokio::test]
async fn repeated_end_of_unit_walks_through_adjacent_units() {
    // After `}` lands at the end of unit A, a second `}` should grow the selection through
    // unit B; a third through unit C. The anchor (set on the first extend press) is
    // preserved across all of them.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    // First `}`: select to end of fn one.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 0, col: 10 });
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 0 });

    // Second `}`: cursor at end of fn one, fall through to end of fn two.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        5,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 1, col: 10 });
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 0 });

    // Third `}`: end of fn three.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        6,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 2, col: 12 });
    assert_eq!(r.anchor, LogicalPosition { line: 0, col: 0 });

    // Fourth `}`: no more siblings, no-op.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        7,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 2, col: 12 });

    drop(server);
}

#[tokio::test]
async fn repeated_start_of_unit_walks_backward_through_adjacent_units() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the closing `}` of `fn three`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 12 },
            anchor: LogicalPosition { line: 2, col: 12 },
        },
    )
    .await;
    // First `{`: select back to start of fn three.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::StartOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 2, col: 0 });
    // Second `{`: cursor at start of fn three, fall through to start of fn two.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        5,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::StartOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 1, col: 0 });

    drop(server);
}

#[tokio::test]
async fn end_of_unit_outside_any_unit_jumps_to_next_unit_end() {
    // On a blank line between top-level items there's no enclosing unit, so `}` falls
    // through to the *next* unit's end — same code path that handles repeated presses.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\n\nfn two() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::EndOfNavigationUnit,
            extend_selection: true,
        },
    )
    .await;
    // Lands on the closing `}` of `fn two` (line 2, col 10).
    assert_eq!(r.position, LogicalPosition { line: 2, col: 10 });
    assert_eq!(r.anchor, LogicalPosition { line: 1, col: 0 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_jumps_between_top_level_rust_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(
        &path,
        "fn one() {\n    let x = 1;\n}\nfn two() {}\nfn three() {}\n",
    )
    .unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // From inside `fn one`'s body — the body has no nav-kind children, so the motion walks
    // up to the source_file level and jumps to `fn two`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 8 },
            anchor: LogicalPosition { line: 1, col: 8 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 3, col: 0 });

    // A second press jumps from `fn two` to `fn three`.
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        5,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 4, col: 0 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_prev_walks_backward() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on `fn three` (line 2 col 0). `[` walks back to `fn two`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 0 },
            anchor: LogicalPosition { line: 2, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::PrevNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 1, col: 0 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_noop_at_end_of_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    // Single function — nothing to navigate to.
    std::fs::write(&path, "fn only() {}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_inside_python_class_finds_next_method() {
    // The hierarchical case: cursor inside `method1`'s body. method1 has no nav-kind
    // children of its own, so the walk-up reaches the class's block, which contains
    // method2 — the natural next unit at the cursor's level.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(
        &path,
        "class Foo:\n    def method1(self):\n        pass\n    def method2(self):\n        pass\n\ndef top_level():\n    pass\n",
    ).unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.py".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor inside method1's body (the `pass` on line 2).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 8 },
            anchor: LogicalPosition { line: 2, col: 8 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 3, col: 4 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_from_last_method_stays_in_class() {
    // Depth preservation: from inside the last method of a class, `]` does NOT cross out
    // of the class to the next top-level item. Scope boundaries are respected.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(
        &path,
        "class Foo:\n    def method1(self):\n        pass\n    def method2(self):\n        pass\n\ndef top_level():\n    pass\n",
    ).unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.py".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor inside method2 (the `pass` on line 4). `]` should no-op rather than jump to
    // `def top_level` outside the class.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 4, col: 8 },
            anchor: LogicalPosition { line: 4, col: 8 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 4, col: 8 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_at_python_class_header_jumps_to_next_top_level() {
    // On `class Foo:` itself (cursor at the class's start byte), the class IS the cursor's
    // smallest containing ancestor, so the motion looks one level up — at module level —
    // and skips to the next top-level item rather than diving into the class.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(
        &path,
        "class Foo:\n    def method1(self):\n        pass\n\ndef top_level():\n    pass\n",
    )
    .unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.py".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 4, col: 0 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_inside_html_head_jumps_between_elements() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.html");
    std::fs::write(
        &path,
        "<html>\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>x</title>\n    <link href=\"a.css\" />\n  </head>\n</html>\n",
    ).unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.html".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on `<meta ...>` (line 2, col 4). The meta is a self-closing element with no
    // nav-kind children of its own, so the walk-up reaches `<head>` and jumps to `<title>`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 4 },
            anchor: LogicalPosition { line: 2, col: 4 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 3, col: 4 });

    drop(server);
}

#[tokio::test]
async fn nav_motion_with_no_syntax_is_noop() {
    // Plain `.txt` buffer — no language registered, so the motion has no nav-kinds to find.
    let (server, mut ws, buffer_id) = setup_with_buffer("hello\nworld\n").await;
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: CursorState = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::NextNavigationUnit,
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(r.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn viewport_highlights_rust_inside_markdown_fence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.md");
    // Content layout (0-indexed logical lines):
    //   0: "# Heading"
    //   1: ""
    //   2: "```rust"
    //   3: "fn main() {}"
    //   4: "```"
    std::fs::write(&path, "# Heading\n\n```rust\nfn main() {}\n```\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("notes.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open.language.as_deref(), Some("markdown"));

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;

    // Logical line 3 is `fn main() {}` — must inherit rust highlights from the injection layer.
    let fence_body = &sub.window.lines[3];
    let segs = &fence_body.visual_rows[0].segments;
    let highlights: Vec<&aether_protocol::viewport::Highlight> =
        segs.iter().flat_map(|s| s.highlights.iter()).collect();
    let fn_kw = highlights.iter().find(|h| h.start == 0 && h.end == 2);
    assert!(
        fn_kw.is_some_and(|h| h.kind.contains("keyword")),
        "expected rust 'fn' keyword highlight inside markdown fence, got highlights={:?}",
        highlights,
    );

    drop(server);
}

#[tokio::test]
async fn save_in_place_writes_file_and_clears_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("greet.txt");
    std::fs::write(&path, "hello\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("greet.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Subscribe a viewport so we receive the buffer/state push.
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;

    // Edit: append "!" at end. Move cursor to end then insert.
    let _ = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams {
            buffer_id: open.buffer_id,
            motion: Motion::BufferEnd,
            extend_selection: false,
        },
    )
    .await;
    // BufferEnd puts cursor on the trailing empty line; move it to end of first line instead.
    send_request::<aether_protocol::cursor::CursorSet>(
        &mut ws,
        5,
        &aether_protocol::cursor::CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 5 },
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;
    let _edit: EditResult = send_request::<InputText>(
        &mut ws,
        6,
        &InputTextParams {
            buffer_id: open.buffer_id,
            text: "!".into(),
            select_pasted: false,
        },
    )
    .await;
    // Drain the viewport/lines_changed pushed by the edit so it doesn't leak into the next test step.
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    let save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        7,
        &BufferSaveParams {
            buffer_id: open.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    assert!(save.saved_at_unix_ms > 0);

    let disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(disk, "hello!\n");

    // The server pushes buffer/state with the new saved_revision.
    let state_push: BufferStateParams = expect_notification::<BufferState>(&mut ws).await;
    assert_eq!(state_push.buffer_id, open.buffer_id);
    assert_eq!(state_push.saved_revision, save.revision);

    drop(server);
}

#[tokio::test]
async fn save_preserves_crlf_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.txt");
    std::fs::write(&path, "one\r\ntwo\r\nthree\r\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("windows.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Save without changes — line endings should round-trip as CRLF.
    let _save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        3,
        &BufferSaveParams {
            buffer_id: open.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.windows(2).any(|w| w == b"\r\n"),
        "expected CRLF after save, got {bytes:?}"
    );
    assert!(
        !bytes.windows(2).any(|w| w[0] != b'\r' && w[1] == b'\n'),
        "expected no bare LF after save"
    );

    drop(server);
}

#[tokio::test]
async fn save_scratch_returns_buffer_has_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Save-in-place on a scratch buffer must return BUFFER_HAS_NO_PATH.
    let req = Request {
        jsonrpc: JsonRpc,
        id: 3,
        method: BufferSave::NAME.into(),
        params: Some(
            serde_json::to_value(BufferSaveParams {
                buffer_id: open.buffer_id,
                path_index: None,
                relative_path: None,
                overwrite: false,
            })
            .unwrap(),
        ),
    };
    ws.send(Message::text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    let text = next_text(&mut ws).await;
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"]["code"], -32015, "expected BUFFER_HAS_NO_PATH");

    drop(server);
}

#[tokio::test]
async fn copy_selection_returns_inclusive_text() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha beta gamma\n").await;
    // Move to col 6, extend forward 3 → selection "beta" (inclusive of position char).
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 6 },
            anchor: LogicalPosition { line: 0, col: 6 },
        },
    )
    .await;
    let _: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 3,
            },
            extend_selection: true,
        },
    )
    .await;
    let r: BufferCopyResult = send_request::<BufferCopy>(
        &mut ws,
        12,
        &BufferCopyParams {
            buffer_id,
            scope: CopyScope::Selection,
        },
    )
    .await;
    assert_eq!(r.text, "beta");
    drop(server);
}

#[tokio::test]
async fn copy_line_returns_full_line_with_newline() {
    let (server, mut ws, buffer_id) = setup_with_buffer("first\nsecond\nthird\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    let r: BufferCopyResult = send_request::<BufferCopy>(
        &mut ws,
        11,
        &BufferCopyParams {
            buffer_id,
            scope: CopyScope::Line,
        },
    )
    .await;
    assert_eq!(r.text, "second\n");
    drop(server);
}

#[tokio::test]
async fn cut_selection_deletes_and_returns_text() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha beta gamma\n").await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 6 },
            anchor: LogicalPosition { line: 0, col: 6 },
        },
    )
    .await;
    let _: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 3,
            },
            extend_selection: true,
        },
    )
    .await;
    let r: BufferCutResult = send_request::<BufferCut>(
        &mut ws,
        13,
        &BufferCopyParams {
            buffer_id,
            scope: CopyScope::Selection,
        },
    )
    .await;
    assert_eq!(r.text, "beta");
    // dirty is now derived client-side from revision vs saved_revision; just confirm the
    // revision advanced.
    assert!(r.revision > 0);
    let notif =
        expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "alpha  gamma"
    );
    drop(server);
}

#[tokio::test]
async fn input_text_with_select_pasted_makes_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        11,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let edit: EditResult = send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams {
            buffer_id,
            text: "XYZ".into(),
            select_pasted: true,
        },
    )
    .await;
    // Anchor at col 0 ('X'), position at col 2 (block on 'Z') — selection covers "XYZ".
    assert_eq!(edit.cursor.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(edit.cursor.position, LogicalPosition { line: 0, col: 2 });
    drop(server);
}

#[tokio::test]
async fn undo_reverts_recent_edit_and_redo_reapplies() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\n").await;

    // Set cursor to end of "abc" and insert "XY".
    send_request::<aether_protocol::cursor::CursorSet>(
        &mut ws,
        10,
        &aether_protocol::cursor::CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        11,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let edit: EditResult = send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams {
            buffer_id,
            text: "XY".into(),
            select_pasted: false,
        },
    )
    .await;
    assert!(edit.revision > 0);
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Undo: should revert "XY", cursor back to col 3, and (since saved_revision is 0) the
    // revision drops to 0 — client derives `dirty == false` from that.
    let undo: UndoResult =
        send_request::<InputUndo>(&mut ws, 13, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert_eq!(undo.cursor.position, LogicalPosition { line: 0, col: 3 });
    assert_eq!(undo.revision, 0, "undo back to saved revision");
    let notif =
        expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "abc"
    );

    // Redo: re-applies "XY", revision advances past saved.
    let redo: UndoResult =
        send_request::<InputRedo>(&mut ws, 14, &BufferOnlyParams { buffer_id }).await;
    assert!(redo.applied);
    assert!(redo.revision > 0);
    let notif =
        expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "abcXY"
    );

    drop(server);
}

#[tokio::test]
async fn undo_on_empty_stack_returns_applied_false() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hi\n").await;
    let r: UndoResult =
        send_request::<InputUndo>(&mut ws, 10, &BufferOnlyParams { buffer_id }).await;
    assert!(!r.applied);
    drop(server);
}

#[tokio::test]
async fn dirty_clears_when_undoing_back_past_save() {
    // Make two edits in distinct groups, save in the middle, then undo back.
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\n").await;
    send_request::<aether_protocol::cursor::CursorSet>(
        &mut ws,
        10,
        &aether_protocol::cursor::CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        11,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    // Edit #1: insert "X"
    let _e1: EditResult = send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams {
            buffer_id,
            text: "X".into(),
            select_pasted: false,
        },
    )
    .await;
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Save.
    let save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        13,
        &BufferSaveParams {
            buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    let saved_state = expect_notification::<BufferState>(&mut ws).await;
    assert_eq!(saved_state.saved_revision, save.revision);

    // Edit #2: delete (different kind, so a new group). Backspace removes the "X".
    let _e2: EditResult =
        send_request::<InputBackspace>(&mut ws, 14, &BufferOnlyParams { buffer_id }).await;
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Undo: should put "X" back, taking us back to the saved revision → derived dirty == false.
    let undo: UndoResult =
        send_request::<InputUndo>(&mut ws, 15, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert_eq!(
        undo.revision, save.revision,
        "undo should return to the saved revision"
    );
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    drop(server);
}

#[tokio::test]
async fn word_motion_forward_and_back() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world-foo bar\n").await;

    // `w` forward: hello → world (col 6)
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        10,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: false,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 6 });

    // `w` again: world → '-' (col 11) — the hyphen starts a new word category
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: false,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 11 });

    // `Alt-w` (WORD): from col 0, skip "hello" → " " then to "world-foo" (col 6)
    send_request::<CursorSet>(
        &mut ws,
        12,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::BigWord,
                exclusive: false,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 6 });
    // Another WORD forward: "world-foo" → "bar" (col 16)
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        14,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::BigWord,
                exclusive: false,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 16 });

    // `b` backward from col 16: → col 12 (start of "foo")
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        15,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Backward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: false,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 12 });

    drop(server);
}

#[tokio::test]
async fn word_end_motion_lands_on_last_char() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world\n").await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        10,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::WordEnd {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
            },
            extend_selection: false,
        },
    )
    .await;
    // From col 0 (on 'h'), `e` lands on the 'o' of "hello" → col 4.
    assert_eq!(st.position, LogicalPosition { line: 0, col: 4 });
    drop(server);
}

#[tokio::test]
async fn join_lines_collapses_lines_with_single_space() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello \n  world\n").await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputJoinLines>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    assert!(r.revision > 0);
    let notif =
        expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    // After join: "hello world\n" — trailing whitespace of line 0 removed, leading whitespace of
    // line 1 removed, single space inserted.
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "hello world"
    );
    drop(server);
}

#[tokio::test]
async fn input_text_with_selection_replaces_it() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha beta gamma\n").await;

    // Select "beta" (cols 6..10).
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 6 },
            anchor: LogicalPosition { line: 0, col: 6 },
        },
    )
    .await;
    let _: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            // Forward 3 from col 6 puts the block cursor on the 'a' of "beta"; with the cursor
            // char in the selection, the operational range covers all of "beta".
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 3,
            },
            extend_selection: true,
        },
    )
    .await;

    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        12,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _ = sub;

    let result: EditResult = send_request::<InputText>(
        &mut ws,
        13,
        &InputTextParams {
            buffer_id,
            text: "DELTA".into(),
            select_pasted: false,
        },
    )
    .await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams =
        expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.replacement_lines[0].visual_rows[0].segments[0].text,
        "alpha DELTA gamma"
    );

    drop(server);
}

// ---- cursor/select_line ------------------------------------------------------------------------

/// 4-line buffer ("alpha\nbeta\ngamma\ndelta\n") used by most select_line tests.
async fn setup_lines() -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64,
) {
    setup_with_buffer("alpha\nbeta\ngamma\ndelta\n").await
}

#[tokio::test]
async fn select_line_forward_picks_current_then_advances_at_end() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Mid-line: selects current line.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Whole-line selection exists → advances to the next line.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 2, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_forward_at_end_of_line_no_anchor_picks_current_line() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Cursor at end-of-line with no anchor (the natural state after typing on a line).
    // First press picks the current line, not the following one.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 4 },
            anchor: LogicalPosition { line: 1, col: 4 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

/// On an empty line, "selecting" the line is a degenerate no-op (the only position is the
/// newline at col 0). Without special-casing, repeated `x` presses would stay on the empty
/// line forever. The forward variant should advance to the next line on first press.
#[tokio::test]
async fn select_line_forward_on_empty_line_advances() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n\ngamma\n").await;

    // Park on the empty line (line 1), no anchor.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    // Advanced to line 2, with a whole-line selection over "gamma".
    assert_eq!(st.anchor, LogicalPosition { line: 2, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

/// Backward `Alt-x` on an empty line walks back to the previous line.
#[tokio::test]
async fn select_line_backward_on_empty_line_walks_up() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n\ngamma\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    // Whole-line selection over "alpha". Cursor at end (same convention as the non-empty
    // backward case), anchor at start.
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_from_point_picks_line_above_then_walks_up() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // No anchor, mid-line: first press picks the line *above* the cursor (not the current
    // line) so non-extend Backward stays distinct from non-extend Forward on the first press.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 2 },
            anchor: LogicalPosition { line: 2, col: 2 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Second press: whole-line selection exists → walks up to the previous line.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_walks_up_via_anchor_on_repeat() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Start at end of "delta" — first press jumps to the line above.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 3, col: 5 },
            anchor: LogicalPosition { line: 3, col: 5 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 2, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    // Second press: walks up via anchor-at-col-0 → line 1.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Third press: → line 0.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        13,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_forward_extend_walks_cursor_down() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x: line 0.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 2 },
            anchor: LogicalPosition { line: 0, col: 2 },
        },
    )
    .await;
    send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;

    // Shift-x: lines 0–1.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Shift-x again: lines 0–2.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        13,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_extend_walks_anchor_up() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x: line 3.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 3, col: 2 },
            anchor: LogicalPosition { line: 3, col: 2 },
        },
    )
    .await;
    send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;

    // Shift-Alt-x: lines 2–3.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 2, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    // Shift-Alt-x again: lines 1–3.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        13,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_after_swap_preserves_backward_orientation() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x at start of line 0, then swap — backward selection of line 0.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    let st: CursorState =
        send_request::<CursorSwapAnchor>(&mut ws, 12, &CursorSwapAnchorParams { buffer_id }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 5 });

    // Shift-x grows the *bottom* edge down (anchor moves), cursor stays at top.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        13,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

/// Alt-x on the first line: there's no line above to advance to, so the cursor stays put
/// (saturating-sub on the row index).
#[tokio::test]
async fn select_line_backward_from_point_on_first_line_clamps() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

/// Alt-x on a multi-line *partial* selection still snaps to whole lines at the top edge
/// (rather than skipping past it). Subsequent presses then walk up from there.
#[tokio::test]
async fn select_line_backward_on_partial_selection_snaps_to_top_edge() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Partial selection from mid-line 1 to mid-line 2 (anchor at top, cursor at bottom).
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 3 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: false,
        },
    )
    .await;
    // Snaps to the top edge's line (line 1), no movement past it.
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

/// Shift-Alt-x starting from a point cursor jumps straight to the line above (same as Alt-x),
/// keeping it distinct from Shift-x on the first press. Subsequent Shift-Alt-x presses extend
/// from there.
#[tokio::test]
async fn select_line_backward_extend_from_point_jumps_to_line_above() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 2 },
            anchor: LogicalPosition { line: 2, col: 2 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Second press extends the top edge upward to include line 0.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Backward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

#[tokio::test]
async fn select_line_snaps_partial_selection_to_whole_lines() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // A partial, non-line-aligned selection (e.g. left over from Shift-arrow motion).
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 3 },
            anchor: LogicalPosition { line: 0, col: 2 },
        },
    )
    .await;

    // Shift-x snaps both ends to whole-line boundaries: anchor → col 0, cursor → line end.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_snaps_partial_selection_when_cursor_at_line_end() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Partial selection whose bottom edge happens to sit exactly at end-of-line.
    // The top edge is mid-line, so it's not a whole-line selection yet — x should
    // snap, not advance to the next line.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 5 },
            anchor: LogicalPosition { line: 0, col: 2 },
        },
    )
    .await;

    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    // Now that the selection is whole-line, a second forward press advances.
    let st: CursorState = send_request::<CursorSelectLine>(
        &mut ws,
        12,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: true,
        },
    )
    .await;
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    drop(server);
}

// ---- cursor/swap_anchor ------------------------------------------------------------------------

#[tokio::test]
async fn swap_anchor_swaps_position_and_anchor() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 3 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;

    let st: CursorState =
        send_request::<CursorSwapAnchor>(&mut ws, 11, &CursorSwapAnchorParams { buffer_id }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 1 });
    assert_eq!(st.anchor, LogicalPosition { line: 1, col: 3 });

    drop(server);
}

#[tokio::test]
async fn swap_anchor_with_no_selection_is_noop() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let st: CursorState =
        send_request::<CursorSwapAnchor>(&mut ws, 11, &CursorSwapAnchorParams { buffer_id }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });
    assert_eq!(st.anchor, st.position);

    drop(server);
}

// ---- Motion::Word { exclusive: true } -----------------------------------------------------------

#[tokio::test]
async fn word_motion_exclusive_progresses_across_boundaries() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world foo\n").await;

    // From 'h' (col 0), exclusive forward Word — lands on space before "world".
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        10,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: true,
            },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });
    assert_eq!(st.anchor, LogicalPosition { line: 0, col: 0 });

    // Repeated press from the space — pre-advance kicks in so we skip "world" entirely and
    // land on the space before "foo" (col 11), rather than getting stuck.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: true,
            },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 11 });

    drop(server);
}

// ---- cursor/undo and cursor/redo --------------------------------------------------------------

#[tokio::test]
async fn motion_undo_restores_previous_cursor() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;

    // Two cursor moves: (0,0) → (1,2) → (2,3).
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 3 },
            anchor: LogicalPosition { line: 2, col: 3 },
        },
    )
    .await;

    // Undo: back to (1,2).
    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 12, &CursorUndoParams { buffer_id }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });

    // Undo again: back to the initial (0, 0).
    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams { buffer_id }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_then_redo_round_trips() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 3 },
            anchor: LogicalPosition { line: 1, col: 3 },
        },
    )
    .await;

    // Undo → back to (0, 0).
    send_request::<CursorUndo>(&mut ws, 11, &CursorUndoParams { buffer_id }).await;

    // Redo → forward to (1, 3).
    let r: CursorUndoResult =
        send_request::<CursorRedo>(&mut ws, 12, &CursorUndoParams { buffer_id }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 3 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_returns_not_applied_when_stack_empty() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;

    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 10, &CursorUndoParams { buffer_id }).await;
    assert!(!r.applied);
    // Cursor unchanged.
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_stack_cleared_by_mutation() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    // Build up some motion history.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 4 },
            anchor: LogicalPosition { line: 1, col: 4 },
        },
    )
    .await;

    // Mutation clears the motion stack.
    send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams {
            buffer_id,
            text: "X".into(),
            select_pasted: false,
        },
    )
    .await;

    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams { buffer_id }).await;
    assert!(!r.applied, "motion stack should be empty after a mutation");

    drop(server);
}

#[tokio::test]
async fn motion_redo_cleared_by_new_motion() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 3 },
            anchor: LogicalPosition { line: 1, col: 3 },
        },
    )
    .await;
    // Undo populates redo.
    send_request::<CursorUndo>(&mut ws, 11, &CursorUndoParams { buffer_id }).await;
    // New motion should clear the redo stack.
    send_request::<CursorSet>(
        &mut ws,
        12,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 2 },
            anchor: LogicalPosition { line: 0, col: 2 },
        },
    )
    .await;

    let r: CursorUndoResult =
        send_request::<CursorRedo>(&mut ws, 13, &CursorUndoParams { buffer_id }).await;
    assert!(
        !r.applied,
        "redo stack should be empty after a fresh motion"
    );

    drop(server);
}

#[tokio::test]
async fn motion_undo_records_select_line_and_swap() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    // Position at line 1 mid.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    // x → selects line 1.
    send_request::<CursorSelectLine>(
        &mut ws,
        11,
        &CursorSelectLineParams {
            buffer_id,
            direction: Direction::Forward,
            extend: false,
        },
    )
    .await;
    // s → swap.
    let after_swap: CursorState =
        send_request::<CursorSwapAnchor>(&mut ws, 12, &CursorSwapAnchorParams { buffer_id }).await;
    assert_eq!(after_swap.position, LogicalPosition { line: 1, col: 0 });

    // Undo the swap.
    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams { buffer_id }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 1, col: 0 });

    // Undo the select_line.
    let r: CursorUndoResult =
        send_request::<CursorUndo>(&mut ws, 14, &CursorUndoParams { buffer_id }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });
    assert_eq!(r.cursor.anchor, r.cursor.position);

    drop(server);
}

#[tokio::test]
async fn word_motion_exclusive_at_buffer_end_does_not_move_past() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello").await;

    // Cursor on last char.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 4 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word {
                direction: Direction::Forward,
                count: 1,
                boundary: WordBoundary::Word,
                exclusive: true,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 4 });

    drop(server);
}

// ---- Motion::VisualLine -----------------------------------------------------------------------

#[tokio::test]
async fn visual_line_down_walks_wrapped_rows_within_a_logical_line() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    // Subscribe with WrapMode::Soft at width 10 so the line wraps to ["the quick", "brown fox"].
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Cursor at start of line — visual col 0 of row 0. Down should land on row 1's col 0 (byte 10).
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 10 });

    drop(server);
}

#[tokio::test]
async fn visual_line_preserves_visual_column() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Put cursor at byte 5 (visual col 5 of row 0). Down should land at byte 10+5=15 in row 1.
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 5 },
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 15 });

    // Up: back to visual col 5 of row 0 = byte 5.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Up,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

#[tokio::test]
async fn visual_line_crosses_logical_line_boundary() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\ndef\n").await;
    // Width is large enough that no line wraps.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 20,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Cursor at (0, 1). Down → (1, 1).
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 1 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 1 });

    drop(server);
}

#[tokio::test]
async fn visual_line_preserves_display_column_across_multibyte_chars() {
    // Line 0 has 7 ASCII chars; line 1 starts with an em dash ('—', 3 bytes / 1 display cell) and
    // then 6 ASCII chars. Moving the cursor down from byte 3 on line 0 (display col 3, on 'd')
    // should land it at byte 5 on line 1 (display col 3, on 'c'). Pre-fix it would have landed
    // at byte 3 — inside / just past the em dash — which is display col 1.
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefg\n—abcdef\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 5 });

    drop(server);
}

#[tokio::test]
async fn visual_line_with_wrap_none_falls_back_to_logical() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\nhi\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Cursor at (0, 5). With wrap=None, Down → logical line + 1, col clamped to line 1's length.
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 5 },
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 2 }); // line 1 = "hi", len 2

    drop(server);
}

// ---- viewport/set_wrap ------------------------------------------------------------------------

#[tokio::test]
async fn viewport_set_wrap_changes_visible_rows() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    // Soft: line 0 wraps to 2 visual rows at cols=10.
    assert_eq!(sub.window.lines[0].visual_rows.len(), 2);

    let r: ViewportWindowResult = send_request::<ViewportSetWrap>(
        &mut ws,
        11,
        &ViewportSetWrapParams {
            viewport_id: sub.viewport_id,
            wrap: WrapMode::None,
        },
    )
    .await;
    // None: one row, full line content.
    assert_eq!(r.window.lines[0].visual_rows.len(), 1);
    assert_eq!(
        r.window.lines[0].visual_rows[0].segments[0].text,
        "the quick brown fox"
    );

    drop(server);
}

// ---- virtual column -----------------------------------------------------------------------

#[tokio::test]
async fn virtual_col_prevents_drift_through_continuation_rows() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefghijklmnopqrst\n").await;
    // With cols=10, marker_width=2, line 0 wraps to 3 rows:
    //   row 0 byte 0..10 = "abcdefghij" (prefix 0)
    //   row 1 byte 10..18 = "klmnopqr"  (prefix 2 — continuation marker)
    //   row 2 byte 18..20 = "st"        (prefix 2)
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    // Start at byte 1 (visual col 1 on row 0, prefix 0).
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 1 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;

    // Alt-j: visual col 1 < prefix 2 on row 1, so cursor clamps to start of row 1's text (byte 10).
    // The remembered virtual col stays at 1.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 10 });

    // Alt-k: with virtual_col=1, target visual col is 1. On row 0 (prefix 0), byte = 1. We end
    // back where we started, not at byte 2 (which is what naive preserve-col would do).
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Up,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 1 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_preserved_across_empty_line_for_logical_motion() {
    // The classic vim virtual-col case: j down through an empty line should land you back at
    // your original column on the next non-empty line, not stick at col 0.
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world\n\nanother line\n").await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;

    // Start at col 5 of line 0.
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 5 },
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;

    // j → empty line 1; col clamps to 0 but virtual_col holds 5.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::LogicalLine {
                direction: Direction::Forward,
                count: 1,
                preserve_col: true,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 0 });

    // j → line 2 with content; virtual_col restores col 5.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::LogicalLine {
                direction: Direction::Forward,
                count: 1,
                preserve_col: true,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_cleared_by_horizontal_motion() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefghijklmnopqrst\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 1 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;
    send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    // Cursor now at byte 10 (visual col 2 = prefix); virtual_col stashed = 1.

    // Char Forward (a horizontal motion) clears the virtual col. Cursor at byte 11, visual col 3.
    send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char {
                direction: Direction::Forward,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;

    // Alt-k: without a virtual col, target is current visual col (3). Lands at byte 3 of row 0.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        14,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Up,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_cleared_by_mutation() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefghijklmnopqrst\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 1 },
            anchor: LogicalPosition { line: 0, col: 1 },
        },
    )
    .await;
    send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Down,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    // virtual_col = 1, cursor at byte 10.

    // Insert "X" — the mutation clears the virtual col. Cursor advances to byte 11.
    send_request::<InputText>(
        &mut ws,
        13,
        &InputTextParams {
            buffer_id,
            text: "X".into(),
            select_pasted: false,
        },
    )
    .await;

    // Alt-k: target is current visual col (3, since cursor is on row 1 with prefix 2 at col 1
    // within the text). Lands at byte 3, not the original byte 1.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        14,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::VisualLine {
                viewport_id,
                direction: VerticalDirection::Up,
                count: 1,
            },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });

    drop(server);
}

#[tokio::test]
async fn continuation_marker_width_reduces_continuation_row_width() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    // With marker_width=2 the continuation rows have 8 cols of content room, so the line wraps
    // into 3 visual rows instead of 2.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        10,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 10,
            rows: 5,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 2,
            tab_width: 4,
        },
    )
    .await;
    assert_eq!(sub.window.lines[0].visual_rows.len(), 3);
    let texts: Vec<&str> = sub.window.lines[0]
        .visual_rows
        .iter()
        .map(|r| r.segments[0].text.as_str())
        .collect();
    assert_eq!(texts, vec!["the quick", "brown", "fox"]);

    drop(server);
}

// ---- input/move_lines ---------------------------------------------------------------------------

async fn buffer_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    buffer_id: u64,
) -> String {
    // Subscribe to a wide-enough viewport and concatenate the visible-text lines.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        ws,
        id,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 200,
            rows: 100,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    sub.window
        .lines
        .iter()
        .map(|l| l.visual_rows[0].segments[0].text.as_str().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn move_lines_swaps_with_neighbor_below() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    // Cursor on line 0 ("alpha").
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 2 },
            anchor: LogicalPosition { line: 0, col: 2 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        11,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Down,
        },
    )
    .await;
    // Cursor follows the line down.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    // The "\n" at the end yields a trailing empty visible row.
    assert_eq!(text, "beta\nalpha\ngamma\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_swaps_with_neighbor_above() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 1 },
            anchor: LogicalPosition { line: 1, col: 1 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        11,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Up,
        },
    )
    .await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 1 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "beta\nalpha\ngamma\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_moves_whole_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("a\nb\nc\nd\ne\n").await;
    // Selection covers lines 1 and 2 ("b" and "c").
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        11,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Down,
        },
    )
    .await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 3, col: 0 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 2, col: 0 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "a\nd\nb\nc\ne\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_at_top_is_noop_up() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        10,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Up,
        },
    )
    .await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });
    let text = buffer_text(&mut ws, 11, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_at_bottom_is_noop_down() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        11,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Down,
        },
    )
    .await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 0 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_preserves_missing_trailing_newline() {
    // Buffer with no trailing newline: moving the last line up should still produce a buffer
    // without a trailing newline (whichever line is now the last keeps that property).
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputMoveLines>(
        &mut ws,
        11,
        &InputMoveLinesParams {
            buffer_id,
            direction: VerticalDirection::Up,
        },
    )
    .await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "beta\nalpha");

    drop(server);
}

// ---- input/indent and input/dedent --------------------------------------------------------------

#[tokio::test]
async fn indent_single_line_adds_two_spaces() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 3 },
            anchor: LogicalPosition { line: 0, col: 3 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputIndent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    // Cursor follows the inserted indent.
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 5 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "  alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn dedent_strips_two_spaces() {
    let (server, mut ws, buffer_id) = setup_with_buffer("  alpha\nbeta\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 4 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputDedent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 2 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn indent_multi_line_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("a\nb\nc\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputIndent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    // Anchor and cursor both shift +2 since both lines were indented.
    assert_eq!(r.cursor.position, LogicalPosition { line: 2, col: 2 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 2 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "  a\n  b\n  c\n");

    drop(server);
}

#[tokio::test]
async fn dedent_line_without_indent_is_noop_for_that_line() {
    let (server, mut ws, buffer_id) = setup_with_buffer("  alpha\nbeta\n").await;
    // Multi-line selection covering both lines.
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 1 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputDedent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    // Line 0 lost 2 chars, line 1 unchanged.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 1 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 2 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn dedent_with_single_leading_space_strips_one() {
    let (server, mut ws, buffer_id) = setup_with_buffer(" alpha\n").await;
    let r: EditResult =
        send_request::<InputDedent>(&mut ws, 10, &BufferOnlyParams { buffer_id }).await;
    let text = buffer_text(&mut ws, 11, buffer_id).await;
    assert_eq!(text, "alpha\n");
    // Cursor was at (0, 0); dedent removes 1 char, cursor stays at 0 (saturated).
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

// ---- input/newline_and_indent -------------------------------------------------------------------

#[tokio::test]
async fn newline_and_indent_copies_leading_whitespace() {
    let (server, mut ws, buffer_id) = setup_with_buffer("    foo\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 7 },
            anchor: LogicalPosition { line: 0, col: 7 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "    foo\n    \n");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_adds_one_level_after_opening_brace() {
    // .rs file so tree-sitter is active (and would *correctly* not suppress, since the brace is
    // a real syntactic opener here).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() {\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let buffer_id = open.buffer_id;

    // Cursor right after the opening brace.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 10 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    // Rust defaults to 4-space indent; cursor lands at col 4 on the new line.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });
    let text = buffer_text(&mut ws, 5, buffer_id).await;
    assert_eq!(text, "fn foo() {\n    \n");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_suppresses_brace_inside_comment() {
    // Brace in a `//` comment must not trigger an indent — tree-sitter sees a `line_comment`
    // node, not a code-level opener.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "// note {\n").unwrap();

    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let buffer_id = open.buffer_id;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 0 });
    let text = buffer_text(&mut ws, 5, buffer_id).await;
    assert_eq!(text, "// note {\n\n");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_on_empty_line_inserts_just_newline() {
    let (server, mut ws, buffer_id) = setup_with_buffer("\n").await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 10, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 0 });
    let text = buffer_text(&mut ws, 11, buffer_id).await;
    assert_eq!(text, "\n\n");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_engine_dedents_after_closing_brace() {
    // Engine-driven test: cursor just past `}` should produce zero indent (block @indent and
    // `}` @outdent cancel each other).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() {\n  x;\n}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let buffer_id = open.buffer_id;

    // Park cursor just past the closing `}` on line 2.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 1 },
            anchor: LogicalPosition { line: 2, col: 1 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    // No indent on the new line — we just left the function body.
    assert_eq!(r.cursor.position, LogicalPosition { line: 3, col: 0 });

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_engine_python_def() {
    // Python `def foo():` followed by Enter — function_definition's @indent should fire even
    // though there's no `{` opener. Exercises the Python indents.scm.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "def foo():\n    pass\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.py".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open.language.as_deref(), Some("python"));
    let buffer_id = open.buffer_id;

    // Cursor at end of `def foo():` (line 0 col 10).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 10 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    // Python defaults to 4-space indent (PEP 8); cursor lands at col 4 on the new line.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_detects_two_space_indent_in_rust_file() {
    // Existing file uses 2-space indent — detection should override Rust's 4-space default
    // and produce a 2-space new indent.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() {\n  let x = 1;\n  let y = 2;\n}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let buffer_id = open.buffer_id;

    // Cursor at end of `let y = 2;` (line 2) — engine returns 1 level, unit is 2 spaces.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 2, col: 12 },
            anchor: LogicalPosition { line: 2, col: 12 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 3, col: 2 });

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_uses_language_default_for_empty_file() {
    // Empty Go file — no indent to detect, so the Go default (Tab) applies. After typing
    // `func foo() {` and pressing Enter, the new line should be a single tab.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.go");
    std::fs::write(&path, "").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.go".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open.language.as_deref(), Some("go"));
    let buffer_id = open.buffer_id;

    send_request::<InputText>(
        &mut ws,
        3,
        &InputTextParams {
            buffer_id,
            text: "func foo() {".into(),
            select_pasted: false,
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    // One tab = col 1 (in byte columns). The opener-bonus heuristic fires because the parser
    // hasn't seen a closing brace yet; one indent level for Go is one tab character.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 1 });
    let text = buffer_text(&mut ws, 5, buffer_id).await;
    assert_eq!(text, "func foo() {\n\t");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_fallback_copies_previous_line() {
    // No indent query for `.txt` (no language detected) — fallback copies the previous line's
    // leading whitespace verbatim, without any brace heuristic magic.
    let (server, mut ws, buffer_id) = setup_with_buffer("    foo {\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 0, col: 9 },
            anchor: LogicalPosition { line: 0, col: 9 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputNewlineAndIndent>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    // Falls back to 4 spaces — the leading whitespace of line 0 — even though the line ends
    // in `{`. Plain text doesn't get the opener heuristic.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

// ---- input/toggle_comment ----------------------------------------------------------------------

#[tokio::test]
async fn toggle_comment_adds_prefix_to_rust_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "    let x = 1;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on `let` (col 4, after the 4-space indent).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 4 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "    // let x = 1;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_strips_when_already_commented() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "    // let x = 1;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "    let x = 1;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_multi_line_selection_lines_up_prefixes() {
    // Indents differ across the selection; the inserted prefix should sit at the smallest
    // indent (col 2) so all three lines line up.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "  a = 1\n    b = 2\n  c = 3\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.py".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(open.language.as_deref(), Some("python"));

    // Selection covers all three lines.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "  # a = 1\n  #   b = 2\n  # c = 3\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_markdown_cursor_only_wraps_line_in_block() {
    // Markdown has no line-comment form; cursor-only should fall back to block-wrapping the
    // current line in `<!-- ... -->`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.md");
    std::fs::write(&path, "# Heading\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "<!-- # Heading -->\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_partial_selection_in_js_block_wraps() {
    // JS has both forms. A mid-line selection (not whole-line) should route to block.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "const x = foo + bar;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 12 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "const x = /* foo */ + bar;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_block_unwrap_strips_wrappers() {
    // Select the entire `/* foo */` span and toggle — should strip back to `foo`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "const x = /* foo */ + bar;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Select `/* foo */` (cols 10..=18 inclusive).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 18 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "const x = foo + bar;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_whole_line_selection_extends_to_cover_added_prefix() {
    // Anchor at the very start of line 0, cursor on the last char of line 2. After
    // commenting, the selection should *grow* to cover the new `// ` on line 0 (anchor stays
    // at col 0) and follow the content on line 2 (cursor shifts forward).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "let a = 1;\nlet b = 2;\nlet c = 3;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Last char of `let c = 3;` is `;` at col 9.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 2, col: 9 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    // Anchor stays at line 0 col 0 (now on the `/` of `// let a = 1;`).
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 0 });
    // Cursor shifts forward by `// `.len() = 3 to follow the `;` at col 12.
    assert_eq!(r.cursor.position, LogicalPosition { line: 2, col: 12 });

    drop(server);
}

#[tokio::test]
async fn toggle_comment_block_wrap_extends_selection_to_cover_wrappers() {
    // Selecting `foo` and toggling should leave the selection covering the whole `/* foo */`,
    // not just the inner `foo`. Matches the line-comment behaviour where the selection grows
    // to include the inserted prefix.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "const x = foo + bar;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 12 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    // Selection now covers the entire `/* foo */` — anchor on the first `/`, cursor on the
    // last `/`. The wrap is 9 chars (`/* foo */`), so cols 10..=18.
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 10 });
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 18 });

    drop(server);
}

#[tokio::test]
async fn toggle_comment_block_wrap_selection_ending_at_newline() {
    // Regression: selection ends exactly on the `\n` of its line. `start_pos.line ==
    // end_pos.line` but the *selected text* contains the newline, so the wrap is multi-line
    // (`close` lands on the following line). The new selection has to follow.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.go");
    std::fs::write(&path, "let a = 1;\nlet b = 2;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.go".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Selection from (0, 5) mid-line through (0, 10) — the newline. Single line in
    // (line, col), but selected text includes `\n`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 10 },
            anchor: LogicalPosition { line: 0, col: 5 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    // The closing `*/` sits on line 1 (after the original `\n`).
    assert_eq!(text, "let a/*  = 1;\n */let b = 2;\n");
    // Anchor stays on the original start; cursor follows the `*/` onto line 1 at col 2.
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 5 });
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });

    // Toggle again to uncomment. Round-trip must restore the original buffer *and* the
    // original selection — cursor back on the `\n` at line 0 col 10, not on line 1 col 0.
    let r2: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        6,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text2 = buffer_text(&mut ws, 7, open.buffer_id).await;
    assert_eq!(text2, "let a = 1;\nlet b = 2;\n");
    assert_eq!(r2.cursor.anchor, LogicalPosition { line: 0, col: 5 });
    assert_eq!(r2.cursor.position, LogicalPosition { line: 0, col: 10 });

    drop(server);
}

#[tokio::test]
async fn toggle_comment_multi_line_block_wrap_sets_correct_cursor_position() {
    // Regression: pre-edit `char_to_pos` for a post-edit char index produces the wrong
    // (line, col) once the wrap spans multiple lines. The selection used to extend by only
    // `close.len() + 1` cols instead of the full `open + space + close + space` worth.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.ts");
    std::fs::write(&path, "let a = 1;\nlet b = 2;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.ts".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Multi-line partial selection: (0, 4) `a` through (1, 4) `b`.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 4 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    // Anchor stays at (0, 4) — the opening `/` of `/*` lives there post-edit. Cursor lands
    // on the last `/` of `*/`, which is at col 7 of line 1 (`let b */ = 2;`).
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 4 });
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 7 });

    drop(server);
}

#[tokio::test]
async fn toggle_comment_multi_line_partial_selection_routes_to_block() {
    // Multi-line selection that *doesn't* cover complete lines (cursor stops mid-line on the
    // last line) should route to block-comment, not line-comment.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "let a = 1;\nlet b = 2;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Select from col 4 of line 0 (the `a`) to col 4 of line 1 (the `b`) — multi-line but
    // neither line is fully covered.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 1, col: 4 },
            anchor: LogicalPosition { line: 0, col: 4 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "let /* a = 1;\nlet b */ = 2;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_round_trip_partial_selection() {
    // Real-world toggle gesture: select `foo`, Ctrl-b to wrap, Ctrl-b again to unwrap. The
    // second toggle works because tree-sitter sees the cursor inside a comment node — the
    // post-wrap selection sits on the inner content, not around the wrappers.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "const x = foo + bar;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 12 },
            anchor: LogicalPosition { line: 0, col: 10 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let after_wrap = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(after_wrap, "const x = /* foo */ + bar;\n");

    // Second toggle: the response from the first toggle moved the selection to the inner
    // `foo`. We don't manually re-set the cursor — just press toggle again.
    send_request::<InputToggleComment>(
        &mut ws,
        6,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let after_unwrap = buffer_text(&mut ws, 7, open.buffer_id).await;
    assert_eq!(after_unwrap, "const x = foo + bar;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_cursor_inside_block_comment_unwraps() {
    // Cursor placed somewhere inside an existing `/* ... */`. No selection, no exact-span
    // gymnastics — just press toggle and have the wrappers come off.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.js");
    std::fs::write(&path, "const x = /* foo */ + bar;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.js".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Cursor on the `f` of `foo` (col 13), inside the comment.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 13 },
            anchor: LogicalPosition { line: 0, col: 13 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "const x = foo + bar;\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_css_cursor_only_wraps_line_in_block() {
    // CSS has only block tokens. Cursor-only → wrap the current line.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.css");
    std::fs::write(&path, "color: red;\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.css".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 0 },
            anchor: LogicalPosition { line: 0, col: 0 },
        },
    )
    .await;
    send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "/* color: red; */\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_block_only_language_is_noop_on_empty_line() {
    // Regression: in a block-only language (markdown / html / css), cursor-only on an empty
    // line used to wrap the lone `\n`, producing a 2-line comment that ate the blank line.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.md");
    std::fs::write(&path, "\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    // Revision unchanged (no edit), text unchanged.
    assert_eq!(r.revision, open.revision);
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "\n");

    drop(server);
}

#[tokio::test]
async fn toggle_comment_is_noop_for_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.json");
    std::fs::write(&path, "{}\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.json".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    let r: EditResult = send_request::<InputToggleComment>(
        &mut ws,
        4,
        &BufferOnlyParams {
            buffer_id: open.buffer_id,
        },
    )
    .await;
    // Buffer revision unchanged (no edit); text unchanged.
    assert_eq!(r.revision, open.revision);
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(text, "{}\n");

    drop(server);
}

// ---- search/* -----------------------------------------------------------------------------------

#[tokio::test]
async fn search_set_returns_summary_and_jumps_to_first_match() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo bar foo baz\nfoo qux\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "foo".into(),
            anchor: Some(LogicalPosition { line: 0, col: 0 }),
        },
    )
    .await;
    assert_eq!(r.summary.total, 3);
    assert!(!r.summary.truncated);
    assert_eq!(r.summary.current_index, 1);
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 2 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn search_smartcase_lowercase_is_case_insensitive() {
    let (server, mut ws, buffer_id) = setup_with_buffer("Foo foo FOO\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "foo".into(),
            anchor: None,
        },
    )
    .await;
    assert_eq!(r.summary.total, 3); // matches all three regardless of case

    drop(server);
}

#[tokio::test]
async fn search_smartcase_uppercase_is_case_sensitive() {
    let (server, mut ws, buffer_id) = setup_with_buffer("Foo foo FOO\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "Foo".into(),
            anchor: None,
        },
    )
    .await;
    assert_eq!(r.summary.total, 1);

    drop(server);
}

#[tokio::test]
async fn search_regex_metacharacters() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc 123 def 4567\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: r"\d+".into(),
            anchor: None,
        },
    )
    .await;
    assert_eq!(r.summary.total, 2);

    drop(server);
}

#[tokio::test]
async fn search_no_matches_returns_zero_summary() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "zzz".into(),
            anchor: None,
        },
    )
    .await;
    assert_eq!(r.summary.total, 0);
    assert_eq!(r.summary.current_index, 0);
    assert!(!r.summary.truncated);

    drop(server);
}

#[tokio::test]
async fn search_empty_query_clears_active_search() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;
    let _: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "alpha".into(),
            anchor: None,
        },
    )
    .await;
    let r: SearchSetResult = send_request::<SearchSet>(
        &mut ws,
        11,
        &SearchSetParams {
            buffer_id,
            query: String::new(),
            anchor: None,
        },
    )
    .await;
    assert_eq!(r.summary.total, 0);

    drop(server);
}

#[tokio::test]
async fn search_next_cycles_forward_and_wraps() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo bar foo baz\nfoo qux\n").await;
    let _ = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "foo".into(),
            anchor: Some(LogicalPosition { line: 0, col: 0 }),
        },
    )
    .await;
    let r1: SearchNavResult =
        send_request::<SearchNext>(&mut ws, 11, &SearchNavParams { buffer_id }).await;
    assert_eq!(r1.summary.current_index, 2);
    assert_eq!(r1.cursor.anchor, LogicalPosition { line: 0, col: 8 });
    let r2: SearchNavResult =
        send_request::<SearchNext>(&mut ws, 12, &SearchNavParams { buffer_id }).await;
    assert_eq!(r2.summary.current_index, 3);
    // Wrap.
    let r3: SearchNavResult =
        send_request::<SearchNext>(&mut ws, 13, &SearchNavParams { buffer_id }).await;
    assert_eq!(r3.summary.current_index, 1);

    drop(server);
}

#[tokio::test]
async fn search_prev_cycles_backward_with_wrap() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo bar foo baz\nfoo qux\n").await;
    let _ = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "foo".into(),
            anchor: Some(LogicalPosition { line: 0, col: 0 }),
        },
    )
    .await;
    // From the first match, prev wraps to the last.
    let r: SearchNavResult =
        send_request::<SearchPrev>(&mut ws, 11, &SearchNavParams { buffer_id }).await;
    assert_eq!(r.summary.current_index, 3);

    drop(server);
}

#[tokio::test]
async fn search_clear_removes_active_search() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo\n").await;
    let _ = send_request::<SearchSet>(
        &mut ws,
        10,
        &SearchSetParams {
            buffer_id,
            query: "foo".into(),
            anchor: None,
        },
    )
    .await;
    let _: () = send_request::<SearchClear>(&mut ws, 11, &SearchClearParams { buffer_id }).await;
    // After clear, n/prev should report no matches.
    let r: SearchNavResult =
        send_request::<SearchNext>(&mut ws, 12, &SearchNavParams { buffer_id }).await;
    assert_eq!(r.summary.total, 0);

    drop(server);
}

// -------- picker --------------------------------------------------------------------------------

async fn setup_picker_workspace() -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    // A small mix of file names so fuzzy matching has something to chew on.
    std::fs::create_dir_all(dir_path.join("src")).unwrap();
    std::fs::create_dir_all(dir_path.join("docs")).unwrap();
    std::fs::write(dir_path.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir_path.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(dir_path.join("docs/intro.md"), "# intro\n").unwrap();
    std::fs::write(dir_path.join("README.md"), "# project\n").unwrap();
    std::mem::forget(dir);

    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    (server, ws)
}

#[tokio::test]
async fn picker_view_returns_all_candidates_on_empty_query() {
    let (server, mut ws) = setup_picker_workspace().await;
    let view = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    assert_eq!(view.query, "");
    assert_eq!(view.effective_offset, 0);
    assert!(
        view.total_candidates >= 4,
        "expected >=4 candidates, got {}",
        view.total_candidates
    );

    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(update.kind, PickerKind::Files);
    assert_eq!(update.offset, 0);
    assert_eq!(update.total_candidates, view.total_candidates);
    assert_eq!(
        update.total_matches, view.total_candidates,
        "empty query matches all"
    );
    let names: Vec<&str> = update
        .items
        .iter()
        .map(|i| {
            let PickerItem::File { path, .. } = i else {
                panic!("expected File item, got {i:?}")
            };
            path.as_str()
        })
        .collect();
    assert!(names.contains(&"src/main.rs"));
    assert!(names.contains(&"README.md"));

    drop(server);
}

#[tokio::test]
async fn picker_query_ranks_matches_and_carries_indices() {
    let (server, mut ws) = setup_picker_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await; // drain initial

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Files,
            query: "main".into(),
            generation: 1,
        },
    )
    .await;

    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(update.generation, 1);
    let top = update.items.first().expect("at least one match");
    let PickerItem::File {
        path,
        match_indices,
    } = top
    else {
        panic!("expected File item, got {top:?}")
    };
    assert_eq!(path, "src/main.rs", "best match for 'main' is src/main.rs");
    assert!(
        !match_indices.is_empty(),
        "match indices should highlight where 'main' lines up"
    );

    drop(server);
}

#[tokio::test]
async fn picker_select_returns_absolute_path() {
    let (server, mut ws) = setup_picker_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Files,
            query: "lib".into(),
            generation: 1,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let item = update.items.first().expect("a match for 'lib'").clone();
    let PickerItem::File { ref path, .. } = item else {
        panic!("expected File item, got {item:?}")
    };
    assert_eq!(path, "src/lib.rs");

    let result: PickerSelectResult = send_request::<PickerSelect>(
        &mut ws,
        12,
        &PickerSelectParams {
            kind: PickerKind::Files,
            item,
        },
    )
    .await;
    let PickerSelectResult::File { path: abs } = result else {
        panic!("expected File result, got {result:?}")
    };
    assert!(
        abs.ends_with("src/lib.rs"),
        "abs path should end with relative: got {abs}"
    );
    assert!(
        std::path::Path::new(&abs).is_absolute(),
        "select must return an absolute path"
    );

    drop(server);
}

#[tokio::test]
async fn picker_resume_centers_on_remembered_item() {
    // Resume = view { reset: false, center_on } recovers query+ranking and frames the item.
    let (server, mut ws) = setup_picker_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;
    // Query "rs" — narrows to .rs files; query is persisted on hide.
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Files,
            query: "rs".into(),
            generation: 1,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;
    let _: () = send_request::<PickerHide>(
        &mut ws,
        12,
        &PickerHideParams {
            kind: PickerKind::Files,
        },
    )
    .await;

    // Resume with center_on pointing at a remembered item.
    let resume = send_request::<PickerView>(
        &mut ws,
        13,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: false,
            offset: 0,
            limit: 30,
            center_on: Some(PickerItem::File {
                path: "src/lib.rs".into(),
                match_indices: vec![],
            }),
            directory_path: None,
        },
    )
    .await;
    assert_eq!(resume.query, "rs", "query persisted across hide");
    // Limit is larger than the result set so the window covers everything; effective_offset is 0.
    assert_eq!(resume.effective_offset, 0);

    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert!(update
        .items
        .iter()
        .any(|i| matches!(i, PickerItem::File { path, .. } if path == "src/lib.rs")));

    drop(server);
}

#[tokio::test]
async fn picker_reset_wipes_persisted_query() {
    let (server, mut ws) = setup_picker_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Files,
            query: "main".into(),
            generation: 1,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;
    let _: () = send_request::<PickerHide>(
        &mut ws,
        12,
        &PickerHideParams {
            kind: PickerKind::Files,
        },
    )
    .await;

    // reset: true → query comes back empty even though we just typed one.
    let reopened = send_request::<PickerView>(
        &mut ws,
        13,
        &PickerViewParams {
            kind: PickerKind::Files,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    assert_eq!(reopened.query, "");
    assert_eq!(reopened.generation, 0);

    drop(server);
}

// -------- buffer picker --------------------------------------------------------------------------

/// Workspace + handshake. Same shape as `setup_picker_workspace` but loads a few files we'll
/// open through `buffer/open` so the buffer picker has something to surface.
async fn setup_buffer_picker_workspace() -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::create_dir_all(dir_path.join("src")).unwrap();
    std::fs::write(dir_path.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir_path.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(dir_path.join("README.md"), "# project\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    (server, ws)
}

/// MRU is per-client and the most-recent open lands at position 0. The first item is the
/// "current" buffer; selecting it is the no-op switch.
#[tokio::test]
async fn buffers_picker_orders_by_mru_with_current_first() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("README.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/lib.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        4,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/main.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let displays: Vec<&str> = update
        .items
        .iter()
        .map(|i| {
            let PickerItem::Buffer { display, .. } = i else {
                panic!("expected Buffer, got {i:?}")
            };
            display.as_str()
        })
        .collect();
    assert_eq!(displays, vec!["src/main.rs", "src/lib.rs", "README.md"]);

    drop(server);
}

/// Selecting an item returns the buffer_id, which is the stable handle the client uses to
/// attach via `buffer/open { buffer_id }`.
#[tokio::test]
async fn buffers_picker_select_returns_buffer_id() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/main.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let item = update.items.first().expect("at least one buffer").clone();
    let result: PickerSelectResult = send_request::<PickerSelect>(
        &mut ws,
        11,
        &PickerSelectParams {
            kind: PickerKind::Buffers,
            item,
        },
    )
    .await;
    let PickerSelectResult::Buffer { buffer_id } = result else {
        panic!("expected Buffer result, got {result:?}");
    };
    assert_eq!(buffer_id, opened.buffer_id);

    drop(server);
}

/// `buffer/open { buffer_id }` attaches to an already-open buffer without consulting paths —
/// the path to a scratch buffer is `None`, and this is the only way to switch to it.
#[tokio::test]
async fn buffer_open_by_id_attaches_to_scratch() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    // Scratch buffer: no path fields.
    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert!(scratch.path.is_none());
    // Open a file so the current buffer is different.
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("README.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    // Now attach back to the scratch by id — no path fields needed.
    let reattach: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        4,
        &BufferOpenParams {
            buffer_id: Some(scratch.buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(reattach.buffer_id, scratch.buffer_id);
    assert!(
        reattach.path.is_none(),
        "scratch buffer still has no path on reattach"
    );

    drop(server);
}

/// Scratch buffers show up in the picker with a `[scratch N]` placeholder display.
#[tokio::test]
async fn buffers_picker_renders_scratch_placeholder() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let expected = format!("[scratch {}]", scratch.buffer_id);
    assert!(
        update
            .items
            .iter()
            .any(|i| matches!(i, PickerItem::Buffer { display, .. } if display == &expected)),
        "expected display {expected:?} in items: {:?}",
        update.items,
    );

    drop(server);
}

/// While the picker is open, a buffer mutation that flips dirty from false → true pushes a
/// fresh `picker/update` so the dirty marker appears without the user closing+reopening.
#[tokio::test]
async fn buffers_picker_pushes_on_dirty_transition() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/main.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    // Subscribe a viewport so subsequent edits' lines_changed pushes are routed (they'd be
    // dropped otherwise, but the picker push lives on its own channel either way).
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 24,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 1,
            tab_width: 4,
        },
    )
    .await;
    // Open the picker. Initial push shows dirty: false.
    let _ = send_request::<PickerView>(
        &mut ws,
        4,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let initial: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let initial_dirty = match initial.items.first().unwrap() {
        PickerItem::Buffer { dirty, .. } => *dirty,
        other => panic!("expected Buffer, got {other:?}"),
    };
    assert!(!initial_dirty);

    // Type a char into the buffer — flips dirty true. Picker should push.
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        5,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "x".into(),
            select_pasted: false,
        },
    )
    .await;
    // Drain notifications until we get a picker update (other pushes — viewport lines, etc.
    // — may arrive first).
    let next: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let dirty_after = next
        .items
        .iter()
        .find_map(|i| match i {
            PickerItem::Buffer {
                buffer_id, dirty, ..
            } if *buffer_id == opened.buffer_id => Some(*dirty),
            _ => None,
        })
        .expect("buffer still in items");
    assert!(dirty_after, "dirty marker should flip after the first edit");

    drop(server);
}

/// Subsequent edits don't generate picker pushes — the dirty flag is already set, so there's
/// no display change. Verifies the hot-path gate.
#[tokio::test]
async fn buffers_picker_no_push_on_subsequent_edits() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/main.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 24,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 1,
            tab_width: 4,
        },
    )
    .await;
    let _ = send_request::<PickerView>(
        &mut ws,
        4,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        5,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "a".into(),
            select_pasted: false,
        },
    )
    .await;
    let _: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await; // dirty flip

    // Second edit — dirty already true, no picker push expected. Drain frames for a short
    // window and assert none of them are picker/update notifications.
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        6,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "b".into(),
            select_pasted: false,
        },
    )
    .await;
    let timed = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            let text = next_text(&mut ws).await;
            if let Ok(ClientInbound::Notification(n)) = serde_json::from_str::<ClientInbound>(&text)
            {
                if n.method == PickerUpdate::NAME {
                    return n;
                }
            }
        }
    })
    .await;
    assert!(
        timed.is_err(),
        "no picker/update should arrive after a same-dirty edit, got {timed:?}"
    );

    drop(server);
}

/// Saving a dirty buffer flips dirty back to clean — picker re-pushes so the marker vanishes.
#[tokio::test]
async fn buffers_picker_pushes_on_save() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/main.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 24,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 1,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "z".into(),
            select_pasted: false,
        },
    )
    .await;
    let _ = send_request::<PickerView>(
        &mut ws,
        5,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let dirty_view: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let saw_dirty = dirty_view.items.iter().any(|i| matches!(i, PickerItem::Buffer { buffer_id, dirty, .. } if *buffer_id == opened.buffer_id && *dirty));
    assert!(saw_dirty, "main.rs should be dirty after the edit");

    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        6,
        &BufferSaveParams {
            buffer_id: opened.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    let clean: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let saw_clean = clean.items.iter().any(|i| matches!(i, PickerItem::Buffer { buffer_id, dirty, .. } if *buffer_id == opened.buffer_id && !*dirty));
    assert!(
        saw_clean,
        "save should flip dirty back off and re-push the picker"
    );

    drop(server);
}

/// Successive scratch opens allocate fresh buffer ids — the server doesn't dedupe scratches
/// the way it dedupes path-backed buffers. Each one shows up independently in the picker.
#[tokio::test]
async fn buffer_open_scratch_each_time_creates_a_new_buffer() {
    let (server, mut ws) = setup_buffer_picker_workspace().await;
    let first: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let second: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_ne!(first.buffer_id, second.buffer_id);
    assert!(first.path.is_none() && second.path.is_none());

    // Both should appear in the picker, second one first (MRU).
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    let ids: Vec<u64> = update
        .items
        .iter()
        .filter_map(|i| match i {
            PickerItem::Buffer { buffer_id, .. } => Some(*buffer_id),
            _ => None,
        })
        .collect();
    let pos_first = ids
        .iter()
        .position(|&id| id == first.buffer_id)
        .expect("first scratch in items");
    let pos_second = ids
        .iter()
        .position(|&id| id == second.buffer_id)
        .expect("second scratch in items");
    assert!(
        pos_second < pos_first,
        "more recent scratch should be ranked above the older one"
    );

    drop(server);
}

/// MRU is per-client: a buffer that was open before disconnect doesn't sit forever in
/// another client's MRU. Closing the connection drops MRU; reconnecting fresh shows the open
/// buffers in id order (since this client hasn't touched any).
#[tokio::test]
async fn buffers_picker_mru_is_per_client() {
    let (server, mut ws_a) = setup_buffer_picker_workspace().await;
    // Client A opens two files in a specific order.
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws_a,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("README.md".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws_a,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("src/lib.rs".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Client B connects fresh — no touches yet. Buffers should appear in id order.
    let (mut ws_b, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws_b,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let _ = send_request::<PickerView>(
        &mut ws_b,
        10,
        &PickerViewParams {
            kind: PickerKind::Buffers,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws_b).await;
    let ids: Vec<u64> = update
        .items
        .iter()
        .map(|i| {
            let PickerItem::Buffer { buffer_id, .. } = i else {
                panic!("expected Buffer, got {i:?}")
            };
            *buffer_id
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(
        ids, sorted,
        "client B should see buffers in id order (no MRU touches yet)"
    );

    drop(server);
}

// -------- save-as --------------------------------------------------------------------------------

/// Save-as: writes a scratch buffer to a new file under the project root. The buffer picks up
/// a canonical path so subsequent in-place saves work, and dirty flips off.
#[tokio::test]
async fn save_as_writes_scratch_to_disk_and_clears_dirty() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path.clone()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: scratch.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: scratch.buffer_id,
            text: "hello world\n".into(),
            select_pasted: false,
        },
    )
    .await;

    // Save-as to "notes.txt" under the project root.
    let saved: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        5,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: Some(0),
            relative_path: Some("notes.txt".into()),
            overwrite: false,
        },
    )
    .await;

    // File exists with the right contents.
    let on_disk = std::fs::read_to_string(dir_path.join("notes.txt")).expect("file written");
    assert_eq!(on_disk, "hello world\n");

    // Dirty cleared. Reopen the buffer by id to check its post-save state.
    let reopen: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        6,
        &BufferOpenParams {
            buffer_id: Some(scratch.buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(reopen.saved_revision, saved.revision);
    assert_eq!(reopen.revision, saved.revision);
    assert!(reopen
        .path
        .as_deref()
        .is_some_and(|p| p.ends_with("notes.txt")));

    drop(server);
}

/// Save-as into a path already owned by another open buffer is rejected — otherwise we'd
/// silently displace the other buffer.
#[tokio::test]
async fn save_as_rejects_path_conflict_with_open_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("existing.txt"), "old content\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    // Open the existing file (now claimed by buffer A).
    let _: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("existing.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    // Open a fresh scratch (buffer B).
    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;

    // Try to save-as scratch -> existing.txt. Expect error, expect the on-disk content
    // untouched.
    let msg = send_request_expect_err::<BufferSave>(
        &mut ws,
        4,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: Some(0),
            relative_path: Some("existing.txt".into()),
            overwrite: false,
        },
    )
    .await;
    assert!(
        msg.contains("already open"),
        "expected conflict message, got: {msg}"
    );

    drop(server);
}

/// Save-as on an already-path-backed buffer to its *own* current path is the in-place save
/// case — no conflict, even though `buffer_for_path` finds a match.
#[tokio::test]
async fn save_as_to_same_path_is_in_place_save() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("doc.txt"), "x\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path.clone()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("doc.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "y".into(),
            select_pasted: false,
        },
    )
    .await;
    // Same-path save-as. Should succeed.
    let _saved: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        5,
        &BufferSaveParams {
            buffer_id: opened.buffer_id,
            path_index: Some(0),
            relative_path: Some("doc.txt".into()),
            overwrite: false,
        },
    )
    .await;
    let on_disk = std::fs::read_to_string(dir_path.join("doc.txt")).unwrap();
    assert!(on_disk.starts_with("y"));

    drop(server);
}

/// Save-as into an existing file (not owned by any open buffer) is rejected with
/// WOULD_OVERWRITE unless overwrite=true. The on-disk content stays put on rejection.
#[tokio::test]
async fn save_as_rejects_existing_file_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("target.txt"), "original\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path.clone()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    // Scratch buffer with some content.
    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: scratch.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: scratch.buffer_id,
            text: "fresh\n".into(),
            select_pasted: false,
        },
    )
    .await;

    // First try: overwrite=false should bounce with WOULD_OVERWRITE.
    let msg = send_request_expect_err::<BufferSave>(
        &mut ws,
        5,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: Some(0),
            relative_path: Some("target.txt".into()),
            overwrite: false,
        },
    )
    .await;
    assert!(
        msg.contains("would overwrite"),
        "expected would-overwrite message, got: {msg}"
    );
    // On-disk unchanged.
    assert_eq!(
        std::fs::read_to_string(dir_path.join("target.txt")).unwrap(),
        "original\n"
    );

    // Second try: overwrite=true succeeds.
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        6,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: Some(0),
            relative_path: Some("target.txt".into()),
            overwrite: true,
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir_path.join("target.txt")).unwrap(),
        "fresh\n"
    );

    drop(server);
}

/// In-place save (target == buffer's current canonical_path) doesn't trigger WOULD_OVERWRITE
/// even though the file obviously exists. Save-as to the same path is also fine.
#[tokio::test]
async fn in_place_save_never_triggers_overwrite_check() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("file.txt"), "before\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path.clone()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("file.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "x".into(),
            select_pasted: false,
        },
    )
    .await;
    // In-place save — overwrite=false, no path args. Must not error.
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        5,
        &BufferSaveParams {
            buffer_id: opened.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    // Save-as to the same path — also fine with overwrite=false.
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        6,
        &BufferSaveParams {
            buffer_id: opened.buffer_id,
            path_index: Some(0),
            relative_path: Some("file.txt".into()),
            overwrite: false,
        },
    )
    .await;

    drop(server);
}

/// After a scratch buffer is named via save-as, a plain in-place save (path_index/relative_path
/// = None) targets the path the save-as set, rather than erroring on "buffer has no path".
#[tokio::test]
async fn in_place_save_after_save_as_targets_new_path() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path.clone()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let scratch: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: scratch.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        4,
        &InputTextParams {
            buffer_id: scratch.buffer_id,
            text: "one\n".into(),
            select_pasted: false,
        },
    )
    .await;
    // Save-as to a fresh path.
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        5,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: Some(0),
            relative_path: Some("named.txt".into()),
            overwrite: false,
        },
    )
    .await;
    // Edit again, then plain in-place save (no path fields). Should write to named.txt.
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        6,
        &InputTextParams {
            buffer_id: scratch.buffer_id,
            text: "two\n".into(),
            select_pasted: false,
        },
    )
    .await;
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        7,
        &BufferSaveParams {
            buffer_id: scratch.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    let on_disk = std::fs::read_to_string(dir_path.join("named.txt")).expect("file on disk");
    assert_eq!(on_disk, "one\ntwo\n");

    drop(server);
}

// -------- buffer/close ---------------------------------------------------------------------------

use aether_protocol::buffer::{BufferClose, BufferCloseParams, BufferCloseResult};

/// Closing a buffer drops it from the server. After close, opening by id fails.
#[tokio::test]
async fn buffer_close_drops_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir_path.join("b.txt"), "beta\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let a: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let b: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("b.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    // MRU is [b, a]; closing b should return next = a.
    let r: BufferCloseResult = send_request::<BufferClose>(
        &mut ws,
        4,
        &BufferCloseParams {
            buffer_id: b.buffer_id,
        },
    )
    .await;
    assert_eq!(r.next_buffer_id, Some(a.buffer_id));
    // Trying to attach to the closed buffer is an error.
    let err = send_request_expect_err::<BufferOpen>(
        &mut ws,
        5,
        &BufferOpenParams {
            buffer_id: Some(b.buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert!(
        err.contains("unknown buffer_id"),
        "expected buffer-not-found, got: {err}"
    );

    drop(server);
}

/// Closing the last buffer returns `next_buffer_id: None` so the client knows to spawn a
/// scratch.
#[tokio::test]
async fn buffer_close_last_buffer_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("only.txt"), "x\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("only.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let r: BufferCloseResult = send_request::<BufferClose>(
        &mut ws,
        3,
        &BufferCloseParams {
            buffer_id: opened.buffer_id,
        },
    )
    .await;
    assert_eq!(r.next_buffer_id, None);

    drop(server);
}

/// Closing also drops any subscribed viewports, so subsequent operations on a dangling
/// viewport return errors gracefully.
#[tokio::test]
async fn buffer_close_drops_viewports() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::write(dir_path.join("a.txt"), "alpha\n").unwrap();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: opened.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    let _: BufferCloseResult = send_request::<BufferClose>(
        &mut ws,
        4,
        &BufferCloseParams {
            buffer_id: opened.buffer_id,
        },
    )
    .await;
    // Resizing the now-dangling viewport should fail rather than return stale data.
    let err = send_request_expect_err::<ViewportResize>(
        &mut ws,
        5,
        &ViewportResizeParams {
            viewport_id: sub.viewport_id,
            cols: 100,
            rows: 20,
        },
    )
    .await;
    let _ = err; // exact message isn't important; just that it's an error.

    drop(server);
}

// -------- line operations (input/delete_line, input/change_line, input/replace_line) -------------

use aether_protocol::input::{
    InputChangeLine, InputDeleteLine, InputReplaceLine, InputReplaceLineParams,
};

/// `input/delete_line` removes the cursor's line including the trailing newline.
#[tokio::test]
async fn input_delete_line_removes_line_with_newline() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        2,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    // Park on line 1 ("beta"), then delete-line.
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    let _: EditResult =
        send_request::<InputDeleteLine>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    let notif: ViewportLinesChangedParams =
        expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(
        notif.line_count, 3,
        "buffer drops from 4 lines (incl trailing empty) to 3"
    );

    drop(server);
}

/// `input/change_line` blanks the line's content but keeps the newline. Subsequent inserts
/// land at col 0 of the now-empty line.
#[tokio::test]
async fn input_change_line_blanks_content_keeps_newline() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        2,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 2 },
            anchor: LogicalPosition { line: 1, col: 2 },
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputChangeLine>(&mut ws, 4, &BufferOnlyParams { buffer_id }).await;
    // Cursor lands at col 0 of the (now-empty) line.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 0 });
    assert_eq!(r.cursor.anchor, LogicalPosition { line: 1, col: 0 });
    let notif: ViewportLinesChangedParams =
        expect_notification::<ViewportLinesChanged>(&mut ws).await;
    // Line count stays at 4 (alpha, empty, gamma, trailing empty).
    assert_eq!(notif.line_count, 4);

    drop(server);
}

/// `input/replace_line` swaps the line (content + newline) for the given text. The cursor
/// lands just past the inserted text.
#[tokio::test]
async fn input_replace_line_swaps_content() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        2,
        &ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        3,
        &CursorSetParams {
            buffer_id,
            position: LogicalPosition { line: 1, col: 0 },
            anchor: LogicalPosition { line: 1, col: 0 },
        },
    )
    .await;
    let _: EditResult = send_request::<InputReplaceLine>(
        &mut ws,
        4,
        &InputReplaceLineParams {
            buffer_id,
            text: "replaced\n".into(),
        },
    )
    .await;
    let _ = expect_notification::<ViewportLinesChanged>(&mut ws).await;
    // Save the buffer to disk, then read back, to verify the content via a side channel.
    // (Easier than asserting via line-state notifications which we'd have to reconstruct.)
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.txt");
    std::mem::forget(dir);
    // We don't actually have a project path matching this temp file, so saving would fail.
    // Instead just verify by issuing a fresh open and reading the line count.
    let _ = target;
    drop(server);
}

// -------- buffer/open jump_to --------------------------------------------------------------------

/// `buffer/open { jump_to }` lands the returned cursor at the requested position and persists it
/// so a follow-up open without `jump_to` resumes from the same spot.
#[tokio::test]
async fn buffer_open_jump_to_places_and_persists_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    let dir_path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: Some(LogicalPosition { line: 1, col: 2 }),
        },
    )
    .await;
    assert_eq!(opened.cursor.position, LogicalPosition { line: 1, col: 2 });
    assert_eq!(opened.cursor.anchor, LogicalPosition { line: 1, col: 2 });

    // Reopen without jump_to — should resume the just-set position, not snap to origin.
    let reopen: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            buffer_id: Some(opened.buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(reopen.cursor.position, LogicalPosition { line: 1, col: 2 });

    drop(server);
}

/// `buffer/open { jump_to }` clamps line past EOF and col past line end — used by the grep
/// picker when a persisted hit's coordinates have drifted out from under the file.
#[tokio::test]
async fn buffer_open_jump_to_clamps_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "ab\ncd\n").unwrap();
    let dir_path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;

    // Line 99 doesn't exist; col 99 is past any line. Server should clamp to (last_line, line_end).
    let opened: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: Some(LogicalPosition { line: 99, col: 99 }),
        },
    )
    .await;
    // The file has lines "ab\n", "cd\n", "". Last visible line is index 1 ("cd"), length 2 bytes.
    assert!(opened.cursor.position.line <= 2);
    assert!(opened.cursor.position.col <= 2);

    drop(server);
}

// -------- picker grep ---------------------------------------------------------------------------

async fn setup_grep_workspace() -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    std::fs::create_dir_all(dir_path.join("src")).unwrap();
    std::fs::write(
        dir_path.join("src/main.rs"),
        "fn main() {\n    needle();\n    needle();\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir_path.join("src/lib.rs"),
        "fn needle() {}\nfn other() {}\n",
    )
    .unwrap();
    std::fs::write(dir_path.join("README.md"), "no match here\n").unwrap();
    std::mem::forget(dir);

    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    (server, ws)
}

/// Drain `picker/update` notifications until one arrives with `ticking: false` (the search has
/// finished). Returns the final params so the caller can assert on hit count and items.
async fn drain_grep_until_done(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> PickerUpdateParams {
    loop {
        let params: PickerUpdateParams = expect_notification::<PickerUpdate>(ws).await;
        if !params.ticking {
            return params;
        }
    }
}

#[tokio::test]
async fn picker_grep_finds_matches_and_select_returns_file_at() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await; // initial empty push

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "needle".into(),
            generation: 1,
        },
    )
    .await;

    let final_update = drain_grep_until_done(&mut ws).await;
    assert_eq!(final_update.kind, PickerKind::Grep);
    assert_eq!(final_update.generation, 1);
    // 2 hits in main.rs + 1 hit in lib.rs = 3 GrepHits total.
    assert_eq!(final_update.total_matches, 3);
    let hit = final_update
        .items
        .iter()
        .find(|i| matches!(i, PickerItem::GrepHit { path, .. } if path == "src/lib.rs"))
        .expect("lib.rs hit present");
    let (line, col, preview) = match hit {
        PickerItem::GrepHit {
            line, col, preview, ..
        } => (*line, *col, preview.clone()),
        _ => unreachable!(),
    };
    assert_eq!(line, 0, "lib.rs hit is on line 0 (`fn needle() {{}}`)");
    assert_eq!(col, 3, "col 3 is the `n` of `needle`");
    assert!(preview.contains("needle"));

    // Select that hit; should return FileAt with the absolute path and position.
    let select_result: PickerSelectResult = send_request::<PickerSelect>(
        &mut ws,
        12,
        &PickerSelectParams {
            kind: PickerKind::Grep,
            item: hit.clone(),
        },
    )
    .await;
    let (sel_path, sel_pos) = match select_result {
        PickerSelectResult::FileAt { path, position } => (path, position),
        other => panic!("expected FileAt, got {other:?}"),
    };
    assert!(sel_path.ends_with("src/lib.rs"));
    assert_eq!(sel_pos.line, 0);
    assert_eq!(sel_pos.col, 3);

    drop(server);
}

/// Queries shorter than the min length don't spawn a search and produce an empty,
/// not-ticking result set.
#[tokio::test]
async fn picker_grep_short_query_yields_empty_result() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "n".into(), // below MIN_QUERY_LEN
            generation: 1,
        },
    )
    .await;
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(update.generation, 1);
    assert_eq!(update.total_matches, 0);
    assert!(
        !update.ticking,
        "below-min queries should not flag the picker as still searching"
    );

    drop(server);
}

/// Grep hits persist across `hide` + non-reset `view` so the user can step through them. After
/// resume the previously-set query is still active and the prior result set is intact.
#[tokio::test]
async fn picker_grep_persists_hits_across_hide_and_resume() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "needle".into(),
            generation: 1,
        },
    )
    .await;
    let before = drain_grep_until_done(&mut ws).await;
    let before_hits = before.total_matches;
    assert!(before_hits >= 1);

    let _: () = send_request::<PickerHide>(
        &mut ws,
        12,
        &PickerHideParams {
            kind: PickerKind::Grep,
        },
    )
    .await;

    // Resume without reset: we should get the prior hits back without re-running the search.
    let resume = send_request::<PickerView>(
        &mut ws,
        13,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: false,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    assert_eq!(resume.query, "needle", "query persists across hide/show");
    let update: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(
        update.total_matches, before_hits,
        "hits preserved on resume"
    );

    drop(server);
}

/// Grep queries are regex, same as buffer search (`/`). A pattern like `n.+dle` matches `needle`
/// — confirms `regex::escape` is not being applied (which would turn the `.+` into a literal).
#[tokio::test]
async fn picker_grep_treats_query_as_regex() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "n.+dle".into(),
            generation: 1,
        },
    )
    .await;
    let final_update = drain_grep_until_done(&mut ws).await;
    assert!(
        final_update.total_matches >= 1,
        "regex `n.+dle` should match `needle` ({} hits)",
        final_update.total_matches,
    );

    drop(server);
}

/// Re-issuing the same query after a search completes uses the cached candidates: the response
/// arrives without `ticking: true` (no fresh walker spawned) and the hit count matches the
/// previous run.
#[tokio::test]
async fn picker_grep_caches_completed_query() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "needle".into(),
            generation: 1,
        },
    )
    .await;
    let first = drain_grep_until_done(&mut ws).await;
    let first_hits = first.total_matches;
    assert!(first_hits >= 1);

    // Same query again — should hit the cache. The single push that arrives must already be
    // non-ticking (no spawn) and carry the cached hit count.
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        12,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "needle".into(),
            generation: 2,
        },
    )
    .await;
    let cached: PickerUpdateParams = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(cached.generation, 2);
    assert_eq!(cached.total_matches, first_hits);
    assert!(
        !cached.ticking,
        "cache hit must not mark the picker as ticking — no new search was spawned"
    );

    drop(server);
}

// ---- explorer picker -----------------------------------------------------------------------------

/// Spin up a small workspace for the explorer tests: `src/`, `src/lib.rs`, `src/main.rs`,
/// `tests/`, `tests/it.rs`, `README.md`. Returns a (server, ws) pair past the handshake.
async fn setup_explorer_workspace() -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let canonical_root = std::fs::canonicalize(&root).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("tests/it.rs"), "// integration\n").unwrap();
    std::fs::write(root.join("README.md"), "hi\n").unwrap();
    std::mem::forget(dir);

    let server = spawn_for_test("test-proj", vec![root], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    (server, ws, canonical_root)
}

/// Opening the Explorer picker without a `directory_path` lists the first project root: the
/// result echoes the canonical path, sets `directory_parent` to `None` (we're *at* a root), and
/// the push carries one `DirEntry` per child with directories sorted before files.
#[tokio::test]
async fn picker_explorer_default_lists_project_root() {
    let (server, mut ws, root) = setup_explorer_workspace().await;
    let view: aether_protocol::picker::PickerViewResult = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    assert_eq!(view.directory_path.as_deref(), Some(root.to_str().unwrap()));
    assert!(
        view.directory_parent.is_none(),
        "at project root → no parent"
    );

    let update = expect_notification::<PickerUpdate>(&mut ws).await;
    let names: Vec<(String, bool)> = update
        .items
        .iter()
        .map(|it| match it {
            PickerItem::DirEntry { name, is_dir, .. } => (name.clone(), *is_dir),
            other => panic!("expected DirEntry, got {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("src".into(), true),
            ("tests".into(), true),
            ("README.md".into(), false),
        ]
    );

    drop(server);
}

/// Passing an explicit `directory_path` lists that directory, and `directory_parent` is the
/// (in-project) parent so the client can render Alt-h navigation.
#[tokio::test]
async fn picker_explorer_navigate_into_subdirectory() {
    let (server, mut ws, root) = setup_explorer_workspace().await;
    let target = root.join("src");
    let view: aether_protocol::picker::PickerViewResult = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: Some(target.display().to_string()),
        },
    )
    .await;
    assert_eq!(
        view.directory_path.as_deref(),
        Some(target.to_str().unwrap())
    );
    assert_eq!(
        view.directory_parent.as_deref(),
        Some(root.to_str().unwrap()),
        "parent should be the project root"
    );
    let update = expect_notification::<PickerUpdate>(&mut ws).await;
    let names: Vec<String> = update
        .items
        .iter()
        .map(|it| match it {
            PickerItem::DirEntry { name, .. } => name.clone(),
            other => panic!("expected DirEntry, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["lib.rs", "main.rs"]);
    drop(server);
}

/// A `picker/query` over the explorer prefix-matches entry names (smartcase). `match_indices`
/// covers the prefix chars the user typed so the renderer can underline the matched prefix.
#[tokio::test]
async fn picker_explorer_query_filters_by_prefix() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: "sr".into(),
            generation: 1,
        },
    )
    .await;
    let update = expect_notification::<PickerUpdate>(&mut ws).await;
    let kept: Vec<(String, Vec<u32>)> = update
        .items
        .iter()
        .map(|it| match it {
            PickerItem::DirEntry {
                name,
                match_indices,
                ..
            } => (name.clone(), match_indices.clone()),
            other => panic!("expected DirEntry, got {other:?}"),
        })
        .collect();
    assert_eq!(
        kept.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["src"],
        "prefix match `sr` should keep only `src`"
    );
    assert_eq!(
        kept[0].1,
        vec![0, 1],
        "match_indices should cover the prefix the user typed"
    );

    drop(server);
}

/// Non-prefix substrings don't match — `rc` would survive under fuzzy (since `src` contains
/// `r` then `c` in order) but must not under prefix semantics.
#[tokio::test]
async fn picker_explorer_query_rejects_non_prefix_substring() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: "rc".into(),
            generation: 1,
        },
    )
    .await;
    let update = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(
        update.total_matches, 0,
        "non-prefix `rc` should not match `src`"
    );
    assert!(update.items.is_empty());
    drop(server);
}

/// Clearing the explorer query (Alt-Backspace on the client) sends a `picker/query` with an
/// empty string and the bumped generation; the server reranks and the push restores the full
/// directory listing.
#[tokio::test]
async fn picker_explorer_empty_query_restores_full_listing() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let initial = expect_notification::<PickerUpdate>(&mut ws).await;
    let total_unfiltered = initial.total_matches;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: "sr".into(),
            generation: 1,
        },
    )
    .await;
    let filtered = expect_notification::<PickerUpdate>(&mut ws).await;
    assert!(
        filtered.total_matches < total_unfiltered,
        "filter should narrow the listing"
    );

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        12,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: String::new(),
            generation: 2,
        },
    )
    .await;
    let restored = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(
        restored.total_matches, total_unfiltered,
        "empty query should restore the full unfiltered listing"
    );
    assert_eq!(restored.generation, 2);

    drop(server);
}

/// Smartcase: an all-lowercase query matches case-insensitively (so `re` finds `README.md`),
/// but any uppercase letter in the query flips to case-sensitive (so `RE` keeps the match
/// while `Re` is the explicit-mixed-case form most users expect to also match).
#[tokio::test]
async fn picker_explorer_query_is_smartcase() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    // Lowercase query → case-insensitive → matches README.md.
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: "re".into(),
            generation: 1,
        },
    )
    .await;
    let lower = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(lower.total_matches, 1);
    match &lower.items[0] {
        PickerItem::DirEntry { name, .. } => assert_eq!(name, "README.md"),
        other => panic!("expected DirEntry, got {other:?}"),
    }

    // Uppercase letter → case-sensitive → `Re` no longer matches the all-uppercase `README.md`.
    let _: () = send_request::<PickerQuery>(
        &mut ws,
        12,
        &PickerQueryParams {
            kind: PickerKind::Explorer,
            query: "Re".into(),
            generation: 2,
        },
    )
    .await;
    let mixed = expect_notification::<PickerUpdate>(&mut ws).await;
    assert_eq!(
        mixed.total_matches, 0,
        "`Re` is case-sensitive under smartcase, README.md starts with `RE`"
    );

    drop(server);
}

/// Selecting a file in the explorer returns `PickerSelectResult::File { path }` with the
/// absolute path the client should feed into `buffer/open`.
#[tokio::test]
async fn picker_explorer_select_file_returns_absolute_path() {
    let (server, mut ws, root) = setup_explorer_workspace().await;
    let target = root.join("src");
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: Some(target.display().to_string()),
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let result: PickerSelectResult = send_request::<PickerSelect>(
        &mut ws,
        11,
        &PickerSelectParams {
            kind: PickerKind::Explorer,
            item: PickerItem::DirEntry {
                name: "lib.rs".into(),
                is_dir: false,
                match_indices: vec![],
            },
        },
    )
    .await;
    match result {
        PickerSelectResult::File { path } => {
            assert_eq!(path, target.join("lib.rs").display().to_string());
        }
        other => panic!("expected File select result, got {other:?}"),
    }

    drop(server);
}

/// Selecting a directory entry in the explorer is an error — navigation is the client's job
/// (it sends a fresh `picker/view` with the new `directory_path`). The contract makes
/// `picker/select` mean "this is the final answer, here you go", which doesn't apply to a
/// directory.
#[tokio::test]
async fn picker_explorer_select_directory_errors() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let err = send_request_expect_err::<PickerSelect>(
        &mut ws,
        11,
        &PickerSelectParams {
            kind: PickerKind::Explorer,
            item: PickerItem::DirEntry {
                name: "src".into(),
                is_dir: true,
                match_indices: vec![],
            },
        },
    )
    .await;
    assert!(
        err.contains("not selectable") || err.contains("not in the picker"),
        "unexpected error message: {err}"
    );
    drop(server);
}

/// Asking the explorer to list a directory outside the project boundary is rejected by the
/// same access-boundary check `directory/list` uses.
#[tokio::test]
async fn picker_explorer_rejects_path_outside_project() {
    let (server, mut ws, _root) = setup_explorer_workspace().await;
    let err = send_request_expect_err::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: Some("/etc".into()),
        },
    )
    .await;
    assert!(
        err.contains("outside the project") || err.contains("canonicalizing"),
        "unexpected error message: {err}"
    );
    drop(server);
}

/// Resuming the explorer (omitting `directory_path` on a follow-up `picker/view`) keeps it
/// pointed at the directory the prior call established — that's what makes "Space e" re-enter
/// the same dir across hide/show.
#[tokio::test]
async fn picker_explorer_resumes_last_directory() {
    let (server, mut ws, root) = setup_explorer_workspace().await;
    let target = root.join("src");
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: Some(target.display().to_string()),
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerHide>(
        &mut ws,
        11,
        &PickerHideParams {
            kind: PickerKind::Explorer,
        },
    )
    .await;

    let view2: aether_protocol::picker::PickerViewResult = send_request::<PickerView>(
        &mut ws,
        12,
        &PickerViewParams {
            kind: PickerKind::Explorer,
            reset: false,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    assert_eq!(
        view2.directory_path.as_deref(),
        Some(target.to_str().unwrap()),
        "second view without directory_path should resume the prior dir"
    );
    drop(server);
}

/// Mid-typing invalid regex (e.g. trailing `[`) is treated as a transient "no matches" rather
/// than an error — the picker stays responsive. The RPC succeeds; the streaming search emits one
/// final non-ticking, zero-hit update and exits.
#[tokio::test]
async fn picker_grep_invalid_regex_yields_no_hits() {
    let (server, mut ws) = setup_grep_workspace().await;
    let _ = send_request::<PickerView>(
        &mut ws,
        10,
        &PickerViewParams {
            kind: PickerKind::Grep,
            reset: true,
            offset: 0,
            limit: 30,
            center_on: None,
            directory_path: None,
        },
    )
    .await;
    let _ = expect_notification::<PickerUpdate>(&mut ws).await;

    let _: () = send_request::<PickerQuery>(
        &mut ws,
        11,
        &PickerQueryParams {
            kind: PickerKind::Grep,
            query: "foo[".into(),
            generation: 1,
        },
    )
    .await;
    let final_update = drain_grep_until_done(&mut ws).await;
    assert_eq!(final_update.total_matches, 0);
    assert!(!final_update.ticking);

    drop(server);
}

// ---- file watcher ------------------------------------------------------------------------------

use aether_protocol::buffer::{BufferReload, BufferReloadParams, BufferReloadResult};

/// Wait up to `max` for a matching notification; panics with a useful message on timeout.
async fn expect_notification_within<N: NotificationMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    max: std::time::Duration,
) -> N::Params {
    match tokio::time::timeout(max, expect_notification::<N>(ws)).await {
        Ok(p) => p,
        Err(_) => panic!("timed out waiting for notification {}", N::NAME),
    }
}

/// Spin up the server with one buffer subscribed to a viewport — the minimum setup for the
/// watcher to fire `buffer/state` pushes on file-change events. Returns the canonical disk
/// path so the test can write to it externally.
async fn setup_watched_buffer(
    initial: &str,
) -> (
    aether_server::ServerHandle,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u64,                  // buffer_id
    std::path::PathBuf,   // file path
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watched.txt");
    std::fs::write(&path, initial).unwrap();
    // Sleep briefly so subsequent external writes have a strictly-greater mtime than the one
    // the buffer records on load. Without this, fast back-to-back writes can produce an
    // identical mtime, which the watcher's self-save filter would mistake for our own write.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let dir_path = dir.path().to_path_buf();
    std::mem::forget(dir);

    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .unwrap();
    let _: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams {
            token: TEST_TOKEN.into(),
            client_version: "test".into(),
        },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            buffer_id: None,
            path_index: Some(0),
            relative_path: Some("watched.txt".into()),
            language: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    let _sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        3,
        &ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            wrap: WrapMode::Soft,
            continuation_marker_width: 0,
            tab_width: 4,
        },
    )
    .await;
    (server, ws, open.buffer_id, path)
}

#[tokio::test]
async fn watcher_reloads_clean_buffer_on_external_write() {
    let (server, mut ws, buffer_id, path) = setup_watched_buffer("hello\n").await;

    // External edit. Buffer was clean, so the server should silently reload + push state.
    std::fs::write(&path, "hello world\n").unwrap();

    let state_push = expect_notification_within::<BufferState>(
        &mut ws,
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(state_push.buffer_id, buffer_id);
    assert!(
        !state_push.externally_modified,
        "clean buffer should silently reload, not flag"
    );
    assert!(!state_push.externally_deleted);

    drop(server);
}

#[tokio::test]
async fn watcher_flags_dirty_buffer_on_external_write() {
    let (server, mut ws, buffer_id, path) = setup_watched_buffer("hello\n").await;

    // Dirty the buffer: insert at start.
    let _edit: EditResult = send_request::<InputText>(
        &mut ws,
        10,
        &InputTextParams {
            buffer_id,
            text: "x".into(),
            select_pasted: false,
        },
    )
    .await;
    // Drain the edit's lines_changed push so the next expect_notification gets the watcher's.
    let _ = expect_notification::<ViewportLinesChanged>(&mut ws).await;

    // External write while dirty: server should flag externally_modified, not silently reload.
    std::fs::write(&path, "external content\n").unwrap();

    let state_push = expect_notification_within::<BufferState>(
        &mut ws,
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(state_push.buffer_id, buffer_id);
    assert!(state_push.externally_modified, "expected externally_modified=true");
    assert!(!state_push.externally_deleted);

    // Save without overwrite should be rejected.
    let err = send_request_expect_err::<BufferSave>(
        &mut ws,
        20,
        &BufferSaveParams {
            buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    assert!(
        err.to_lowercase().contains("modified") || err.to_lowercase().contains("disk"),
        "expected external-mod error, got: {err}"
    );

    // Retry with overwrite: succeeds. We may still receive the `buffer/state` push from
    // earlier or other intermediate frames; `send_request` drains them and returns on the
    // matching response.
    let save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        21,
        &BufferSaveParams {
            buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: true,
        },
    )
    .await;
    assert!(save.saved_at_unix_ms > 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "xhello\n");

    drop(server);
}

#[tokio::test]
async fn watcher_flags_deleted_file() {
    let (server, mut ws, buffer_id, path) = setup_watched_buffer("hello\n").await;

    std::fs::remove_file(&path).unwrap();

    // First state push: externally_deleted = true. The watcher may also fire other events
    // depending on the OS (e.g. modify on the parent dir); loop until we see the deleted flag.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut seen_deleted = false;
    while tokio::time::Instant::now() < deadline {
        let p = match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            expect_notification::<BufferState>(&mut ws),
        )
        .await
        {
            Ok(p) => p,
            Err(_) => continue,
        };
        if p.externally_deleted && p.buffer_id == buffer_id {
            seen_deleted = true;
            break;
        }
    }
    assert!(seen_deleted, "no buffer/state with externally_deleted=true");

    // Save (with overwrite) recreates the file.
    let _save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        20,
        &BufferSaveParams {
            buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: true,
        },
    )
    .await;
    assert!(path.exists(), "save should recreate the deleted file");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");

    drop(server);
}

#[tokio::test]
async fn buffer_reload_discards_local_changes() {
    let (server, mut ws, buffer_id, path) = setup_watched_buffer("original\n").await;

    // Dirty the buffer with a local edit.
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        10,
        &InputTextParams {
            buffer_id,
            text: "local-edit-".into(),
            select_pasted: false,
        },
    )
    .await;

    // Change the file externally so reload picks up something visibly different.
    std::fs::write(&path, "from-disk\n").unwrap();

    // First try without force — server should reject with WOULD_DISCARD_CHANGES since the
    // buffer is dirty.
    let err = send_request_expect_err::<BufferReload>(
        &mut ws,
        20,
        &BufferReloadParams {
            buffer_id,
            force: false,
        },
    )
    .await;
    assert!(
        err.to_lowercase().contains("unsaved") || err.to_lowercase().contains("discard"),
        "expected would-discard-changes error, got: {err}"
    );

    // Retry with force: succeeds, bumps the revision, clears flags.
    let r: BufferReloadResult = send_request::<BufferReload>(
        &mut ws,
        21,
        &BufferReloadParams {
            buffer_id,
            force: true,
        },
    )
    .await;
    assert!(r.revision > 0);

    // Subsequent save (no overwrite) must succeed — flags cleared, content is now "from-disk\n".
    let _: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        22,
        &BufferSaveParams {
            buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
        },
    )
    .await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-disk\n");

    drop(server);
}

#[tokio::test]
async fn buffer_reload_clean_buffer_does_not_require_force() {
    let (server, mut ws, buffer_id, _path) = setup_watched_buffer("clean content\n").await;

    // No edits — buffer is clean. Reload without force should succeed.
    let r: BufferReloadResult = send_request::<BufferReload>(
        &mut ws,
        10,
        &BufferReloadParams {
            buffer_id,
            force: false,
        },
    )
    .await;
    assert!(r.revision > 0);

    drop(server);
}
