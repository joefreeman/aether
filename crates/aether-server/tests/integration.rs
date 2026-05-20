//! End-to-end test: spawn the server in-process, talk to it via WebSocket, exercise the
//! handshake and `buffer/open`.

use aether_protocol::buffer::{
    BufferOpen, BufferOpenParams, BufferOpenResult, BufferSave, BufferSaveParams,
    BufferSaveResult, BufferState, BufferStateParams,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorSet, CursorSetParams, CursorState, Direction, Motion,
};
use aether_protocol::envelope::{
    ClientInbound, JsonRpc, Notification, NotificationMethod, Request, Response, RpcMethod,
};
use aether_protocol::handshake::{ClientHello, ClientHelloParams, ClientHelloResult};
use aether_protocol::input::{
    EditResult, InputDelete, InputDeleteParams, InputText, InputTextParams,
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

    // Extend selection 4 chars right (cover "beta").
    let st: CursorState = send_request::<CursorMove>(
        &mut ws,
        11,
        &CursorMoveParams {
            buffer_id,
            motion: Motion::Char { direction: Direction::Forward, count: 4 },
            extend_selection: true,
        },
    )
    .await;
    assert_eq!(st.position, LogicalPosition { line: 0, col: 10 });
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
        send_request::<InputText>(&mut ws, 12, &InputTextParams { buffer_id, text: "XY".into() }).await;
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
        &InputTextParams { buffer_id: open.buffer_id, text: "!".into() },
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
            motion: Motion::Char { direction: Direction::Forward, count: 4 },
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
        &InputTextParams { buffer_id, text: "DELTA".into() },
    )
    .await;
    assert_eq!(result.revision, 1);

    let notif: ViewportLinesChangedParams = expect_notification::<ViewportLinesChanged>(&mut ws).await;
    assert_eq!(notif.replacement_lines[0].visual_rows[0].segments[0].text, "alpha DELTA gamma");

    drop(server);
}
