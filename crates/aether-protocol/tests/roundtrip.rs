//! Wire-format round-trip tests. These exist to catch serde-derive surprises (untagged enums,
//! internally-tagged enums, optional fields) and to lock in the JSON shape against the protocol
//! doc.

use aether_protocol::buffer::{BufferOpen, BufferOpenParams, BufferOpenResult};
use aether_protocol::cursor::{CursorMove, CursorMoveParams, Direction, Motion, WordBoundary};
use aether_protocol::directory::{
    DirectoryCreate, DirectoryCreateParams, DirectoryCreateResult, DirectoryEntry, DirectoryList,
    DirectoryListParams, DirectoryListResult,
};
use aether_protocol::envelope::{
    ClientInbound, ErrorObject, ErrorResponse, JsonRpc, Notification, NotificationMethod, Request,
    RpcMethod,
};
use aether_protocol::git::{
    BlameInfo, GitBlameLine, GitBlameLineParams, GitBlameLineResult, GitNavigateHunk,
    GitNavigateHunkParams, HunkDirection, GitSetDiffView, GitSetDiffViewParams,
};
use aether_protocol::viewport::{DiffMarker, LogicalLineRender, VirtualRow, VirtualRowKind};
use aether_protocol::input::{InputSurround, InputSurroundParams, InputText, InputTextParams};
use aether_protocol::search::{SearchSet, SearchSetParams};
use aether_protocol::project::{
    ProjectActivate, ProjectActivateParams, ProjectInfo, ProjectList, ProjectSummary,
};
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
        method: ProjectActivate::NAME.into(),
        params: Some(to_value(ProjectActivateParams { name: "aether".into() }).unwrap()),
    };
    let s = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "project/activate");
    assert_eq!(v["params"]["name"], "aether");
}

#[test]
fn client_inbound_discriminates() {
    let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {"x": 1}});
    let err = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32010, "message": "bad path"}});
    let notif = json!({"jsonrpc": "2.0", "method": "buffer/state", "params": {}});

    assert!(matches!(
        from_value::<ClientInbound>(resp).unwrap(),
        ClientInbound::Response(_)
    ));
    assert!(matches!(
        from_value::<ClientInbound>(err).unwrap(),
        ClientInbound::Error(_)
    ));
    assert!(matches!(
        from_value::<ClientInbound>(notif).unwrap(),
        ClientInbound::Notification(_)
    ));
}

#[test]
fn git_blame_line_params_shape() {
    let p = GitBlameLineParams {
        buffer_id: 3,
        line: 41,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"buffer_id": 3, "line": 41}));
    assert_eq!(GitBlameLine::NAME, "git/blame_line");
}

#[test]
fn git_blame_line_result_roundtrip() {
    // A committed line and an uncommitted line both round-trip; `None` is the no-blame case.
    let committed = GitBlameLineResult {
        blame: Some(BlameInfo {
            commit: "a1b2c3d".into(),
            author: "Ada".into(),
            timestamp: 1_700_000_000,
            summary: "Wire up blame".into(),
            is_uncommitted: false,
        }),
    };
    let v = to_value(&committed).unwrap();
    assert_eq!(v["blame"]["commit"], "a1b2c3d");
    assert_eq!(v["blame"]["author"], "Ada");
    assert_eq!(v["blame"]["timestamp"], 1_700_000_000_i64);
    assert_eq!(v["blame"]["is_uncommitted"], false);
    let back: GitBlameLineResult = from_value(v).unwrap();
    assert_eq!(back.blame.unwrap().summary, "Wire up blame");

    let none = GitBlameLineResult { blame: None };
    assert_eq!(to_value(&none).unwrap(), json!({"blame": null}));
}

#[test]
fn git_set_diff_view_params_shape() {
    let p = GitSetDiffViewParams {
        viewport_id: 9,
        enabled: true,
    };
    assert_eq!(to_value(&p).unwrap(), json!({"viewport_id": 9, "enabled": true}));
    assert_eq!(GitSetDiffView::NAME, "git/set_diff_view");
}

#[test]
fn logical_line_render_virtual_rows_shape() {
    // `virtual_rows_above` is omitted when empty (back-compat) and uses snake_case kinds.
    let bare = LogicalLineRender {
        logical_line: 0,
        visual_rows: vec![],
        search_matches: vec![],
        virtual_rows_above: vec![],
        diff_marker: None,
    };
    let v = to_value(&bare).unwrap();
    assert!(v.get("virtual_rows_above").is_none(), "empty omitted from wire");
    assert!(v.get("diff_marker").is_none(), "None marker omitted from wire");

    let with_del = LogicalLineRender {
        logical_line: 4,
        visual_rows: vec![],
        search_matches: vec![],
        virtual_rows_above: vec![VirtualRow {
            text: "old line".into(),
            kind: VirtualRowKind::Deleted,
        }],
        diff_marker: Some(DiffMarker::Modified),
    };
    let v = to_value(&with_del).unwrap();
    assert_eq!(v["virtual_rows_above"][0]["text"], "old line");
    assert_eq!(v["virtual_rows_above"][0]["kind"], "deleted");
    assert_eq!(v["diff_marker"], "modified");
    let back: LogicalLineRender = from_value(v).unwrap();
    assert_eq!(back.virtual_rows_above.len(), 1);
    assert_eq!(back.virtual_rows_above[0].kind, VirtualRowKind::Deleted);
    assert_eq!(back.diff_marker, Some(DiffMarker::Modified));
}

#[test]
fn git_navigate_hunk_shapes() {
    let p = GitNavigateHunkParams {
        buffer_id: 2,
        from_line: 10,
        direction: HunkDirection::Next,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"buffer_id": 2, "from_line": 10, "direction": "next"}));
    assert_eq!(GitNavigateHunk::NAME, "git/navigate_hunk");
}

#[test]
fn motion_is_internally_tagged() {
    let m = Motion::Char {
        direction: Direction::Backward,
        count: 1,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "char", "direction": "backward", "count": 1})
    );

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

    let m = Motion::Goto {
        position: LogicalPosition { line: 17, col: 4 },
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "goto", "position": {"line": 17, "col": 4}})
    );
}

#[test]
fn cursor_move_params_use_motion() {
    let v = to_value(CursorMoveParams {
        buffer_id: 42,
        motion: Motion::Char {
            direction: Direction::Forward,
            count: 1,
        },
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
fn search_set_params() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(SearchSet::NAME, "search/set");

    // Full shape: anchor present, extend set.
    let v = to_value(SearchSetParams {
        buffer_id: 3,
        query: "foo".into(),
        anchor: Some(LogicalPosition { line: 2, col: 5 }),
        extend: true,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "buffer_id": 3,
            "query": "foo",
            "anchor": {"line": 2, "col": 5},
            "extend": true,
        })
    );

    // `extend` defaults to false when omitted on the wire (back-compat with older clients).
    let p: SearchSetParams =
        from_value(json!({"buffer_id": 3, "query": "foo", "anchor": null})).unwrap();
    assert!(!p.extend);
    assert!(p.anchor.is_none());
}

#[test]
fn input_text_params() {
    let v = to_value(InputTextParams {
        buffer_id: 1,
        text: "hi".into(),
        select_pasted: false,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 1, "text": "hi", "select_pasted": false})
    );
}

#[test]
fn input_surround_params() {
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::input::SurroundTarget;
    assert_eq!(InputSurround::NAME, "input/surround");

    // `delimiter` is a char — serialises as a one-char JSON string; `target` is snake_case.
    let v = to_value(InputSurroundParams {
        buffer_id: 7,
        delimiter: '(',
        target: SurroundTarget::Line,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 7, "delimiter": "(", "target": "line"})
    );

    // Round-trips back to the same values.
    let back: InputSurroundParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.buffer_id, 7);
    assert_eq!(back.delimiter, '(');
    assert_eq!(back.target, SurroundTarget::Line);

    // `target` defaults to Selection when omitted on the wire.
    let defaulted: InputSurroundParams =
        serde_json::from_value(json!({"buffer_id": 1, "delimiter": "{"})).unwrap();
    assert_eq!(defaulted.target, SurroundTarget::Selection);
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
        scratch_number: Some(3),
        cursor: Default::default(),
        scroll: None,
    })
    .unwrap();
    assert_eq!(v["buffer_id"], 42);
    assert_eq!(v["language"], "rust");
    assert_eq!(v["saved_revision"], 0);
    assert_eq!(v["scratch_number"], 3);
    // Cursor always serialises (CursorState::default() is `{position: {line:0,col:0}, anchor: {line:0,col:0}}`).
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
        scratch_number: None,
        cursor: Default::default(),
        scroll: Some(ScrollPosition {
            logical_line: 7,
            sub_row: 0.5,
        }),
    })
    .unwrap();
    assert_eq!(v["scroll"]["logical_line"], 7);
    assert_eq!(v["scroll"]["sub_row"], 0.5);
    // `scratch_number: None` skips serialisation, like a file buffer.
    assert!(v.get("scratch_number").is_none());
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
    assert!(
        v["error"].get("data").is_none(),
        "data: None should be skipped"
    );
}

#[test]
fn method_name_constants() {
    assert_eq!(ProjectList::NAME, "project/list");
    assert_eq!(ProjectActivate::NAME, "project/activate");
    assert_eq!(BufferOpen::NAME, "buffer/open");
    assert_eq!(CursorMove::NAME, "cursor/move");
    assert_eq!(InputText::NAME, "input/text");
    assert_eq!(ViewportLinesChanged::NAME, "viewport/lines_changed");
    assert_eq!(DirectoryList::NAME, "directory/list");
}

#[test]
fn directory_list_params_shape() {
    let v = to_value(DirectoryListParams {
        path: "/home/foo/proj/src".into(),
    })
    .unwrap();
    assert_eq!(v, json!({ "path": "/home/foo/proj/src" }));
}

#[test]
fn directory_list_result_shape() {
    let v = to_value(DirectoryListResult {
        path: "/home/foo/proj/src".into(),
        parent: Some("/home/foo/proj".into()),
        entries: vec![
            DirectoryEntry {
                name: "lib".into(),
                is_dir: true,
            },
            DirectoryEntry {
                name: "main.rs".into(),
                is_dir: false,
            },
        ],
    })
    .unwrap();
    assert_eq!(v["path"], "/home/foo/proj/src");
    assert_eq!(v["parent"], "/home/foo/proj");
    assert_eq!(v["entries"][0], json!({"name": "lib", "is_dir": true}));
    assert_eq!(v["entries"][1], json!({"name": "main.rs", "is_dir": false}));
}

#[test]
fn directory_create_method_name_and_shape() {
    assert_eq!(DirectoryCreate::NAME, "directory/create");
    let params = to_value(DirectoryCreateParams {
        path: "/proj/newdir".into(),
    })
    .unwrap();
    assert_eq!(params, json!({ "path": "/proj/newdir" }));
    let result = to_value(DirectoryCreateResult {
        path: "/proj/newdir".into(),
    })
    .unwrap();
    assert_eq!(result, json!({ "path": "/proj/newdir" }));
}

#[test]
fn directory_list_result_skips_none_parent() {
    let v = to_value(DirectoryListResult {
        path: "/proj".into(),
        parent: None,
        entries: Vec::new(),
    })
    .unwrap();
    assert!(
        v.get("parent").is_none(),
        "parent: None should be skipped on the wire"
    );
    assert_eq!(v["entries"], json!([]));
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
    let p = ProjectInfo {
        name: "aether".into(),
        paths: vec!["/home/joe/x".into()],
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "aether", "paths": ["/home/joe/x"]}));
}

#[test]
fn project_list_result_shape() {
    use aether_protocol::project::ProjectListResult;
    let r = ProjectListResult {
        projects: vec![
            ProjectSummary { name: "a".into() },
            ProjectSummary { name: "b".into() },
        ],
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v, json!({"projects": [{"name": "a"}, {"name": "b"}]}));
}

#[test]
fn project_activate_result_wraps_info() {
    use aether_protocol::project::ProjectActivateResult;
    let r = ProjectActivateResult {
        project: ProjectInfo {
            name: "aether".into(),
            paths: vec!["/p".into()],
        },
        last_buffer_id: None,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["project"]["name"], "aether");
    assert_eq!(v["project"]["paths"][0], "/p");
    assert!(
        v.get("last_buffer_id").is_none(),
        "None last_buffer_id should be skipped"
    );
}

#[test]
fn project_create_params_round_trip() {
    use aether_protocol::project::{ProjectCreate, ProjectCreateParams};
    assert_eq!(ProjectCreate::NAME, "project/create");
    let p = ProjectCreateParams {
        name: "newproj".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "newproj"}));
}

#[test]
fn project_add_root_params_round_trip() {
    use aether_protocol::project::{ProjectAddRoot, ProjectAddRootParams};
    assert_eq!(ProjectAddRoot::NAME, "project/add_root");
    let p = ProjectAddRootParams {
        project: "aether".into(),
        path: "~/src/aether".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"project": "aether", "path": "~/src/aether"}));
}

#[test]
fn project_rename_params_round_trip() {
    use aether_protocol::project::{ProjectRename, ProjectRenameParams};
    assert_eq!(ProjectRename::NAME, "project/rename");
    let p = ProjectRenameParams {
        project: "aether".into(),
        new_name: "aether-next".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"project": "aether", "new_name": "aether-next"}));
    // Result is a plain ProjectInfo (new name + paths).
    let info = ProjectInfo {
        name: "aether-next".into(),
        paths: vec!["/p".into()],
    };
    assert_eq!(to_value(&info).unwrap()["name"], "aether-next");
}

#[test]
fn project_delete_params_round_trip() {
    use aether_protocol::project::{ProjectDelete, ProjectDeleteParams};
    assert_eq!(ProjectDelete::NAME, "project/delete");
    let p = ProjectDeleteParams {
        name: "aether".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "aether"}));
}

#[test]
fn picker_grep_file_jump_round_trips() {
    use aether_protocol::cursor::Direction;
    use aether_protocol::picker::{PickerGrepFileJump, PickerGrepFileJumpParams};
    assert_eq!(PickerGrepFileJump::NAME, "picker/grep_file_jump");
    let p = PickerGrepFileJumpParams {
        from_index: 7,
        direction: Direction::Backward,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["from_index"], 7);
    let back: PickerGrepFileJumpParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.from_index, 7);
    assert!(matches!(back.direction, Direction::Backward));
}

#[test]
fn path_delete_round_trips() {
    use aether_protocol::path::{PathDelete, PathDeleteParams, PathDeleteResult};
    assert_eq!(PathDelete::NAME, "path/delete");
    let p = PathDeleteParams {
        path: "/ws/src/foo.rs".into(),
    };
    assert_eq!(to_value(&p).unwrap(), json!({"path": "/ws/src/foo.rs"}));

    let full = PathDeleteResult {
        closed_buffer_ids: vec![3, 7],
        next_buffer_id: Some(9),
    };
    let v = to_value(&full).unwrap();
    assert_eq!(v["closed_buffer_ids"], json!([3, 7]));
    assert_eq!(v["next_buffer_id"], 9);

    // `next_buffer_id` is omitted when there's nothing to attach to.
    let none = PathDeleteResult {
        closed_buffer_ids: vec![],
        next_buffer_id: None,
    };
    assert_eq!(to_value(&none).unwrap().get("next_buffer_id"), None);
}

#[test]
fn project_remove_root_result_shape() {
    use aether_protocol::project::{ProjectRemoveRoot, ProjectRemoveRootResult};
    assert_eq!(ProjectRemoveRoot::NAME, "project/remove_root");
    let r = ProjectRemoveRootResult {
        project: ProjectInfo {
            name: "aether".into(),
            paths: vec!["/p".into()],
        },
        closed_buffer_ids: vec![3, 5],
        next_buffer_id: Some(7),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["project"]["name"], "aether");
    assert_eq!(v["closed_buffer_ids"], json!([3, 5]));
    assert_eq!(v["next_buffer_id"], 7);
}

#[test]
fn project_remove_root_result_skips_none_next_buffer() {
    use aether_protocol::project::ProjectRemoveRootResult;
    let r = ProjectRemoveRootResult {
        project: ProjectInfo {
            name: "aether".into(),
            paths: vec![],
        },
        closed_buffer_ids: vec![],
        next_buffer_id: None,
    };
    let v = to_value(&r).unwrap();
    assert!(v.get("next_buffer_id").is_none());
}

#[test]
fn project_activate_result_includes_last_buffer_id_when_set() {
    use aether_protocol::project::ProjectActivateResult;
    let r = ProjectActivateResult {
        project: ProjectInfo {
            name: "aether".into(),
            paths: vec!["/p".into()],
        },
        last_buffer_id: Some(7),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["last_buffer_id"], 7);
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
        jump_to: None,
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
    let item = PickerItem::File {
        path_index: 0,
        relative_path: "src/main.rs".into(),
        match_indices: vec![0, 4],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "file",
            "path_index": 0,
            "relative_path": "src/main.rs",
            "match_indices": [0, 4],
        })
    );
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
        center_on_cursor_grep_hit: None,
        directory_path: None,
        explorer_roots: false,
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("center_on").is_none(),
        "None center_on should be skipped"
    );
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
        center_on: Some(PickerItem::File {
            path_index: 0,
            relative_path: "x".into(),
            match_indices: vec![],
        }),
        center_on_cursor_grep_hit: None,
        directory_path: None,
        explorer_roots: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["center_on"]["kind"], "file");
    assert_eq!(v["center_on"]["relative_path"], "x");
    assert_eq!(v["center_on"]["path_index"], 0);
}

#[test]
fn picker_update_round_trips_through_notification() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdate, PickerUpdateParams};
    let params = PickerUpdateParams {
        kind: PickerKind::Files,
        generation: 7,
        offset: 0,
        items: vec![PickerItem::File {
            path_index: 0,
            relative_path: "a".into(),
            match_indices: vec![0],
        }],
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
    assert_eq!(v["params"]["items"][0]["relative_path"], "a");
    assert_eq!(v["params"]["items"][0]["path_index"], 0);
}

#[test]
fn picker_select_result_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::File {
        path: "/abs/path".into(),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({"kind": "file", "path": "/abs/path"})
    );
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
    assert_eq!(
        to_value(&r).unwrap(),
        json!({"kind": "buffer", "buffer_id": 42})
    );
}

#[test]
fn picker_kind_buffers_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Buffers).unwrap(), json!("buffers"));
}

#[test]
fn picker_kind_grep_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Grep).unwrap(), json!("grep"));
    assert_eq!(
        from_value::<PickerKind>(json!("grep")).unwrap(),
        PickerKind::Grep,
    );
}

#[test]
fn picker_item_grep_hit_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::GrepHit {
        path_index: 0,
        relative_path: "src/main.rs".into(),
        line: 12,
        col: 4,
        preview: "    let foo = 1;".into(),
        match_indices: vec![8, 9, 10],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "grep_hit",
            "path_index": 0,
            "relative_path": "src/main.rs",
            "line": 12,
            "col": 4,
            "preview": "    let foo = 1;",
            "match_indices": [8, 9, 10],
        })
    );
}

#[test]
fn picker_select_result_file_at_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::FileAt {
        path: "/abs/x.rs".into(),
        position: LogicalPosition { line: 3, col: 7 },
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({
            "kind": "file_at",
            "path": "/abs/x.rs",
            "position": {"line": 3, "col": 7},
        })
    );
}

#[test]
fn picker_kind_explorer_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Explorer).unwrap(), json!("explorer"));
    assert_eq!(
        from_value::<PickerKind>(json!("explorer")).unwrap(),
        PickerKind::Explorer,
    );
}

#[test]
fn picker_kind_projects_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Projects).unwrap(), json!("projects"));
    assert_eq!(
        from_value::<PickerKind>(json!("projects")).unwrap(),
        PickerKind::Projects,
    );
}

#[test]
fn picker_item_project_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::Project {
        name: "aether".into(),
        match_indices: vec![0, 4],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({"kind": "project", "name": "aether", "match_indices": [0, 4]})
    );
}

#[test]
fn picker_select_result_project_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::Project {
        name: "aether".into(),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({"kind": "project", "name": "aether"})
    );
}

#[test]
fn picker_item_dir_entry_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::DirEntry {
        name: "src".into(),
        is_dir: true,
        match_indices: vec![0, 1],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "dir_entry",
            "name": "src",
            "is_dir": true,
            "match_indices": [0, 1],
        })
    );
}

#[test]
fn picker_view_params_directory_path_skipped_when_none() {
    use aether_protocol::picker::{PickerKind, PickerViewParams};
    let p = PickerViewParams {
        kind: PickerKind::Explorer,
        reset: false,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor_grep_hit: None,
        directory_path: None,
        explorer_roots: false,
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("directory_path").is_none(),
        "None directory_path should be skipped from the wire"
    );
}

#[test]
fn picker_view_params_directory_path_serialized() {
    use aether_protocol::picker::{PickerKind, PickerViewParams};
    let p = PickerViewParams {
        kind: PickerKind::Explorer,
        reset: true,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor_grep_hit: None,
        directory_path: Some("/home/x/proj/src".into()),
        explorer_roots: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["directory_path"], "/home/x/proj/src");
}

#[test]
fn picker_view_result_directory_fields_skipped_when_none() {
    use aether_protocol::picker::PickerViewResult;
    let r = PickerViewResult {
        query: String::new(),
        generation: 0,
        total_candidates: 5,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
    };
    let v = to_value(&r).unwrap();
    assert!(v.get("directory_path").is_none());
    assert!(v.get("directory_parent").is_none());
    assert!(v.get("effective_center_on").is_none());
}

#[test]
fn picker_view_result_directory_fields_serialized() {
    use aether_protocol::picker::PickerViewResult;
    let r = PickerViewResult {
        query: String::new(),
        generation: 0,
        total_candidates: 3,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: Some("/proj/src".into()),
        directory_parent: Some("/proj".into()),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["directory_path"], "/proj/src");
    assert_eq!(v["directory_parent"], "/proj");
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
        jump_to: None,
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
        jump_to: None,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["buffer_id"], 11);
}

#[test]
fn buffer_open_params_jump_to_skipped_when_none() {
    use aether_protocol::buffer::BufferOpenParams;
    let p = BufferOpenParams {
        buffer_id: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: None,
    };
    let v = to_value(&p).unwrap();
    assert!(v.get("jump_to").is_none());
}

#[test]
fn buffer_open_params_jump_to_round_trips() {
    use aether_protocol::buffer::BufferOpenParams;
    let p = BufferOpenParams {
        buffer_id: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: Some(LogicalPosition { line: 7, col: 13 }),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["jump_to"], json!({"line": 7, "col": 13}));
}

// ---- file-watcher / external-change additions --------------------------------------------------

#[test]
fn buffer_state_params_external_flags_default_false_when_missing() {
    use aether_protocol::buffer::BufferStateParams;
    let v = json!({
        "buffer_id": 5,
        "saved_revision": 7,
        "saved_at_unix_ms": null
    });
    let p: BufferStateParams = from_value(v).unwrap();
    assert_eq!(p.buffer_id, 5);
    assert_eq!(p.saved_revision, 7);
    assert!(!p.externally_modified);
    assert!(!p.externally_deleted);
}

#[test]
fn buffer_state_params_external_flags_round_trip() {
    use aether_protocol::buffer::BufferStateParams;
    let p = BufferStateParams {
        buffer_id: 5,
        saved_revision: 7,
        saved_at_unix_ms: Some(123),
        externally_modified: true,
        externally_deleted: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["externally_modified"], true);
    assert_eq!(v["externally_deleted"], false);
    let p2: BufferStateParams = from_value(v).unwrap();
    assert!(p2.externally_modified);
    assert!(!p2.externally_deleted);
}

#[test]
fn buffer_reload_shape() {
    use aether_protocol::buffer::{BufferReload, BufferReloadParams, BufferReloadResult};
    assert_eq!(BufferReload::NAME, "buffer/reload");
    let p = BufferReloadParams {
        buffer_id: 11,
        force: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["buffer_id"], 11);
    assert_eq!(v["force"], false);

    // `force` defaults to false when missing on the wire.
    let parsed: BufferReloadParams = from_value(json!({"buffer_id": 11})).unwrap();
    assert_eq!(parsed.buffer_id, 11);
    assert!(!parsed.force);

    let r = BufferReloadResult {
        revision: 4,
        saved_at_unix_ms: Some(999),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["revision"], 4);
    assert_eq!(v["saved_at_unix_ms"], 999);
}

#[test]
fn external_change_error_codes_distinct() {
    use aether_protocol::error::ErrorCode;
    let codes = [
        ErrorCode::WOULD_OVERWRITE.code(),
        ErrorCode::EXTERNALLY_MODIFIED.code(),
        ErrorCode::EXTERNALLY_DELETED.code(),
    ];
    let unique: std::collections::HashSet<_> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len());
}
