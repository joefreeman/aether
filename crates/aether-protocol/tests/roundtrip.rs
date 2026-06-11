//! Wire-format round-trip tests. These exist to catch serde-derive surprises (untagged enums,
//! internally-tagged enums, optional fields) and to lock in the JSON shape against the protocol
//! doc.

use aether_protocol::buffer::{BufferOpen, BufferOpenParams, BufferOpenResult};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorSet, CursorSetParams, CursorState, Direction, Granularity,
    Motion, WordBoundary,
};
use aether_protocol::directory::{
    DirectoryCreate, DirectoryCreateParams, DirectoryCreateResult, DirectoryEntry, DirectoryList,
    DirectoryListParams, DirectoryListResult,
};
use aether_protocol::envelope::{
    ClientInbound, ErrorObject, ErrorResponse, JsonRpc, Notification, NotificationMethod, Request,
    RpcMethod,
};
use aether_protocol::git::{
    ApplyHunkStatus, BlameInfo, CommitInfo, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult,
    GitBlameLine, GitBlameLineParams, GitBlameLineResult, GitChangeCounts, GitCommitInfo,
    GitCommitInfoParams, GitCommitInfoResult, GitNavigateHunk, GitNavigateHunkParams,
    GitSetDiffView, GitSetDiffViewParams, HunkAction, HunkDirection,
};
use aether_protocol::input::{InputSurround, InputSurroundParams, InputText, InputTextParams};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspFormat, LspFormatResult, LspGotoDefinition,
    LspGotoDefinitionResult, LspHover, LspHoverResult, LspLocation, LspNavigateDiagnostic,
    LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspRestartServer, LspServerStatus,
    LspServerStatusList, LspStatus, LspStatusChanged,
};
use aether_protocol::project::{
    ProjectActivate, ProjectActivateParams, ProjectInfo, ProjectList, ProjectSummary,
};
use aether_protocol::search::{SearchSet, SearchSetParams};
use aether_protocol::viewport::ViewportLinesChanged;
use aether_protocol::viewport::{
    BufferStatusSnapshot, DiagnosticSeverity, DiagnosticSpan, DiffMarker, DiffStage,
    LogicalLineRender, VirtualRow, VirtualRowKind,
};
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
        params: Some(
            to_value(ProjectActivateParams {
                name: "aether".into(),
            })
            .unwrap(),
        ),
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
            is_uncommitted: false,
        }),
    };
    let v = to_value(&committed).unwrap();
    assert_eq!(v["blame"]["commit"], "a1b2c3d");
    assert_eq!(v["blame"]["author"], "Ada");
    assert_eq!(v["blame"]["timestamp"], 1_700_000_000_i64);
    assert_eq!(v["blame"]["is_uncommitted"], false);
    // The commit message no longer rides along on blame — it's fetched via `git/commit_info`.
    assert!(v["blame"].get("summary").is_none());
    let back: GitBlameLineResult = from_value(v).unwrap();
    assert_eq!(back.blame.unwrap().author, "Ada");

    let none = GitBlameLineResult { blame: None };
    assert_eq!(to_value(&none).unwrap(), json!({"blame": null}));
}

#[test]
fn git_commit_info_roundtrip() {
    let p = GitCommitInfoParams {
        buffer_id: 7,
        commit: "a1b2c3d".into(),
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"buffer_id": 7, "commit": "a1b2c3d"})
    );
    assert_eq!(GitCommitInfo::NAME, "git/commit_info");

    let res = GitCommitInfoResult {
        info: Some(CommitInfo {
            commit: "a1b2c3d4e5f6".into(),
            author: "Ada".into(),
            email: "ada@example.com".into(),
            date: "2026-06-01 14:32:05 +0100".into(),
            message: "Wire up blame\n\nLong body.".into(),
        }),
    };
    let v = to_value(&res).unwrap();
    assert_eq!(v["info"]["commit"], "a1b2c3d4e5f6");
    assert_eq!(v["info"]["email"], "ada@example.com");
    assert_eq!(v["info"]["date"], "2026-06-01 14:32:05 +0100");
    let back: GitCommitInfoResult = from_value(v).unwrap();
    assert_eq!(back.info.unwrap().message, "Wire up blame\n\nLong body.");

    let none = GitCommitInfoResult { info: None };
    assert_eq!(to_value(&none).unwrap(), json!({"info": null}));
}

#[test]
fn git_apply_hunk_roundtrip() {
    let p = GitApplyHunkParams {
        buffer_id: 4,
        action: HunkAction::Toggle,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"buffer_id": 4, "action": "toggle"})
    );
    assert_eq!(GitApplyHunk::NAME, "git/apply_hunk");

    // The status reports which direction a toggle resolved to.
    for (status, wire) in [
        (ApplyHunkStatus::Staged, "staged"),
        (ApplyHunkStatus::Unstaged, "unstaged"),
        (ApplyHunkStatus::Reverted, "reverted"),
        (ApplyHunkStatus::DirtyBuffer, "dirty_buffer"),
    ] {
        let res = GitApplyHunkResult {
            cursor: CursorState::default(),
            status,
        };
        let v = to_value(&res).unwrap();
        assert_eq!(v["status"], wire);
        let back: GitApplyHunkResult = from_value(v).unwrap();
        assert_eq!(back.status, status);
    }
}

#[test]
fn git_set_diff_view_params_shape() {
    let p = GitSetDiffViewParams {
        viewport_id: 9,
        enabled: true,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"viewport_id": 9, "enabled": true})
    );
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
        diff_stage: DiffStage::Unstaged,
        diagnostics: vec![],
    };
    let v = to_value(&bare).unwrap();
    assert!(
        v.get("virtual_rows_above").is_none(),
        "empty omitted from wire"
    );
    assert!(
        v.get("diff_marker").is_none(),
        "None marker omitted from wire"
    );
    assert!(
        v.get("diff_stage").is_none(),
        "unstaged stage omitted from wire"
    );
    assert!(
        v.get("diagnostics").is_none(),
        "empty diagnostics omitted from wire"
    );

    let with_del = LogicalLineRender {
        logical_line: 4,
        visual_rows: vec![],
        search_matches: vec![],
        virtual_rows_above: vec![VirtualRow {
            text: "old line".into(),
            kind: VirtualRowKind::Deleted,
            stage: DiffStage::Staged,
        }],
        diff_marker: Some(DiffMarker::Modified),
        diff_stage: DiffStage::Staged,
        diagnostics: vec![DiagnosticSpan {
            start: 4,
            end: 9,
            severity: DiagnosticSeverity::Error,
            message: "unused variable".into(),
        }],
    };
    let v = to_value(&with_del).unwrap();
    assert_eq!(v["virtual_rows_above"][0]["text"], "old line");
    assert_eq!(v["virtual_rows_above"][0]["kind"], "deleted");
    assert_eq!(v["virtual_rows_above"][0]["stage"], "staged");
    assert_eq!(v["diff_marker"], "modified");
    assert_eq!(v["diff_stage"], "staged");
    assert_eq!(v["diagnostics"][0]["start"], 4);
    assert_eq!(v["diagnostics"][0]["end"], 9);
    assert_eq!(v["diagnostics"][0]["severity"], "error");
    assert_eq!(v["diagnostics"][0]["message"], "unused variable");
    let back: LogicalLineRender = from_value(v).unwrap();
    assert_eq!(back.virtual_rows_above.len(), 1);
    assert_eq!(back.virtual_rows_above[0].kind, VirtualRowKind::Deleted);
    assert_eq!(back.virtual_rows_above[0].stage, DiffStage::Staged);
    assert_eq!(back.diff_marker, Some(DiffMarker::Modified));
    assert_eq!(back.diff_stage, DiffStage::Staged);
    assert_eq!(back.diagnostics[0].severity, DiagnosticSeverity::Error);
}

#[test]
fn buffer_status_snapshot_shape() {
    use aether_protocol::lsp::{DiagnosticCounts, LspServerStatus, LspStatus};

    // A clean, unbacked buffer: flags false, empty diagnostics and no LSP status drop off the wire.
    let empty = BufferStatusSnapshot::default();
    let v = to_value(&empty).unwrap();
    assert_eq!(v["externally_modified"], false);
    assert_eq!(v["externally_deleted"], false);
    assert!(v.get("diagnostics").is_none(), "empty counts omitted");
    assert!(v.get("lsp_status").is_none(), "no server → omitted");

    // A populated snapshot serializes every component, and round-trips back.
    let full = BufferStatusSnapshot {
        externally_modified: true,
        externally_deleted: false,
        diagnostics: DiagnosticCounts {
            errors: 2,
            warnings: 1,
            infos: 0,
            hints: 0,
        },
        lsp_status: Some(LspServerStatus {
            name: "rust-analyzer".into(),
            language: "rust".into(),
            workspace_root: "/ws".into(),
            status: LspStatus::Ready,
            progress: Vec::new(),
        }),
    };
    let v = to_value(&full).unwrap();
    assert_eq!(v["externally_modified"], true);
    assert_eq!(v["diagnostics"]["errors"], 2);
    assert_eq!(v["lsp_status"]["name"], "rust-analyzer");
    let back: BufferStatusSnapshot = from_value(v).unwrap();
    assert!(back.externally_modified);
    assert_eq!(back.diagnostics.errors, 2);
    assert_eq!(back.lsp_status.unwrap().language, "rust");

    // Absent on the wire (older server) → defaults, so deserialization never fails.
    let bare: BufferStatusSnapshot = from_value(json!({})).unwrap();
    assert!(bare.diagnostics.is_empty() && bare.lsp_status.is_none());
}

#[test]
fn git_change_counts_shape() {
    // The counts only ride `GitBufferStatus` (staged/unstaged halves); each empty side drops off
    // the wire there — pinned in `git_buffer_status_shape` below.
    let counts = GitChangeCounts::default();
    assert!(counts.is_empty());
    assert_eq!(
        to_value(counts).unwrap(),
        json!({"added": 0, "modified": 0, "deleted": 0})
    );
}

#[test]
fn git_buffer_status_shape() {
    use aether_protocol::git::GitBufferStatus;
    // Clean / outside a repo: branch None, both sides empty → empty object on the wire.
    assert_eq!(to_value(GitBufferStatus::default()).unwrap(), json!({}));

    // Branch + a staged modification + an unstaged addition; empty count side is omitted.
    let s = GitBufferStatus {
        branch: Some("main".into()),
        staged: GitChangeCounts {
            added: 0,
            modified: 1,
            deleted: 0,
        },
        unstaged: GitChangeCounts {
            added: 2,
            modified: 0,
            deleted: 0,
        },
    };
    let v = to_value(&s).unwrap();
    assert_eq!(v["branch"], "main");
    assert_eq!(
        v["staged"],
        json!({"added": 0, "modified": 1, "deleted": 0})
    );
    assert_eq!(
        v["unstaged"],
        json!({"added": 2, "modified": 0, "deleted": 0})
    );
    let back: GitBufferStatus = from_value(v).unwrap();
    assert_eq!(back.branch.as_deref(), Some("main"));
    assert_eq!((back.staged.modified, back.unstaged.added), (1, 2));
}

#[test]
fn git_navigate_hunk_shapes() {
    let p = GitNavigateHunkParams {
        buffer_id: 2,
        from_line: 10,
        direction: HunkDirection::Next,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 2, "from_line": 10, "direction": "next"})
    );
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

    let m = Motion::LogicalLineFirstNonblank {
        direction: Direction::Forward,
        count: 3,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "logical_line_first_nonblank", "direction": "forward", "count": 3})
    );

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
fn cursor_set_params_granularity() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(CursorSet::NAME, "cursor/set");

    // Char granularity (the default) is omitted on the wire.
    let v = to_value(CursorSetParams {
        buffer_id: 7,
        position: LogicalPosition { line: 1, col: 4 },
        anchor: LogicalPosition { line: 1, col: 4 },
        granularity: Granularity::Char,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "buffer_id": 7,
            "position": {"line": 1, "col": 4},
            "anchor": {"line": 1, "col": 4},
        })
    );

    // Word/Line serialise as snake_case strings.
    let v = to_value(CursorSetParams {
        buffer_id: 7,
        position: LogicalPosition { line: 1, col: 4 },
        anchor: LogicalPosition { line: 0, col: 2 },
        granularity: Granularity::Word,
    })
    .unwrap();
    assert_eq!(v["granularity"], "word");

    // Omitted on the wire defaults to Char (back-compat with older clients).
    let p: CursorSetParams = from_value(json!({
        "buffer_id": 7,
        "position": {"line": 0, "col": 0},
        "anchor": {"line": 0, "col": 0},
    }))
    .unwrap();
    assert_eq!(p.granularity, Granularity::Char);
    let p: CursorSetParams = from_value(json!({
        "buffer_id": 7,
        "position": {"line": 0, "col": 0},
        "anchor": {"line": 0, "col": 0},
        "granularity": "line",
    }))
    .unwrap();
    assert_eq!(p.granularity, Granularity::Line);
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
        transient: false,
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
        lsp_server: Some(aether_protocol::lsp::LspServerRef {
            language: "rust".into(),
            workspace_root: "/proj".into(),
        }),
    })
    .unwrap();
    assert_eq!(v["buffer_id"], 42);
    assert_eq!(v["language"], "rust");
    assert_eq!(v["lsp_server"]["workspace_root"], "/proj");
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
        transient: false,
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
        lsp_server: None,
    })
    .unwrap();
    assert_eq!(v["scroll"]["logical_line"], 7);
    assert_eq!(v["scroll"]["sub_row"], 0.5);
    // `scratch_number: None` skips serialisation, like a file buffer.
    assert!(v.get("scratch_number").is_none());
    // `lsp_server: None` is skipped too.
    assert!(
        v.get("lsp_server").is_none(),
        "lsp_server: None should be skipped"
    );
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
fn lsp_method_names() {
    assert_eq!(LspServerStatusList::NAME, "lsp/server_status");
    assert_eq!(LspRestartServer::NAME, "lsp/restart_server");
    assert_eq!(LspStatusChanged::NAME, "lsp/status_changed");
}

#[test]
fn lsp_diagnostics_changed_shape() {
    assert_eq!(LspDiagnosticsChanged::NAME, "lsp/diagnostics_changed");
    let p = LspDiagnosticsChangedParams {
        buffer_id: 5,
        counts: DiagnosticCounts {
            errors: 2,
            warnings: 1,
            infos: 0,
            hints: 3,
        },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["buffer_id"], 5);
    assert_eq!(v["counts"]["errors"], 2);
    assert_eq!(v["counts"]["hints"], 3);
    assert!(!DiagnosticCounts {
        errors: 1,
        ..Default::default()
    }
    .is_empty());
    assert!(DiagnosticCounts::default().is_empty());
}

#[test]
fn lsp_hover_and_goto_shapes() {
    assert_eq!(LspHover::NAME, "lsp/hover");
    assert_eq!(LspGotoDefinition::NAME, "lsp/goto_definition");
    // Cursor-relative params carry only the buffer.
    let v = to_value(LspBufferParams { buffer_id: 3 }).unwrap();
    assert_eq!(v, json!({"buffer_id": 3}));
    // Hover: optional contents.
    let v = to_value(LspHoverResult {
        contents: Some("fn x()".into()),
    })
    .unwrap();
    assert_eq!(v["contents"], "fn x()");
    // Goto: optional location with absolute path + byte-col position.
    let r = LspGotoDefinitionResult {
        location: Some(LspLocation {
            path: "/p/src/lib.rs".into(),
            position: LogicalPosition { line: 12, col: 4 },
        }),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["location"]["path"], "/p/src/lib.rs");
    assert_eq!(v["location"]["position"]["line"], 12);
    let back: LspGotoDefinitionResult = from_value(v).unwrap();
    assert_eq!(back.location.unwrap().position.col, 4);
}

#[test]
fn lsp_format_shape() {
    assert_eq!(LspFormat::NAME, "lsp/format");
    // Params are the shared cursor-relative buffer params.
    assert_eq!(
        to_value(LspBufferParams { buffer_id: 4 }).unwrap(),
        json!({"buffer_id": 4})
    );
    let r = LspFormatResult {
        cursor: CursorState::default(),
        status: FormatStatus::Applied,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["status"], "applied");
    assert_eq!(
        to_value(FormatStatus::Unsupported).unwrap(),
        json!("unsupported")
    );
    let back: LspFormatResult = from_value(v).unwrap();
    assert_eq!(back.status, FormatStatus::Applied);
}

#[test]
fn lsp_navigate_diagnostic_shape() {
    assert_eq!(LspNavigateDiagnostic::NAME, "lsp/navigate_diagnostic");
    let p = LspNavigateDiagnosticParams {
        buffer_id: 7,
        from_line: 3,
        direction: DiagnosticDirection::Next,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 7, "from_line": 3, "direction": "next"})
    );
    assert_eq!(to_value(DiagnosticDirection::Prev).unwrap(), json!("prev"));
    let r = LspNavigateDiagnosticResult {
        cursor: CursorState::default(),
        moved: true,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["moved"], true);
    let back: LspNavigateDiagnosticResult = from_value(v).unwrap();
    assert!(back.moved);
}

#[test]
fn lsp_status_is_internally_tagged() {
    // Unit variant: just the tag.
    assert_eq!(
        to_value(LspStatus::Ready).unwrap(),
        json!({"state": "ready"})
    );
    // Struct variant: tag alongside its fields, flat.
    assert_eq!(
        to_value(LspStatus::Crashed {
            code: Some(1),
            message: "boom".into(),
        })
        .unwrap(),
        json!({"state": "crashed", "code": 1, "message": "boom"})
    );
    // Round-trips back.
    let s = LspStatus::Stopped;
    assert_eq!(from_value::<LspStatus>(to_value(&s).unwrap()).unwrap(), s);
}

#[test]
fn lsp_server_status_shape() {
    let st = LspServerStatus {
        name: "rust-analyzer".into(),
        language: "rust".into(),
        workspace_root: "/home/joe/proj".into(),
        status: LspStatus::Initializing,
        progress: Vec::new(),
    };
    let v = to_value(&st).unwrap();
    assert_eq!(v["name"], "rust-analyzer");
    assert_eq!(v["language"], "rust");
    assert_eq!(v["workspace_root"], "/home/joe/proj");
    assert_eq!(v["status"], json!({"state": "initializing"}));
    assert!(v.get("progress").is_none(), "idle server omits progress");

    // A busy server carries its active work-done operations.
    use aether_protocol::lsp::LspProgress;
    let busy = LspServerStatus {
        progress: vec![LspProgress {
            title: "cargo check".into(),
            message: Some("1/4".into()),
            percentage: Some(25),
        }],
        ..st
    };
    let v = to_value(&busy).unwrap();
    assert_eq!(v["progress"][0]["title"], "cargo check");
    assert_eq!(v["progress"][0]["message"], "1/4");
    assert_eq!(v["progress"][0]["percentage"], 25);
    let back: LspServerStatus = from_value(v).unwrap();
    assert_eq!(back.progress.len(), 1);
    assert_eq!(back.progress[0].percentage, Some(25));
}

#[test]
fn lsp_status_changed_notification_roundtrip() {
    let n = Notification {
        jsonrpc: JsonRpc,
        method: LspStatusChanged::NAME.into(),
        params: to_value(LspServerStatus {
            name: "gopls".into(),
            language: "go".into(),
            workspace_root: "/x".into(),
            status: LspStatus::Ready,
            progress: Vec::new(),
        })
        .unwrap(),
    };
    let s = serde_json::to_string(&n).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["method"], "lsp/status_changed");
    assert_eq!(v["params"]["status"]["state"], "ready");
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
        transient: None,
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
fn buffer_closed_notification_shape() {
    use aether_protocol::buffer::BufferClosedParams;
    // With a next buffer to switch to.
    let some = to_value(BufferClosedParams {
        buffer_id: 4,
        next_buffer_id: Some(7),
    })
    .unwrap();
    assert_eq!(some, json!({"buffer_id": 4, "next_buffer_id": 7}));
    // No buffers remain — `next_buffer_id` is omitted, signalling "open a fresh scratch".
    let none = to_value(BufferClosedParams {
        buffer_id: 4,
        next_buffer_id: None,
    })
    .unwrap();
    assert_eq!(none, json!({"buffer_id": 4}));
    // And it deserializes back when the field is absent.
    let parsed: BufferClosedParams = from_value(json!({"buffer_id": 9})).unwrap();
    assert_eq!(parsed.buffer_id, 9);
    assert_eq!(parsed.next_buffer_id, None);
}

#[test]
fn nav_goto_params_shape() {
    use aether_protocol::cursor::CursorState;
    use aether_protocol::nav::NavGotoParams;
    // File entry: path fields present, buffer_id omitted; cursor carries the selection.
    let p = NavGotoParams {
        buffer_id: None,
        path_index: Some(0),
        relative_path: Some("src/main.rs".into()),
        cursor: CursorState {
            position: LogicalPosition { line: 9, col: 2 },
            anchor: LogicalPosition { line: 5, col: 0 },
            match_bracket: None,
            grep_position: None,
        },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(
        v,
        json!({
            "path_index": 0,
            "relative_path": "src/main.rs",
            "cursor": { "position": {"line": 9, "col": 2}, "anchor": {"line": 5, "col": 0} },
        })
    );
    // Round-trips with a bare cursor (no match_bracket/grep_position) and a buffer_id reference.
    let parsed: NavGotoParams = from_value(json!({
        "buffer_id": 3,
        "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
    }))
    .unwrap();
    assert_eq!(parsed.buffer_id, Some(3));
    assert_eq!(parsed.relative_path, None);
}

#[test]
fn nav_step_result_omits_absent_target() {
    use aether_protocol::nav::NavStepResult;
    assert_eq!(to_value(NavStepResult { target: None }).unwrap(), json!({}));
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
        git_status: None,
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "file",
            "path_index": 0,
            "relative_path": "src/main.rs",
            "match_indices": [0, 4],
        }),
        "git_status is omitted from the wire when None"
    );
}

#[test]
fn picker_item_file_carries_git_status() {
    use aether_protocol::git::GitStatus;
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::File {
        path_index: 0,
        relative_path: "src/main.rs".into(),
        match_indices: vec![],
        git_status: Some(GitStatus::Modified),
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["git_status"], "modified");
    let back: PickerItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn picker_item_diagnostic_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::Diagnostics).unwrap(),
        json!("diagnostics")
    );
    let item = PickerItem::Diagnostic {
        line: 12,
        col: 4,
        end_line: 12,
        end_col: 9,
        severity: DiagnosticSeverity::Error,
        message: "mismatched types".into(),
        match_indices: vec![0, 1],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "diagnostic");
    assert_eq!(v["line"], 12);
    assert_eq!(v["col"], 4);
    assert_eq!(v["end_line"], 12);
    assert_eq!(v["end_col"], 9);
    assert_eq!(v["severity"], "error");
    assert_eq!(v["message"], "mismatched types");
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    // The range fields default when an older server omits them (back-compat).
    let bare: PickerItem = from_value(json!({
        "kind": "diagnostic", "line": 3, "col": 0, "severity": "warning", "message": "unused"
    }))
    .unwrap();
    assert!(matches!(
        bare,
        PickerItem::Diagnostic {
            end_line: 0,
            end_col: 0,
            ..
        }
    ));
}

#[test]
fn picker_item_reference_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::References).unwrap(),
        json!("references")
    );
    let item = PickerItem::Reference {
        path: "/home/u/proj/src/lib.rs".into(),
        display_path: "src/lib.rs".into(),
        line: 41,
        col: 7,
        preview: "    helper();".into(),
        match_indices: vec![4, 5],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "reference");
    assert_eq!(v["path"], "/home/u/proj/src/lib.rs");
    assert_eq!(v["display_path"], "src/lib.rs");
    assert_eq!(v["line"], 41);
    assert_eq!(v["col"], 7);
    assert_eq!(v["preview"], "    helper();");
    assert_eq!(v["match_indices"], json!([4, 5]));
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    // match_indices defaults to empty when omitted (matches the other item variants).
    let bare: PickerItem = from_value(json!({
        "kind": "reference", "path": "/a", "display_path": "a", "line": 0, "col": 0, "preview": ""
    }))
    .unwrap();
    assert!(
        matches!(bare, PickerItem::Reference { ref match_indices, .. } if match_indices.is_empty())
    );
}

#[test]
fn picker_item_lsp_server_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::LspServers).unwrap(),
        json!("lsp_servers")
    );
    let item = PickerItem::LspServer {
        name: "rust-analyzer".into(),
        language: "rust".into(),
        workspace_root: "/proj".into(),
        root_label: String::new(),
        status: LspStatus::Ready,
        progress: vec![],
        match_indices: vec![0, 1],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "lsp_server");
    assert_eq!(v["name"], "rust-analyzer");
    assert_eq!(v["language"], "rust");
    // Status nests its own internally-tagged shape.
    assert_eq!(v["status"], json!({"state": "ready"}));
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);
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
        buffer_id: None,
        explorer_roots: false,
        filters: None,
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
            git_status: None,
        }),
        center_on_cursor_grep_hit: None,
        directory_path: None,
        buffer_id: None,
        explorer_roots: false,
        filters: None,
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
            git_status: None,
        }],
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        grep_display_offset: None,
        grep_total_display_rows: None,
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
    use aether_protocol::picker::{BufferDirtyState, PickerItem};
    let item = PickerItem::Buffer {
        buffer_id: 7,
        display: "src/main.rs".into(),
        status: BufferDirtyState::ExternallyModified,
        path_index: Some(0),
        relative_path: Some("src/main.rs".into()),
        match_indices: vec![0, 4],
        transient: true,
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "buffer",
            "buffer_id": 7,
            "display": "src/main.rs",
            "status": "externally_modified",
            "path_index": 0,
            "relative_path": "src/main.rs",
            "match_indices": [0, 4],
            "transient": true,
        })
    );

    // Scratch buffer: no path → both fields skipped; clean status → `status` skipped too;
    // permanent → `transient` skipped (the common case).
    let scratch = PickerItem::Buffer {
        buffer_id: 9,
        display: "(scratch 1)".into(),
        status: BufferDirtyState::Clean,
        path_index: None,
        relative_path: None,
        match_indices: vec![],
        transient: false,
    };
    let sv = to_value(&scratch).unwrap();
    assert!(sv.get("status").is_none(), "clean buffer omits status");
    assert!(
        sv.get("path_index").is_none(),
        "scratch buffer omits path_index"
    );
    assert!(
        sv.get("relative_path").is_none(),
        "scratch buffer omits relative_path"
    );
    assert!(
        sv.get("transient").is_none(),
        "permanent buffer omits transient"
    );

    // A clean status absent on the wire deserializes back to `Clean` (serde default).
    let back: PickerItem = from_value(json!({
        "kind": "buffer", "buffer_id": 9, "display": "(scratch 1)"
    }))
    .unwrap();
    assert_eq!(back, scratch);
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
        git_status: None,
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "dir_entry",
            "name": "src",
            "is_dir": true,
            "match_indices": [0, 1],
        }),
        "git_status is omitted from the wire when None"
    );
}

#[test]
fn picker_item_dir_entry_carries_git_status() {
    use aether_protocol::git::GitStatus;
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::DirEntry {
        name: "target".into(),
        is_dir: true,
        match_indices: vec![],
        git_status: Some(GitStatus::Ignored),
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "dir_entry",
            "name": "target",
            "is_dir": true,
            "match_indices": [],
            "git_status": "ignored",
        })
    );
    // Round-trips back to the same value.
    let back: PickerItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
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
        buffer_id: None,
        explorer_roots: false,
        filters: None,
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
        buffer_id: None,
        explorer_roots: false,
        filters: None,
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
        filters: Default::default(),
    };
    let v = to_value(&r).unwrap();
    assert!(v.get("directory_path").is_none());
    assert!(v.get("directory_parent").is_none());
    assert!(v.get("effective_center_on").is_none());
    assert!(
        v.get("filters").is_none(),
        "all-default filters should be skipped from the wire"
    );
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
        filters: Default::default(),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["directory_path"], "/proj/src");
    assert_eq!(v["directory_parent"], "/proj");
}

#[test]
fn picker_filters_default_is_empty_object_and_absent_field_deserializes() {
    use aether_protocol::picker::{PickerFilters, PickerQueryParams};
    // All-default filters serialize to an empty object (every field is skipped)...
    assert_eq!(to_value(PickerFilters::default()).unwrap(), json!({}));
    // ...and an absent `filters` field on params deserializes to the default set, so the old
    // wire shape stays valid.
    let p: PickerQueryParams =
        from_value(json!({"kind": "grep", "query": "foo", "generation": 3})).unwrap();
    assert!(p.filters.is_default());
    let v = to_value(&p).unwrap();
    assert!(
        v.get("filters").is_none(),
        "default filters should be skipped on the wire"
    );
}

#[test]
fn picker_filters_wire_shape() {
    use aether_protocol::picker::{CaseMode, PickerFilters, ScopedPath};
    let f = PickerFilters {
        case: CaseMode::Insensitive,
        whole_word: true,
        fixed_string: true,
        include_ignored: true,
        include_hidden: true,
        hide_ignored: true,
        hide_hidden: true,
        changed_only: true,
        globs: vec!["*.rs".into(), "!*_test.rs".into()],
        directories: vec![
            ScopedPath {
                path_index: 1,
                relative_path: "src/app".into(),
            },
            ScopedPath {
                path_index: 0,
                relative_path: String::new(),
            },
        ],
    };
    let v = to_value(&f).unwrap();
    assert_eq!(
        v,
        json!({
            "case": "insensitive",
            "whole_word": true,
            "fixed_string": true,
            "include_ignored": true,
            "include_hidden": true,
            "hide_ignored": true,
            "hide_hidden": true,
            "changed_only": true,
            "globs": ["*.rs", "!*_test.rs"],
            "directories": [
                {"path_index": 1, "relative_path": "src/app"},
                {"path_index": 0, "relative_path": ""},
            ],
        })
    );
    let back: PickerFilters = from_value(v).unwrap();
    assert_eq!(back, f);
}

#[test]
fn picker_view_result_filters_serialized_when_non_default() {
    use aether_protocol::picker::{PickerFilters, PickerViewResult};
    let r = PickerViewResult {
        query: "needle".into(),
        generation: 2,
        total_candidates: 3,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: PickerFilters {
            whole_word: true,
            ..Default::default()
        },
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["filters"], json!({"whole_word": true}));
}

#[test]
fn buffer_open_params_buffer_id_skipped_when_none() {
    use aether_protocol::buffer::BufferOpenParams;
    let p = BufferOpenParams {
        transient: None,
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
        transient: None,
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
        transient: None,
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
        transient: None,
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
        transient: false,
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

// ---- transient buffers ---------------------------------------------------------------------

/// `BufferOpenParams.transient` is a three-state intent: omitted = leave as-is, `true` =
/// transient-if-created, `false` = pin. Pin the skip-when-None shape and the round trip.
#[test]
fn buffer_open_params_transient_shape() {
    use aether_protocol::buffer::BufferOpenParams;
    let mut p = BufferOpenParams {
        transient: None,
        buffer_id: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: None,
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("transient").is_none(),
        "transient: None should be skipped"
    );

    p.transient = Some(true);
    let v = to_value(&p).unwrap();
    assert_eq!(v["transient"], true);
    let p2: BufferOpenParams = from_value(v).unwrap();
    assert_eq!(p2.transient, Some(true));

    // Missing on the wire deserialises as None (older clients).
    let p3: BufferOpenParams = from_value(json!({"path_index": 0, "relative_path": "x"})).unwrap();
    assert_eq!(p3.transient, None);
}

/// `transient` defaults to false when missing in `BufferOpenResult` and `BufferStateParams`,
/// and round-trips when set.
#[test]
fn transient_flag_defaults_false_in_result_and_state() {
    use aether_protocol::buffer::{BufferOpenResult, BufferStateParams};
    let r: BufferOpenResult = from_value(json!({
        "buffer_id": 1,
        "language": null,
        "line_count": 1,
        "byte_count": 0,
        "revision": 0,
        "saved_revision": 0,
        "path": null
    }))
    .unwrap();
    assert!(!r.transient);

    let s: BufferStateParams = from_value(json!({
        "buffer_id": 5,
        "saved_revision": 7,
        "saved_at_unix_ms": null,
        "transient": true
    }))
    .unwrap();
    assert!(s.transient);
    let v = to_value(&s).unwrap();
    assert_eq!(v["transient"], true);
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
