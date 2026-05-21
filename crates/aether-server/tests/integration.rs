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
    CursorUndoParams, CursorUndoResult, Direction, Motion, WordBoundary,
};
use aether_protocol::envelope::{
    ClientInbound, JsonRpc, Notification, NotificationMethod, Request, Response, RpcMethod,
};
use aether_protocol::handshake::{ClientHello, ClientHelloParams, ClientHelloResult};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDelete, InputDeleteParams, InputJoinLines, InputRedo,
    InputText, InputTextParams, InputUndo, UndoResult,
};
use aether_protocol::viewport::{
    ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams, ViewportScroll,
    ViewportScrollParams, ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult,
    ViewportWindowResult, WrapMode,
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
        },
    )
    .await;
    assert!(open.buffer_id > 0);
    assert_eq!(open.language.as_deref(), Some("rust"));
    assert_eq!(open.dirty, false);
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
        &BufferOpenParams { path_index: Some(0), relative_path: Some("buf.txt".into()), language: None },
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
        &BufferOpenParams { path_index: Some(0), relative_path: Some("a.rs".into()), language: None },
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
        &BufferOpenParams { path_index: Some(0), relative_path: Some("greet.txt".into()), language: None },
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

    // The server pushes buffer/state to clear dirty.
    let state_push: BufferStateParams = expect_notification::<BufferState>(&mut ws).await;
    assert_eq!(state_push.dirty, false);
    assert_eq!(state_push.buffer_id, open.buffer_id);

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
        &BufferOpenParams { path_index: Some(0), relative_path: Some("windows.txt".into()), language: None },
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
        &BufferOpenParams { path_index: None, relative_path: None, language: None },
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
    assert!(r.dirty);
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
        },
    )
    .await;
    let edit: EditResult =
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "XY".into(), select_pasted: false }).await;
    assert!(edit.dirty);
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Undo: should revert "XY", cursor back to col 3, and dirty cleared (we're back at saved rev).
    let undo: UndoResult = send_request::<InputUndo>(&mut ws, 13, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert_eq!(undo.cursor.position, LogicalPosition { line: 0, col: 3 });
    assert!(!undo.dirty, "undo back to saved should clear dirty");
    let notif = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "abc");

    // Redo: re-applies "XY", dirty true again.
    let redo: UndoResult = send_request::<InputRedo>(&mut ws, 14, &BufferOnlyParams { buffer_id }).await;
    assert!(redo.applied);
    assert!(redo.dirty);
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
    assert!(!r.dirty);
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
        },
    )
    .await;
    // Edit #1: insert "X"
    let _e1: EditResult =
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "X".into(), select_pasted: false }).await;
    let _ = expect_notification::<aether_protocol::viewport::ViewportLinesChanged>(&mut ws).await;

    // Save.
    let _save: BufferSaveResult = send_request::<BufferSave>(
        &mut ws,
        13,
        &BufferSaveParams { buffer_id, path_index: None, relative_path: None },
    )
    .await;
    let _ = expect_notification::<BufferState>(&mut ws).await;

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

    // Undo: should put "X" back, taking us back to the saved revision → dirty cleared.
    let undo: UndoResult = send_request::<InputUndo>(&mut ws, 15, &BufferOnlyParams { buffer_id }).await;
    assert!(undo.applied);
    assert!(!undo.dirty, "undo back to saved revision should clear dirty");
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
        },
    )
    .await;
    let r: EditResult =
        send_request::<InputJoinLines>(&mut ws, 11, &BufferOnlyParams { buffer_id }).await;
    assert!(r.dirty);
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
