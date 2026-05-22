//! End-to-end test: spawn the server in-process, talk to it via WebSocket, exercise the
//! handshake and `buffer/open`.

use aether_protocol::buffer::{
    BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen,
    BufferOpenParams, BufferOpenResult, BufferSave, BufferSaveParams, BufferSaveResult,
    BufferState, BufferStateParams,
    CopyScope,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo,
    CursorUndoParams, CursorUndoResult, Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{
    ClientInbound, JsonRpc, Notification, NotificationMethod, Request, Response, RpcMethod,
};
use aether_protocol::handshake::{ClientHello, ClientHelloParams, ClientHelloResult};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNavResult, SearchNext, SearchPrev,
    SearchSet, SearchSetParams, SearchSetResult,
};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDedent, InputDelete, InputDeleteParams, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent, InputRedo,
    InputText, InputTextParams, InputToggleComment, InputUndo, UndoResult,
};
use aether_protocol::viewport::{
    ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams, ViewportScroll,
    ViewportScrollParams, ViewportSetWrap, ViewportSetWrapParams, ViewportSubscribe,
    ViewportSubscribeParams, ViewportSubscribeResult, ViewportWindowResult, WrapMode,
};
use aether_protocol::LogicalPosition;
use aether_server::spawn_for_test;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

const TEST_TOKEN: &str = "test-token-xyz";

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> String {
    loop {
        let msg = ws.next().await.expect("ws closed").expect("ws error");
        if let Message::Text(t) = msg {
            return t.to_string();
        }
    }
}

async fn send_request<M: RpcMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
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
            ClientInbound::Notification(_) | ClientInbound::Response(_) | ClientInbound::Error(_) => {
                // Skip unrelated frames; tests that care use `expect_notification` below.
            }
        }
    }
}

/// Read frames until one matching notification arrives. Panics if the stream ends first.
async fn expect_notification<N: NotificationMethod>(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
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

    let (mut ws, _resp) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();

    // Handshake.
    let hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    assert_eq!(hello.project.name, "test-proj");
    assert_eq!(hello.project.paths.len(), 1);

    // Open the file.
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("hello.rs".into()),
            language: None,
            create_if_missing: false,
        },
    )
    .await;
    assert!(open.buffer_id > 0);
    assert_eq!(open.language.as_deref(), Some("rust"));
    assert_eq!(open.saved_revision, open.revision);
    assert_eq!(open.revision, 0);
    assert!(open.line_count >= 3);
    assert!(open.byte_count > 0);

    // Re-opening returns the same buffer id (deduping by canonical path).
    let open2: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        3,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("hello.rs".into()),
            language: None,
            create_if_missing: false,
        },
    )
    .await;
    assert_eq!(open2.buffer_id, open.buffer_id);

    drop(server);
}

#[tokio::test]
async fn rejects_bad_token() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();

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
    ws.send(Message::text(serde_json::to_string(&req).unwrap())).await.unwrap();

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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();

    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;

    // Try to open by providing relative_path that escapes upward.
    let req = Request {
        jsonrpc: JsonRpc,
        id: 2,
        method: BufferOpen::NAME.into(),
        params: Some(
            serde_json::to_value(BufferOpenParams {
                path_index: Some(0),
                relative_path: Some("../aether-outside-test.txt".into()),
                language: None,
                create_if_missing: false,
            })
            .unwrap(),
        ),
    };
    ws.send(Message::text(serde_json::to_string(&req).unwrap())).await.unwrap();

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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();

    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;

    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            language: None,
            create_if_missing: false,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("long.txt".into()),
            language: None,
            create_if_missing: false,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
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
        .map(|r| r.segments.iter().map(|s| s.text.as_str()).collect::<String>())
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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("many.txt".into()),
            language: None,
            create_if_missing: false,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    assert_eq!(sub.window.first_logical_line, 0);

    let scrolled: ViewportWindowResult = send_request::<ViewportScroll>(
        &mut ws,
        4,
        &ViewportScrollParams {
            viewport_id: sub.viewport_id,
            scroll: ScrollPosition { logical_line: 20, sub_row: 0.0 },
        },
    )
    .await;
    assert_eq!(scrolled.window.first_logical_line, 18); // 20 - overscan(2)
    assert!(scrolled.window.last_logical_line_exclusive >= 25);
    let first_text = &scrolled.window.lines[2].visual_rows[0].segments[0].text;
    assert_eq!(first_text, "line 20");
}

// -------- cursor + input ------------------------------------------------------------------------

async fn setup_with_buffer(content: &str) -> (
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

    let server = spawn_for_test("test-proj", vec![dir_path], TEST_TOKEN).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams { path_index: Some(0), relative_path: Some("buf.txt".into()), language: None, create_if_missing: false },
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
            motion: Motion::Char { direction: Direction::Forward, count: 3 },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });
    assert!(st.anchor.is_none());

    // Moving forward past the end of line should land on the next line.
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Forward, count: 5 },
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
            anchor: None,
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
            motion: Motion::Char { direction: Direction::Forward, count: 3 },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 9 });
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 6 }));

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
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });

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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
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
            anchor: None,
        },
    )
    .await;

    let result: EditResult =
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "XY".into(), select_pasted: false }).await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams = expect_notification::<ViewportLinesChanged>(&mut ws).await;
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
            anchor: None,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    let _ = sub;

    let result: EditResult = send_request::<InputDelete>(
        &mut ws,
        12,
        &InputDeleteParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Backward, count: 1 },
        },
    )
    .await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams = expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "hell");

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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams { path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::None,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;

    let line0 = &sub.window.lines[0];
    let segs = &line0.visual_rows[0].segments;
    let highlights = &segs[0].highlights;
    assert!(!highlights.is_empty(), "expected highlight spans on a Rust line");

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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    // Park on the `{` (col 9 on line 0).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 9 },
        anchor: None,
    }).await;
    let r: CursorState = send_request::<CursorMove>(&mut ws, 4, &CursorMoveParams {
        buffer_id: open.buffer_id,
        motion: Motion::MatchBracket,
        extend_selection: false,
    }).await;
    // `}` lives at col 22 on the same line.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 22 });
    assert!(r.anchor.is_none());
    // match_bracket is populated; positions are the same pair regardless of orientation.
    let pair = r.match_bracket.expect("match_bracket should be populated");
    assert!(pair == (LogicalPosition { line: 0, col: 9 }, LogicalPosition { line: 0, col: 22 })
         || pair == (LogicalPosition { line: 0, col: 22 }, LogicalPosition { line: 0, col: 9 }));

    drop(server);
}

#[tokio::test]
async fn match_bracket_with_extend_selects_to_pair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 9 },
        anchor: None,
    }).await;
    let r: CursorState = send_request::<CursorMove>(&mut ws, 4, &CursorMoveParams {
        buffer_id: open.buffer_id,
        motion: Motion::MatchBracket,
        extend_selection: true,
    }).await;
    // Cursor lands on the `}`; anchor pinned at the original `{`. Together they cover the
    // whole `{...}` pair inclusive — that's the "select around brackets" gesture.
    assert_eq!(r.position, LogicalPosition { line: 0, col: 22 });
    assert_eq!(r.anchor, Some(LogicalPosition { line: 0, col: 9 }));

    drop(server);
}

#[tokio::test]
async fn match_bracket_from_inside_pair_jumps_to_opener() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn foo() { let x = 1; }\n").unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    // Cursor on the `l` of `let` — inside the block, not on any bracket.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 11 },
        anchor: None,
    }).await;
    let r: CursorState = send_request::<CursorMove>(&mut ws, 4, &CursorMoveParams {
        buffer_id: open.buffer_id,
        motion: Motion::MatchBracket,
        extend_selection: false,
    }).await;
    // Cursor jumps to the opening `{` (we pick the opener when cursor is between brackets).
    assert_eq!(r.position, LogicalPosition { line: 0, col: 9 });

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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams {
            path_index: Some(0),
            relative_path: Some("notes.md".into()),
            language: None,
            create_if_missing: false,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::None,
            continuation_marker_width: 0, tab_width: 4,
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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams { path_index: Some(0), relative_path: Some("greet.txt".into()), language: None, create_if_missing: false },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;

    // Edit: append "!" at end. Move cursor to end then insert.
    let _ = send_request::<CursorMove>(
        &mut ws,
        4,
        &CursorMoveParams { buffer_id: open.buffer_id, motion: Motion::BufferEnd, extend_selection: false },
    )
    .await;
    // BufferEnd puts cursor on the trailing empty line; move it to end of first line instead.
    send_request::<aether_protocol::cursor::CursorSet>(
        &mut ws,
        5,
        &aether_protocol::cursor::CursorSetParams {
            buffer_id: open.buffer_id,
            position: LogicalPosition { line: 0, col: 5 },
            anchor: None,
        },
    )
    .await;
    let _edit: EditResult = send_request::<InputText>(
        &mut ws,
        6,
        &InputTextParams { buffer_id: open.buffer_id, text: "!".into(), select_pasted: false },
    )
    .await;
    // Drain the viewport/lines_changed pushed by the edit so it doesn't leak into the next test step.
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    let save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        7,
        &BufferSaveParams { buffer_id: open.buffer_id, path_index: None, relative_path: None },
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
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams { path_index: Some(0), relative_path: Some("windows.txt".into()), language: None, create_if_missing: false },
    )
    .await;

    // Save without changes — line endings should round-trip as CRLF.
    let _save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        3,
        &BufferSaveParams { buffer_id: open.buffer_id, path_index: None, relative_path: None },
    )
    .await;
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.windows(2).any(|w| w == b"\r\n"), "expected CRLF after save, got {bytes:?}");
    assert!(!bytes.windows(2).any(|w| w[0] != b'\r' && w[1] == b'\n'),
        "expected no bare LF after save");

    drop(server);
}

#[tokio::test]
async fn save_scratch_returns_buffer_has_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()], TEST_TOKEN)
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(
        &mut ws,
        1,
        &ClientHelloParams { token: TEST_TOKEN.into(), client_version: "test".into() },
    )
    .await;
    let open: BufferOpenResult = send_request::<BufferOpen>(
        &mut ws,
        2,
        &BufferOpenParams { path_index: None, relative_path: None, language: None, create_if_missing: false },
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
            })
            .unwrap(),
        ),
    };
    ws.send(Message::text(serde_json::to_string(&req).unwrap())).await.unwrap();
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
        &CursorSetParams { buffer_id, position: LogicalPosition { line: 0, col: 6 }, anchor: None },
    )
    .await;
    let _: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Forward, count: 3 },
            extend_selection: true,
        },
    )
    .await;
    let r: BufferCopyResult = send_request::<BufferCopy>(
        &mut ws,
        12,
        &BufferCopyParams { buffer_id, scope: CopyScope::Selection },
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
        &CursorSetParams { buffer_id, position: LogicalPosition { line: 1, col: 2 }, anchor: None },
    )
    .await;
    let r: BufferCopyResult = send_request::<BufferCopy>(
        &mut ws,
        11,
        &BufferCopyParams { buffer_id, scope: CopyScope::Line },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    send_request::<CursorSet>(
        &mut ws,
        11,
        &CursorSetParams { buffer_id, position: LogicalPosition { line: 0, col: 6 }, anchor: None },
    )
    .await;
    let _: CursorState = send_request::<CursorMove>(
        &mut ws,
        12,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Forward, count: 3 },
            extend_selection: true,
        },
    )
    .await;
    let r: BufferCutResult = send_request::<BufferCut>(
        &mut ws,
        13,
        &BufferCopyParams { buffer_id, scope: CopyScope::Selection },
    )
    .await;
    assert_eq!(r.text, "beta");
    // dirty is now derived client-side from revision vs saved_revision; just confirm the
    // revision advanced.
    assert!(r.revision > 0);
    let notif = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "alpha  gamma");
    drop(server);
}

#[tokio::test]
async fn input_text_with_select_pasted_makes_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\n").await;
    send_request::<CursorSet>(
        &mut ws,
        10,
        &CursorSetParams { buffer_id, position: LogicalPosition { line: 0, col: 0 }, anchor: None },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    let edit: EditResult = send_request::<InputText>(
        &mut ws,
        12,
        &InputTextParams { buffer_id, text: "XYZ".into(), select_pasted: true },
    )
    .await;
    // Anchor at col 0 ('X'), position at col 2 (block on 'Z') — selection covers "XYZ".
    assert_eq!(edit.cursor.anchor, Some(LogicalPosition { line: 0, col: 0 }));
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
            anchor: None,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    let edit: EditResult =
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "XY".into(), select_pasted: false }).await;
    assert!(edit.revision > 0);
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Undo: should revert "XY", cursor back to col 3, and (since saved_revision is 0) the
    // revision drops to 0 — client derives `dirty == false` from that.
    let undo: UndoResult = send_request::<InputUndo>(&mut ws, 13, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert_eq!(undo.cursor.position, LogicalPosition { line: 0, col: 3 });
    assert_eq!(undo.revision, 0, "undo back to saved revision");
    let notif = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "abc");

    // Redo: re-applies "XY", revision advances past saved.
    let redo: UndoResult = send_request::<InputRedo>(&mut ws, 14, &BufferOnlyParams { buffer_id }).await;
    assert!(redo.applied);
    assert!(redo.revision > 0);
    let notif = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "abcXY");

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
            anchor: None,
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    // Edit #1: insert "X"
    let _e1: EditResult =
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "X".into(), select_pasted: false }).await;
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Save.
    let save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        13,
        &BufferSaveParams { buffer_id, path_index: None, relative_path: None },
    )
    .await;
    let saved_state = expect_notification::<BufferState>(&mut ws).await;
    assert_eq!(saved_state.saved_revision, save.revision);

    // Edit #2: delete (different kind, so a new group). Backspace removes the "X".
    let _e2: EditResult = send_request::<InputDelete>(
        &mut ws,
        14,
        &InputDeleteParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Backward, count: 1 },
        },
    )
    .await;
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Undo: should put "X" back, taking us back to the saved revision → derived dirty == false.
    let undo: UndoResult = send_request::<InputUndo>(&mut ws, 15, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert_eq!(undo.revision, save.revision, "undo should return to the saved revision");
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
            motion: Motion::Word { direction: Direction::Forward, count: 1, boundary: WordBoundary::Word, exclusive: false },
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
            motion: Motion::Word { direction: Direction::Forward, count: 1, boundary: WordBoundary::Word, exclusive: false },
            extend_selection: false,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 11 });

    // `Alt-w` (WORD): from col 0, skip "hello" → " " then to "world-foo" (col 6)
    send_request::<CursorSet>(&mut ws, 12, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 0, col: 0 },
        anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        13,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Word { direction: Direction::Forward, count: 1, boundary: WordBoundary::BigWord, exclusive: false },
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
            motion: Motion::Word { direction: Direction::Forward, count: 1, boundary: WordBoundary::BigWord, exclusive: false },
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
            motion: Motion::Word { direction: Direction::Backward, count: 1, boundary: WordBoundary::Word, exclusive: false },
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
            motion: Motion::WordEnd { direction: Direction::Forward, count: 1, boundary: WordBoundary::Word },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputJoinLines>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    assert!(r.revision > 0);
    let notif = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
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
            anchor: None,
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
            motion: Motion::Char { direction: Direction::Forward, count: 3 },
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
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,

            continuation_marker_width: 0, tab_width: 4,
        },
    )
    .await;
    let _ = sub;

    let result: EditResult = send_request::<InputText>(
        &mut ws,
        13,
        &InputTextParams { buffer_id, text: "DELTA".into(), select_pasted: false },
    )
    .await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams = expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "alpha DELTA gamma");

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
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 2 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Now at end-of-line: advances to next line.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 12, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 2, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_picks_previous_then_stays_at_end() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Mid-line: selects previous line.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 2, col: 2 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // At end-of-line: stays on the current line (selecting it).
    send_request::<CursorSet>(&mut ws, 12, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 2, col: 5 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 13, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 2, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_walks_up_via_anchor_on_repeat() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // Start at end of "delta" — first press picks current line.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 3, col: 5 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 3, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    // Second press: walks up via anchor-at-col-0 → line 2.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 12, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 2, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    // Third press: → line 1.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 13, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: false,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    drop(server);
}

#[tokio::test]
async fn select_line_forward_extend_walks_cursor_down() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x: line 0.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 2 }, anchor: None,
    }).await;
    send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;

    // Shift-x: lines 0–1.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 12, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: true,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 1, col: 4 });

    // Shift-x again: lines 0–2.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 13, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: true,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_backward_extend_walks_anchor_up() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x: line 3.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 3, col: 2 }, anchor: None,
    }).await;
    send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;

    // Shift-Alt-x: lines 2–3.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 12, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: true,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 2, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    // Shift-Alt-x again: lines 1–3.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 13, &CursorSelectLineParams {
        buffer_id, direction: Direction::Backward, extend: true,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 3, col: 5 });

    drop(server);
}

#[tokio::test]
async fn select_line_after_swap_preserves_backward_orientation() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // x at start of line 0, then swap — backward selection of line 0.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 0 }, anchor: None,
    }).await;
    send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;
    let st: CursorState = send_request::<CursorSwapAnchor>(&mut ws, 12, &CursorSwapAnchorParams {
        buffer_id,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 5 }));

    // Shift-x grows the *bottom* edge down (anchor moves), cursor stays at top.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 13, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: true,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 0 });
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 4 }));

    drop(server);
}

#[tokio::test]
async fn select_line_snaps_partial_selection_to_whole_lines() {
    let (server, mut ws, buffer_id) = setup_lines().await;

    // A partial, non-line-aligned selection (e.g. left over from Shift-arrow motion).
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 2, col: 3 },
        anchor: Some(LogicalPosition { line: 0, col: 2 }),
    }).await;

    // Shift-x snaps both ends to whole-line boundaries: anchor → col 0, cursor → line end.
    let st: CursorState = send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: true,
    }).await;
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 0 }));
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

// ---- cursor/swap_anchor ------------------------------------------------------------------------

#[tokio::test]
async fn swap_anchor_swaps_position_and_anchor() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 1, col: 3 },
        anchor: Some(LogicalPosition { line: 0, col: 1 }),
    }).await;

    let st: CursorState = send_request::<CursorSwapAnchor>(&mut ws, 11, &CursorSwapAnchorParams {
        buffer_id,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 1 });
    assert_eq!(st.anchor, Some(LogicalPosition { line: 1, col: 3 }));

    drop(server);
}

#[tokio::test]
async fn swap_anchor_with_no_selection_is_noop() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;

    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 3 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorSwapAnchor>(&mut ws, 11, &CursorSwapAnchorParams {
        buffer_id,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });
    assert_eq!(st.anchor, None);

    drop(server);
}

// ---- Motion::Word { exclusive: true } -----------------------------------------------------------

#[tokio::test]
async fn word_motion_exclusive_progresses_across_boundaries() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world foo\n").await;

    // From 'h' (col 0), exclusive forward Word — lands on space before "world".
    let st: CursorState = send_request::<CursorMove>(&mut ws, 10, &CursorMoveParams {
        buffer_id,
        motion: Motion::Word {
            direction: Direction::Forward,
            count: 1,
            boundary: WordBoundary::Word,
            exclusive: true,
        },
        extend_selection: true,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });
    assert_eq!(st.anchor, Some(LogicalPosition { line: 0, col: 0 }));

    // Repeated press from the space — pre-advance kicks in so we skip "world" entirely and
    // land on the space before "foo" (col 11), rather than getting stuck.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 11, &CursorMoveParams {
        buffer_id,
        motion: Motion::Word {
            direction: Direction::Forward,
            count: 1,
            boundary: WordBoundary::Word,
            exclusive: true,
        },
        extend_selection: true,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 11 });

    drop(server);
}

// ---- cursor/undo and cursor/redo --------------------------------------------------------------

#[tokio::test]
async fn motion_undo_restores_previous_cursor() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;

    // Two cursor moves: (0,0) → (1,2) → (2,3).
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 2 }, anchor: None,
    }).await;
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 2, col: 3 }, anchor: None,
    }).await;

    // Undo: back to (1,2).
    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 12, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });

    // Undo again: back to the initial (0, 0).
    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_then_redo_round_trips() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 3 }, anchor: None,
    }).await;

    // Undo → back to (0, 0).
    send_request::<CursorUndo>(&mut ws, 11, &CursorUndoParams { buffer_id }).await;

    // Redo → forward to (1, 3).
    let r: CursorUndoResult = send_request::<CursorRedo>(&mut ws, 12, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 3 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_returns_not_applied_when_stack_empty() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;

    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 10, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(!r.applied);
    // Cursor unchanged.
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });

    drop(server);
}

#[tokio::test]
async fn motion_undo_stack_cleared_by_mutation() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    // Build up some motion history.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 2 }, anchor: None,
    }).await;
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 4 }, anchor: None,
    }).await;

    // Mutation clears the motion stack.
    send_request::<InputText>(&mut ws, 12, &InputTextParams {
        buffer_id, text: "X".into(), select_pasted: false,
    }).await;

    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(!r.applied, "motion stack should be empty after a mutation");

    drop(server);
}

#[tokio::test]
async fn motion_redo_cleared_by_new_motion() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 3 }, anchor: None,
    }).await;
    // Undo populates redo.
    send_request::<CursorUndo>(&mut ws, 11, &CursorUndoParams { buffer_id }).await;
    // New motion should clear the redo stack.
    send_request::<CursorSet>(&mut ws, 12, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 2 }, anchor: None,
    }).await;

    let r: CursorUndoResult = send_request::<CursorRedo>(&mut ws, 13, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(!r.applied, "redo stack should be empty after a fresh motion");

    drop(server);
}

#[tokio::test]
async fn motion_undo_records_select_line_and_swap() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;

    // Position at line 1 mid.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 2 }, anchor: None,
    }).await;
    // x → selects line 1.
    send_request::<CursorSelectLine>(&mut ws, 11, &CursorSelectLineParams {
        buffer_id, direction: Direction::Forward, extend: false,
    }).await;
    // s → swap.
    let after_swap: CursorState = send_request::<CursorSwapAnchor>(&mut ws, 12, &CursorSwapAnchorParams {
        buffer_id,
    }).await;
    assert_eq!(after_swap.position, LogicalPosition { line: 1, col: 0 });

    // Undo the swap.
    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 13, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 4 });
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 1, col: 0 }));

    // Undo the select_line.
    let r: CursorUndoResult = send_request::<CursorUndo>(&mut ws, 14, &CursorUndoParams {
        buffer_id,
    }).await;
    assert!(r.applied);
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });
    assert_eq!(r.cursor.anchor, None);

    drop(server);
}

#[tokio::test]
async fn word_motion_exclusive_at_buffer_end_does_not_move_past() {
    let (server, mut ws, buffer_id) = setup_with_buffer("hello").await;

    // Cursor on last char.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 4 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(&mut ws, 11, &CursorMoveParams {
        buffer_id,
        motion: Motion::Word {
            direction: Direction::Forward,
            count: 1,
            boundary: WordBoundary::Word,
            exclusive: true,
        },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 4 });

    drop(server);
}

// ---- Motion::VisualLine -----------------------------------------------------------------------

#[tokio::test]
async fn visual_line_down_walks_wrapped_rows_within_a_logical_line() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    // Subscribe with WrapMode::Soft at width 10 so the line wraps to ["the quick", "brown fox"].
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,

        continuation_marker_width: 0, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    // Cursor at start of line — visual col 0 of row 0. Down should land on row 1's col 0 (byte 10).
    let st: CursorState = send_request::<CursorMove>(&mut ws, 11, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 10 });

    drop(server);
}

#[tokio::test]
async fn visual_line_preserves_visual_column() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,

        continuation_marker_width: 0, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    // Put cursor at byte 5 (visual col 5 of row 0). Down should land at byte 10+5=15 in row 1.
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 5 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 15 });

    // Up: back to visual col 5 of row 0 = byte 5.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 13, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Up, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 5 });

    drop(server);
}

#[tokio::test]
async fn visual_line_crosses_logical_line_boundary() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc\ndef\n").await;
    // Width is large enough that no line wraps.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 20, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,

        continuation_marker_width: 0, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    // Cursor at (0, 1). Down → (1, 1).
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 1 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
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
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 80, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 3 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 5 });

    drop(server);
}

#[tokio::test]
async fn visual_line_with_wrap_none_falls_back_to_logical() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\nhi\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::None,

        continuation_marker_width: 0, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    // Cursor at (0, 5). With wrap=None, Down → logical line + 1, col clamped to line 1's length.
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 5 }, anchor: None,
    }).await;
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 2 }); // line 1 = "hi", len 2

    drop(server);
}

// ---- viewport/set_wrap ------------------------------------------------------------------------

#[tokio::test]
async fn viewport_set_wrap_changes_visible_rows() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,

        continuation_marker_width: 0, tab_width: 4,
    }).await;
    // Soft: line 0 wraps to 2 visual rows at cols=10.
    assert_eq!(sub.window.lines[0].visual_rows.len(), 2);

    let r: ViewportWindowResult = send_request::<ViewportSetWrap>(&mut ws, 11, &ViewportSetWrapParams {
        viewport_id: sub.viewport_id,
        wrap: WrapMode::None,
    }).await;
    // None: one row, full line content.
    assert_eq!(r.window.lines[0].visual_rows.len(), 1);
    assert_eq!(r.window.lines[0].visual_rows[0].segments[0].text, "the quick brown fox");

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
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    // Start at byte 1 (visual col 1 on row 0, prefix 0).
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 1 }, anchor: None,
    }).await;

    // Alt-j: visual col 1 < prefix 2 on row 1, so cursor clamps to start of row 1's text (byte 10).
    // The remembered virtual col stays at 1.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 10 });

    // Alt-k: with virtual_col=1, target visual col is 1. On row 0 (prefix 0), byte = 1. We end
    // back where we started, not at byte 2 (which is what naive preserve-col would do).
    let st: CursorState = send_request::<CursorMove>(&mut ws, 13, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Up, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 1 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_preserved_across_empty_line_for_logical_motion() {
    // The classic vim virtual-col case: j down through an empty line should land you back at
    // your original column on the next non-empty line, not stick at col 0.
    let (server, mut ws, buffer_id) = setup_with_buffer("hello world\n\nanother line\n").await;
    let _: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 80, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;

    // Start at col 5 of line 0.
    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 5 }, anchor: None,
    }).await;

    // j → empty line 1; col clamps to 0 but virtual_col holds 5.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::LogicalLine { direction: Direction::Forward, count: 1, preserve_col: true },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 1, col: 0 });

    // j → line 2 with content; virtual_col restores col 5.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 13, &CursorMoveParams {
        buffer_id,
        motion: Motion::LogicalLine { direction: Direction::Forward, count: 1, preserve_col: true },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 2, col: 5 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_cleared_by_horizontal_motion() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefghijklmnopqrst\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 1 }, anchor: None,
    }).await;
    send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    // Cursor now at byte 10 (visual col 2 = prefix); virtual_col stashed = 1.

    // Char Forward (a horizontal motion) clears the virtual col. Cursor at byte 11, visual col 3.
    send_request::<CursorMove>(&mut ws, 13, &CursorMoveParams {
        buffer_id,
        motion: Motion::Char { direction: Direction::Forward, count: 1 },
        extend_selection: false,
    }).await;

    // Alt-k: without a virtual col, target is current visual col (3). Lands at byte 3 of row 0.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 14, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Up, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });

    drop(server);
}

#[tokio::test]
async fn virtual_col_cleared_by_mutation() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abcdefghijklmnopqrst\n").await;
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;
    let viewport_id = sub.viewport_id;

    send_request::<CursorSet>(&mut ws, 11, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 1 }, anchor: None,
    }).await;
    send_request::<CursorMove>(&mut ws, 12, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Down, count: 1 },
        extend_selection: false,
    }).await;
    // virtual_col = 1, cursor at byte 10.

    // Insert "X" — the mutation clears the virtual col. Cursor advances to byte 11.
    send_request::<InputText>(&mut ws, 13, &InputTextParams {
        buffer_id, text: "X".into(), select_pasted: false,
    }).await;

    // Alt-k: target is current visual col (3, since cursor is on row 1 with prefix 2 at col 1
    // within the text). Lands at byte 3, not the original byte 1.
    let st: CursorState = send_request::<CursorMove>(&mut ws, 14, &CursorMoveParams {
        buffer_id,
        motion: Motion::VisualLine { viewport_id, direction: VerticalDirection::Up, count: 1 },
        extend_selection: false,
    }).await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 3 });

    drop(server);
}

#[tokio::test]
async fn continuation_marker_width_reduces_continuation_row_width() {
    let (server, mut ws, buffer_id) = setup_with_buffer("the quick brown fox\n").await;
    // With marker_width=2 the continuation rows have 8 cols of content room, so the line wraps
    // into 3 visual rows instead of 2.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(&mut ws, 10, &ViewportSubscribeParams {
        buffer_id, cols: 10, rows: 5, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::Soft,
        continuation_marker_width: 2, tab_width: 4,
    }).await;
    assert_eq!(sub.window.lines[0].visual_rows.len(), 3);
    let texts: Vec<&str> = sub.window.lines[0].visual_rows.iter()
        .map(|r| r.segments[0].text.as_str())
        .collect();
    assert_eq!(texts, vec!["the quick", "brown", "fox"]);

    drop(server);
}

// ---- input/move_lines ---------------------------------------------------------------------------

async fn buffer_text(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, id: u64, buffer_id: u64) -> String {
    // Subscribe to a wide-enough viewport and concatenate the visible-text lines.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(ws, id, &ViewportSubscribeParams {
        buffer_id, cols: 200, rows: 100, overscan_rows: 0,
        scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
        wrap: WrapMode::None,
        continuation_marker_width: 0, tab_width: 4,
    }).await;
    sub.window.lines.iter()
        .map(|l| l.visual_rows[0].segments[0].text.as_str().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn move_lines_swaps_with_neighbor_below() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\ngamma\n").await;
    // Cursor on line 0 ("alpha").
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 2 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 11, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Down,
    }).await;
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
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 1 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 11, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Up,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 1 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "beta\nalpha\ngamma\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_moves_whole_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("a\nb\nc\nd\ne\n").await;
    // Selection covers lines 1 and 2 ("b" and "c").
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 2, col: 0 },
        anchor: Some(LogicalPosition { line: 1, col: 0 }),
    }).await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 11, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Down,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 3, col: 0 });
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 2, col: 0 }));
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "a\nd\nb\nc\ne\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_at_top_is_noop_up() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 10, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Up,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });
    let text = buffer_text(&mut ws, 11, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn move_lines_at_bottom_is_noop_down() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 0 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 11, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Down,
    }).await;
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
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 1, col: 0 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputMoveLines>(&mut ws, 11, &InputMoveLinesParams {
        buffer_id, direction: VerticalDirection::Up,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 0 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "beta\nalpha");

    drop(server);
}

// ---- input/indent and input/dedent --------------------------------------------------------------

#[tokio::test]
async fn indent_single_line_adds_two_spaces() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 3 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputIndent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
    // Cursor follows the inserted indent.
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 5 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "  alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn dedent_strips_two_spaces() {
    let (server, mut ws, buffer_id) = setup_with_buffer("  alpha\nbeta\n").await;
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 4 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputDedent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 2 });
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn indent_multi_line_selection() {
    let (server, mut ws, buffer_id) = setup_with_buffer("a\nb\nc\n").await;
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 2, col: 0 },
        anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    let r: EditResult = send_request::<InputIndent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
    // Anchor and cursor both shift +2 since both lines were indented.
    assert_eq!(r.cursor.position, LogicalPosition { line: 2, col: 2 });
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 2 }));
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "  a\n  b\n  c\n");

    drop(server);
}

#[tokio::test]
async fn dedent_line_without_indent_is_noop_for_that_line() {
    let (server, mut ws, buffer_id) = setup_with_buffer("  alpha\nbeta\n").await;
    // Multi-line selection covering both lines.
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id,
        position: LogicalPosition { line: 1, col: 1 },
        anchor: Some(LogicalPosition { line: 0, col: 4 }),
    }).await;
    let r: EditResult = send_request::<InputDedent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
    // Line 0 lost 2 chars, line 1 unchanged.
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 1 });
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 2 }));
    let text = buffer_text(&mut ws, 12, buffer_id).await;
    assert_eq!(text, "alpha\nbeta\n");

    drop(server);
}

#[tokio::test]
async fn dedent_with_single_leading_space_strips_one() {
    let (server, mut ws, buffer_id) = setup_with_buffer(" alpha\n").await;
    let r: EditResult = send_request::<InputDedent>(&mut ws, 10, &BufferOnlyParams {
        buffer_id,
    }).await;
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
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 7 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;
    let buffer_id = open.buffer_id;

    // Cursor right after the opening brace.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 10 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;
    let buffer_id = open.buffer_id;

    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 9 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 0 });
    let text = buffer_text(&mut ws, 5, buffer_id).await;
    assert_eq!(text, "// note {\n\n");

    drop(server);
}

#[tokio::test]
async fn newline_and_indent_on_empty_line_inserts_just_newline() {
    let (server, mut ws, buffer_id) = setup_with_buffer("\n").await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 10, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;
    let buffer_id = open.buffer_id;

    // Park cursor just past the closing `}` on line 2.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 2, col: 1 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.py".into()), language: None, create_if_missing: false,
    }).await;
    assert_eq!(open.language.as_deref(), Some("python"));
    let buffer_id = open.buffer_id;

    // Cursor at end of `def foo():` (line 0 col 10).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 10 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;
    let buffer_id = open.buffer_id;

    // Cursor at end of `let y = 2;` (line 2) — engine returns 1 level, unit is 2 spaces.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 2, col: 12 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.go".into()), language: None, create_if_missing: false,
    }).await;
    assert_eq!(open.language.as_deref(), Some("go"));
    let buffer_id = open.buffer_id;

    send_request::<InputText>(&mut ws, 3, &InputTextParams {
        buffer_id, text: "func foo() {".into(), select_pasted: false,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 4, &BufferOnlyParams {
        buffer_id,
    }).await;
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
    send_request::<CursorSet>(&mut ws, 10, &CursorSetParams {
        buffer_id, position: LogicalPosition { line: 0, col: 9 }, anchor: None,
    }).await;
    let r: EditResult = send_request::<InputNewlineAndIndent>(&mut ws, 11, &BufferOnlyParams {
        buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    // Cursor on `let` (col 4, after the 4-space indent).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id, position: LogicalPosition { line: 0, col: 4 }, anchor: None,
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id, position: LogicalPosition { line: 0, col: 0 }, anchor: None,
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.py".into()), language: None, create_if_missing: false,
    }).await;
    assert_eq!(open.language.as_deref(), Some("python"));

    // Selection covers all three lines.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 2, col: 0 },
        anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.md".into()), language: None, create_if_missing: false,
    }).await;

    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id, position: LogicalPosition { line: 0, col: 0 }, anchor: None,
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 12 },
        anchor: Some(LogicalPosition { line: 0, col: 10 }),
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Select `/* foo */` (cols 10..=18 inclusive).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 18 },
        anchor: Some(LogicalPosition { line: 0, col: 10 }),
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.rs".into()), language: None, create_if_missing: false,
    }).await;

    // Last char of `let c = 3;` is `;` at col 9.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 2, col: 9 },
        anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    // Anchor stays at line 0 col 0 (now on the `/` of `// let a = 1;`).
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 0 }));
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 12 },
        anchor: Some(LogicalPosition { line: 0, col: 10 }),
    }).await;
    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    // Selection now covers the entire `/* foo */` — anchor on the first `/`, cursor on the
    // last `/`. The wrap is 9 chars (`/* foo */`), so cols 10..=18.
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 10 }));
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.go".into()), language: None, create_if_missing: false,
    }).await;

    // Selection from (0, 5) mid-line through (0, 10) — the newline. Single line in
    // (line, col), but selected text includes `\n`.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 10 },
        anchor: Some(LogicalPosition { line: 0, col: 5 }),
    }).await;
    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    let text = buffer_text(&mut ws, 5, open.buffer_id).await;
    // The closing `*/` sits on line 1 (after the original `\n`).
    assert_eq!(text, "let a/*  = 1;\n */let b = 2;\n");
    // Anchor stays on the original start; cursor follows the `*/` onto line 1 at col 2.
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 5 }));
    assert_eq!(r.cursor.position, LogicalPosition { line: 1, col: 2 });

    // Toggle again to uncomment. Round-trip must restore the original buffer *and* the
    // original selection — cursor back on the `\n` at line 0 col 10, not on line 1 col 0.
    let r2: EditResult = send_request::<InputToggleComment>(&mut ws, 6, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    let text2 = buffer_text(&mut ws, 7, open.buffer_id).await;
    assert_eq!(text2, "let a = 1;\nlet b = 2;\n");
    assert_eq!(r2.cursor.anchor, Some(LogicalPosition { line: 0, col: 5 }));
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.ts".into()), language: None, create_if_missing: false,
    }).await;

    // Multi-line partial selection: (0, 4) `a` through (1, 4) `b`.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 1, col: 4 },
        anchor: Some(LogicalPosition { line: 0, col: 4 }),
    }).await;
    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    // Anchor stays at (0, 4) — the opening `/` of `/*` lives there post-edit. Cursor lands
    // on the last `/` of `*/`, which is at col 7 of line 1 (`let b */ = 2;`).
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 4 }));
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Select from col 4 of line 0 (the `a`) to col 4 of line 1 (the `b`) — multi-line but
    // neither line is fully covered.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 1, col: 4 },
        anchor: Some(LogicalPosition { line: 0, col: 4 }),
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Select `foo` (cols 10..=12 inclusive).
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id,
        position: LogicalPosition { line: 0, col: 12 },
        anchor: Some(LogicalPosition { line: 0, col: 10 }),
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
    let after_wrap = buffer_text(&mut ws, 5, open.buffer_id).await;
    assert_eq!(after_wrap, "const x = /* foo */ + bar;\n");

    // Second toggle: the response from the first toggle moved the selection to the inner
    // `foo`. We don't manually re-set the cursor — just press toggle again.
    send_request::<InputToggleComment>(&mut ws, 6, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.js".into()), language: None, create_if_missing: false,
    }).await;

    // Cursor on the `f` of `foo` (col 13), inside the comment.
    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id, position: LogicalPosition { line: 0, col: 13 }, anchor: None,
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.css".into()), language: None, create_if_missing: false,
    }).await;

    send_request::<CursorSet>(&mut ws, 3, &CursorSetParams {
        buffer_id: open.buffer_id, position: LogicalPosition { line: 0, col: 0 }, anchor: None,
    }).await;
    send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.md".into()), language: None, create_if_missing: false,
    }).await;

    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
        .await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(server.ws_url()).await.unwrap();
    let _hello: ClientHelloResult = send_request::<ClientHello>(&mut ws, 1, &ClientHelloParams {
        token: TEST_TOKEN.into(), client_version: "test".into(),
    }).await;
    let open: BufferOpenResult = send_request::<BufferOpen>(&mut ws, 2, &BufferOpenParams {
        path_index: Some(0), relative_path: Some("a.json".into()), language: None, create_if_missing: false,
    }).await;

    let r: EditResult = send_request::<InputToggleComment>(&mut ws, 4, &BufferOnlyParams {
        buffer_id: open.buffer_id,
    }).await;
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
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id,
        query: "foo".into(),
        anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    assert_eq!(r.summary.total, 3);
    assert!(!r.summary.truncated);
    assert_eq!(r.summary.current_index, 1);
    assert_eq!(r.cursor.position, LogicalPosition { line: 0, col: 2 });
    assert_eq!(r.cursor.anchor, Some(LogicalPosition { line: 0, col: 0 }));

    drop(server);
}

#[tokio::test]
async fn search_smartcase_lowercase_is_case_insensitive() {
    let (server, mut ws, buffer_id) = setup_with_buffer("Foo foo FOO\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "foo".into(), anchor: None,
    }).await;
    assert_eq!(r.summary.total, 3); // matches all three regardless of case

    drop(server);
}

#[tokio::test]
async fn search_smartcase_uppercase_is_case_sensitive() {
    let (server, mut ws, buffer_id) = setup_with_buffer("Foo foo FOO\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "Foo".into(), anchor: None,
    }).await;
    assert_eq!(r.summary.total, 1);

    drop(server);
}

#[tokio::test]
async fn search_regex_metacharacters() {
    let (server, mut ws, buffer_id) = setup_with_buffer("abc 123 def 4567\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: r"\d+".into(), anchor: None,
    }).await;
    assert_eq!(r.summary.total, 2);

    drop(server);
}

#[tokio::test]
async fn search_no_matches_returns_zero_summary() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\nbeta\n").await;
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "zzz".into(), anchor: None,
    }).await;
    assert_eq!(r.summary.total, 0);
    assert_eq!(r.summary.current_index, 0);
    assert!(!r.summary.truncated);

    drop(server);
}

#[tokio::test]
async fn search_empty_query_clears_active_search() {
    let (server, mut ws, buffer_id) = setup_with_buffer("alpha\n").await;
    let _: SearchSetResult = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "alpha".into(), anchor: None,
    }).await;
    let r: SearchSetResult = send_request::<SearchSet>(&mut ws, 11, &SearchSetParams {
        buffer_id, query: String::new(), anchor: None,
    }).await;
    assert_eq!(r.summary.total, 0);

    drop(server);
}

#[tokio::test]
async fn search_next_cycles_forward_and_wraps() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo bar foo baz\nfoo qux\n").await;
    let _ = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "foo".into(), anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    let r1: SearchNavResult = send_request::<SearchNext>(&mut ws, 11, &SearchNavParams { buffer_id }).await;
    assert_eq!(r1.summary.current_index, 2);
    assert_eq!(r1.cursor.anchor, Some(LogicalPosition { line: 0, col: 8 }));
    let r2: SearchNavResult = send_request::<SearchNext>(&mut ws, 12, &SearchNavParams { buffer_id }).await;
    assert_eq!(r2.summary.current_index, 3);
    // Wrap.
    let r3: SearchNavResult = send_request::<SearchNext>(&mut ws, 13, &SearchNavParams { buffer_id }).await;
    assert_eq!(r3.summary.current_index, 1);

    drop(server);
}

#[tokio::test]
async fn search_prev_cycles_backward_with_wrap() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo bar foo baz\nfoo qux\n").await;
    let _ = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "foo".into(), anchor: Some(LogicalPosition { line: 0, col: 0 }),
    }).await;
    // From the first match, prev wraps to the last.
    let r: SearchNavResult = send_request::<SearchPrev>(&mut ws, 11, &SearchNavParams { buffer_id }).await;
    assert_eq!(r.summary.current_index, 3);

    drop(server);
}

#[tokio::test]
async fn search_clear_removes_active_search() {
    let (server, mut ws, buffer_id) = setup_with_buffer("foo\n").await;
    let _ = send_request::<SearchSet>(&mut ws, 10, &SearchSetParams {
        buffer_id, query: "foo".into(), anchor: None,
    }).await;
    let _: () = send_request::<SearchClear>(&mut ws, 11, &SearchClearParams { buffer_id }).await;
    // After clear, n/prev should report no matches.
    let r: SearchNavResult = send_request::<SearchNext>(&mut ws, 12, &SearchNavParams { buffer_id }).await;
    assert_eq!(r.summary.total, 0);

    drop(server);
}
