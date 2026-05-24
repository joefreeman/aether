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
        saved_revision: 0,
        path: None,
        cursor: Default::default(),
        scroll: None,
    })
    .unwrap();
    assert_eq!(v["buffer_id"], 42);
    assert_eq!(v["language"], "rust");
    assert_eq!(v["saved_revision"], 0);
    // Cursor always serialises (CursorState::default() is `{position: {line:0,col:0}, anchor: null}`).
    assert_eq!(v["cursor"]["position"]["line"], 0);
    assert_eq!(v["cursor"]["position"]["col"], 0);
    // `scroll: None` skips serialisation — keeps the wire shape tight for first-open cases.
    assert!(v.get("scroll").is_none(), "scroll: None should be skipped");
}

#[test]
fn buffer_open_result_restored_scroll() {
    use aether_protocol::viewport::ScrollPosition;
    let v = to_value(BufferOpenResult {
        buffer_id: 42,
        language: None,
        line_count: 1,
        byte_count: 0,
        revision: 0,
        saved_revision: 0,
        path: None,
        cursor: Default::default(),
        scroll: Some(ScrollPosition { logical_line: 7, sub_row: 0.5 }),
    })
    .unwrap();
    assert_eq!(v["scroll"]["logical_line"], 7);
    assert_eq!(v["scroll"]["sub_row"], 0.5);
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
        buffer_id: None,
        path_index: None,
        relative_path: None,
        language: Some("rust".into()),
        create_if_missing: false,
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

// ---- picker ------------------------------------------------------------------------------------

#[test]
fn picker_kind_serializes_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Files).unwrap(), json!("files"));
    assert_eq!(
        from_value::<PickerKind>(json!("files")).unwrap(),
        PickerKind::Files,
    );
}

#[test]
fn picker_item_file_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::File { path: "src/main.rs".into(), match_indices: vec![0, 4] };
    let v = to_value(&item).unwrap();
    assert_eq!(v, json!({"kind": "file", "path": "src/main.rs", "match_indices": [0, 4]}));
}

#[test]
fn picker_view_params_omit_center_on_when_none() {
    use aether_protocol::picker::{PickerKind, PickerViewParams};
    let p = PickerViewParams {
        kind: PickerKind::Files,
        reset: true,
        offset: 0,
        limit: 30,
        center_on: None,
    };
    let v = to_value(&p).unwrap();
    assert!(v.get("center_on").is_none(), "None center_on should be skipped");
    assert_eq!(v["kind"], "files");
    assert_eq!(v["reset"], true);
}

#[test]
fn picker_view_params_center_on_serialized() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerViewParams};
    let p = PickerViewParams {
        kind: PickerKind::Files,
        reset: false,
        offset: 0,
        limit: 30,
        center_on: Some(PickerItem::File { path: "x".into(), match_indices: vec![] }),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["center_on"]["kind"], "file");
    assert_eq!(v["center_on"]["path"], "x");
}

#[test]
fn picker_update_round_trips_through_notification() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdate, PickerUpdateParams};
    let params = PickerUpdateParams {
        kind: PickerKind::Files,
        generation: 7,
        offset: 0,
        items: vec![PickerItem::File { path: "a".into(), match_indices: vec![0] }],
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
    };
    let notif = Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: to_value(&params).unwrap(),
    };
    let s = serde_json::to_string(&notif).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["method"], "picker/update");
    assert_eq!(v["params"]["generation"], 7);
    assert_eq!(v["params"]["items"][0]["path"], "a");
}

#[test]
fn picker_select_result_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::File { path: "/abs/path".into() };
    assert_eq!(to_value(&r).unwrap(), json!({"kind": "file", "path": "/abs/path"}));
}

#[test]
fn picker_item_buffer_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::Buffer {
        buffer_id: 7,
        display: "src/main.rs".into(),
        dirty: true,
        match_indices: vec![0, 4],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "buffer",
            "buffer_id": 7,
            "display": "src/main.rs",
            "dirty": true,
            "match_indices": [0, 4],
        })
    );
}

#[test]
fn picker_select_result_buffer_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::Buffer { buffer_id: 42 };
    assert_eq!(to_value(&r).unwrap(), json!({"kind": "buffer", "buffer_id": 42}));
}

#[test]
fn picker_kind_buffers_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Buffers).unwrap(), json!("buffers"));
}

#[test]
fn buffer_open_params_buffer_id_skipped_when_none() {
    use aether_protocol::buffer::BufferOpenParams;
    let p = BufferOpenParams {
        buffer_id: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
    };
    let v = to_value(&p).unwrap();
    assert!(v.get("buffer_id").is_none());
    assert_eq!(v["path_index"], 0);
}

#[test]
fn buffer_open_params_buffer_id_round_trips() {
    use aether_protocol::buffer::BufferOpenParams;
    let p = BufferOpenParams {
        buffer_id: Some(11),
        path_index: None,
        relative_path: None,
        language: None,
        create_if_missing: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["buffer_id"], 11);
}
