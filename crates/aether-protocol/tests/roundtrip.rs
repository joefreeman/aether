//! Wire-format round-trip tests. These exist to catch serde-derive surprises (untagged enums,
//! internally-tagged enums, optional fields) and to lock in the JSON shape against the protocol
//! doc.

use aether_protocol::buffer::{BufferOpen, BufferOpenParams, BufferOpenResult};
use aether_protocol::cursor::{CursorMove, CursorMoveParams, Direction, Motion, WordBoundary};
use aether_protocol::envelope::{
    ClientInbound, ErrorObject, ErrorResponse, JsonRpc, Notification, NotificationMethod, Request,
    RpcMethod,
};
use aether_protocol::handshake::{ClientHello, ClientHelloParams, ProjectInfo};
use aether_protocol::input::{InputText, InputTextParams};
use aether_protocol::viewport::ViewportLinesChanged;
use aether_protocol::LogicalPosition;
use serde_json::{from_str, from_value, json, to_value};

#[test]
fn jsonrpc_marker_rejects_non_20() {
    let bad = json!({"jsonrpc": "1.0", "id": 1, "method": "x", "params": null});
    assert!(from_value::<Request>(bad).is_err());
}

#[test]
fn request_roundtrip() {
    let req = Request {
        jsonrpc: JsonRpc,
        id: 7,
        method: ClientHello::NAME.into(),
        params: Some(
            to_value(ClientHelloParams {
                token: "abc".into(),
                client_version: "0.1.0".into(),
            })
            .unwrap(),
        ),
    };
    let s = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "client/hello");
    assert_eq!(v["params"]["token"], "abc");
}

#[test]
fn client_inbound_discriminates() {
    let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {"x": 1}});
    let err = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32010, "message": "bad path"}});
    let notif = json!({"jsonrpc": "2.0", "method": "buffer/state", "params": {}});

    assert!(matches!(from_value::<ClientInbound>(resp).unwrap(), ClientInbound::Response(_)));
    assert!(matches!(from_value::<ClientInbound>(err).unwrap(), ClientInbound::Error(_)));
    assert!(matches!(
        from_value::<ClientInbound>(notif).unwrap(),
        ClientInbound::Notification(_)
    ));
}

#[test]
fn motion_is_internally_tagged() {
    let m = Motion::Char { direction: Direction::Backward, count: 1 };
    let v = to_value(&m).unwrap();
    assert_eq!(v, json!({"kind": "char", "direction": "backward", "count": 1}));

    let m = Motion::LineStart;
    let v = to_value(&m).unwrap();
    assert_eq!(v, json!({"kind": "line_start"}));

    let m = Motion::Word {
        direction: Direction::Forward,
        count: 2,
        boundary: WordBoundary::BigWord,
        exclusive: false,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "word", "direction": "forward", "count": 2, "boundary": "WORD", "exclusive": false})
    );

    let m = Motion::Goto { position: LogicalPosition { line: 17, col: 4 } };
    let v = to_value(&m).unwrap();
    assert_eq!(v, json!({"kind": "goto", "position": {"line": 17, "col": 4}}));
}

#[test]
fn cursor_move_params_use_motion() {
    let v = to_value(CursorMoveParams {
        buffer_id: 42,
        motion: Motion::Char { direction: Direction::Forward, count: 1 },
        extend_selection: true,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "buffer_id": 42,
            "motion": {"kind": "char", "direction": "forward", "count": 1},
            "extend_selection": true,
        })
    );
}

#[test]
fn input_text_params() {
    let v = to_value(InputTextParams { buffer_id: 1, text: "hi".into(), select_pasted: false }).unwrap();
    assert_eq!(v, json!({"buffer_id": 1, "text": "hi", "select_pasted": false}));
}

#[test]
fn buffer_open_result_shape() {
    let v = to_value(BufferOpenResult {
        buffer_id: 42,
        language: Some("rust".into()),
        line_count: 100,
        byte_count: 1234,
        revision: 0,
        dirty: false,
    })
    .unwrap();
    assert_eq!(v["buffer_id"], 42);
    assert_eq!(v["language"], "rust");
    assert_eq!(v["dirty"], false);
}

#[test]
fn error_response_shape() {
    let er = ErrorResponse {
        jsonrpc: JsonRpc,
        id: 3,
        error: ErrorObject {
            code: -32010,
            message: "path outside project".into(),
            data: None,
        },
    };
    let v = to_value(&er).unwrap();
    assert_eq!(v["error"]["code"], -32010);
    assert!(v["error"].get("data").is_none(), "data: None should be skipped");
}

#[test]
fn method_name_constants() {
    assert_eq!(ClientHello::NAME, "client/hello");
    assert_eq!(BufferOpen::NAME, "buffer/open");
    assert_eq!(CursorMove::NAME, "cursor/move");
    assert_eq!(InputText::NAME, "input/text");
    assert_eq!(ViewportLinesChanged::NAME, "viewport/lines_changed");
}

#[test]
fn notification_roundtrip() {
    let n = Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: json!({"viewport_id": 1, "revision": 5, "range": {}, "replacement_lines": []}),
    };
    let s = serde_json::to_string(&n).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["method"], "viewport/lines_changed");
    assert!(v.get("id").is_none(), "notifications carry no id");
}

#[test]
fn project_info_shape() {
    let p = ProjectInfo { name: "aether".into(), paths: vec!["/home/joe/x".into()] };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "aether", "paths": ["/home/joe/x"]}));
}

#[test]
fn buffer_open_scratch_form() {
    // Both path_index and relative_path null => scratch buffer per §6.1.
    let v = to_value(BufferOpenParams {
        path_index: None,
        relative_path: None,
        language: Some("rust".into()),
    })
    .unwrap();
    assert_eq!(v["path_index"], serde_json::Value::Null);
    assert_eq!(v["relative_path"], serde_json::Value::Null);
}

#[test]
fn unit_result_round_trips() {
    // BufferClose and ViewportUnsubscribe have Result = (). The JSON unit value is `null`.
    let unit: () = ();
    let s = serde_json::to_string(&unit).unwrap();
    assert_eq!(s, "null");
    let _: () = serde_json::from_str(&s).unwrap();
}
