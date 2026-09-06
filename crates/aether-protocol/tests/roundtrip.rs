//! Wire-format round-trip tests. These exist to catch serde-derive surprises (untagged enums,
//! internally-tagged enums, optional fields) and to lock in the JSON shape against the protocol
//! doc.

use aether_protocol::coords::ElementRow;
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorSelectWord, CursorSelectWordParams, CursorSet,
    CursorSetParams, CursorState, Direction, Granularity, Motion, SelectionEdge, VerticalDirection,
    WordBoundary,
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
    ApplyHunkStatus, ApplyScope, BlameInfo, CommitInfo, GitApplyHunk, GitApplyHunkParams,
    GitApplyHunkResult, GitBaselineChoice, GitBaselineSource, GitBlameChanged,
    GitBlameChangedParams, GitBlameLine, GitBlameLineParams, GitBlameLineResult, GitBufferStatus,
    GitChangeCounts, GitHead, GitRefresh, GitRefreshParams, GitRefreshResult, GitRepoInfo,
    GitSetBaseline, GitSetBaselineParams, GitSetBaselineResult, GitSetBlameFollow,
    GitSetBlameFollowParams, GitSetDiffView, GitSetDiffViewParams, GitStashPush,
    GitStashPushParams, GitStashResult, GitStashStatus, HunkAction,
};
use aether_protocol::input::{
    BufferOnlyParams, CountedEditParams, InputAdjustNumber, InputAdjustNumberParams,
    InputBackspace, InputNewlineAndIndentParams, InputSurround, InputSurroundParams, InputTab,
    InputText, InputTextParams, UndoRedoParams,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspDocumentHighlight, LspDocumentHighlightParams, LspFormat,
    LspFormatResult, LspGotoDefinition, LspGotoDefinitionResult, LspHover, LspHoverResult,
    LspLocation, LspNavigateDiagnostic, LspNavigateDiagnosticParams, LspNavigateDiagnosticResult,
    LspReadiness, LspRestartServer, LspServerStatus, LspStatus, LspStatusChanged,
};
use aether_protocol::picker::{BranchCheckout, CaseMode, MatchOptions};
use aether_protocol::search::{SearchSet, SearchSetParams};
use aether_protocol::sneak::{
    SneakCancel, SneakSelect, SneakSelectParams, SneakTarget, SneakUpdate, SneakUpdateParams,
    SneakUpdateResult,
};
use aether_protocol::ui::{Element, RailJoin};
use aether_protocol::view::{BufferDescription, ViewOpen, ViewOpenParams, ViewOpenResult};
use aether_protocol::viewport::ViewportLinesChanged;
use aether_protocol::viewport::{
    BaselineRow, BufferStatusSnapshot, ChromeKind, DiagnosticSeverity, DiagnosticSpan, DiffMarker,
    DiffStage, EmphasisRange, LineChange, LogicalLineRender, ViewportLinesChangedParams,
};
use aether_protocol::workspace::{
    WorkspaceActivate, WorkspaceActivateParams, WorkspaceInfo, WorkspaceList, WorkspaceOpenPath,
    WorkspaceOpenPathParams, WorkspaceSummary,
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
        method: WorkspaceActivate::NAME.into(),
        params: Some(
            to_value(WorkspaceActivateParams {
                worktrees: None,
                name: "aether".into(),
                open_last: false,
            })
            .unwrap(),
        ),
    };
    let s = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "workspace/activate");
    assert_eq!(v["params"]["name"], "aether");
}

#[test]
fn workspace_open_path_roundtrip() {
    let req = Request {
        jsonrpc: JsonRpc,
        id: 9,
        method: WorkspaceOpenPath::NAME.into(),
        params: Some(
            to_value(WorkspaceOpenPathParams {
                path: "/etc/hosts".into(),
                transient: None,
                create_if_missing: false,
                jump_to: None,
            })
            .unwrap(),
        ),
    };
    let s = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["method"], "workspace/open_path");
    assert_eq!(v["params"]["path"], "/etc/hosts");
    // `transient: None` stays off the wire.
    assert!(v["params"].get("transient").is_none());
    // Nor does an absent jump — `ae PATH` with no `:LINE` suffix, and every overlay open.
    assert!(v["params"].get("jump_to").is_none());
    // A jump rides in `view/open`'s shape (0-based), which this delegates to: `ae /etc/hosts:42`.
    let jumped = to_value(WorkspaceOpenPathParams {
        path: "/etc/hosts".into(),
        transient: None,
        create_if_missing: false,
        jump_to: Some(aether_protocol::LogicalPosition { line: 41, col: 9 }),
    })
    .unwrap();
    assert_eq!(jumped["jump_to"], serde_json::json!({"line": 41, "col": 9}));
    // `create_if_missing` rides the wire when set, and an old-style params object without it
    // (or with it false — serialized either way) still parses.
    let with: WorkspaceOpenPathParams = serde_json::from_value(
        serde_json::json!({ "path": "/tmp/new.txt", "create_if_missing": true }),
    )
    .unwrap();
    assert!(with.create_if_missing);
    let without: WorkspaceOpenPathParams =
        serde_json::from_value(serde_json::json!({ "path": "/etc/hosts" })).unwrap();
    assert!(!without.create_if_missing);
}

#[test]
fn buffer_open_absolute_path_roundtrips_and_omits_when_absent() {
    // Present: serialized through.
    let with = to_value(ViewOpenParams {
        absolute_path: Some("/tmp/x.rs".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(with["absolute_path"], "/tmp/x.rs");
    // Absent (the default): skipped, so existing root-relative opens keep their wire shape.
    let without = to_value(ViewOpenParams::default()).unwrap();
    assert!(without.get("absolute_path").is_none());
}

#[test]
fn ephemeral_workspace_id_predicate() {
    assert!(aether_protocol::is_ephemeral_workspace_id("ephemeral/1"));
    assert!(!aether_protocol::is_ephemeral_workspace_id("my-workspace"));
    // A real workspace name can't contain a separator, so the namespaces never collide.
    assert!(!aether_protocol::is_ephemeral_workspace_id("ephemeral")); // no slash, not the prefix
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
        include_commit_info: false,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 3, "line": 41, "include_commit_info": false})
    );
    assert_eq!(GitBlameLine::NAME, "git/blame_line");
}

#[test]
fn git_blame_line_result_roundtrip() {
    // A committed line and an uncommitted line both round-trip; `None` is the no-blame case.
    // The lean `blame` carries no message; full commit details ride alongside in `commit_info`
    // when the caller asks for them (`include_commit_info`, composite G).
    let committed = GitBlameLineResult {
        blame: Some(BlameInfo {
            commit: "a1b2c3d".into(),
            author: "Ada".into(),
            timestamp: 1_700_000_000,
            is_uncommitted: false,
        }),
        commit_info: Some(CommitInfo {
            commit: "a1b2c3d4e5f6".into(),
            author: "Ada".into(),
            email: "ada@example.com".into(),
            date: "2026-06-01 14:32:05 +0100".into(),
            message: "Wire up blame\n\nLong body.".into(),
        }),
    };
    let v = to_value(&committed).unwrap();
    assert_eq!(v["blame"]["commit"], "a1b2c3d");
    assert_eq!(v["blame"]["author"], "Ada");
    assert_eq!(v["blame"]["timestamp"], 1_700_000_000_i64);
    assert_eq!(v["blame"]["is_uncommitted"], false);
    // The commit message no longer rides on `blame` itself — it lives in `commit_info`.
    assert!(v["blame"].get("summary").is_none());
    assert_eq!(v["commit_info"]["commit"], "a1b2c3d4e5f6");
    assert_eq!(v["commit_info"]["email"], "ada@example.com");
    assert_eq!(v["commit_info"]["date"], "2026-06-01 14:32:05 +0100");
    let back: GitBlameLineResult = from_value(v).unwrap();
    assert_eq!(back.blame.unwrap().author, "Ada");
    assert_eq!(
        back.commit_info.unwrap().message,
        "Wire up blame\n\nLong body."
    );

    let none = GitBlameLineResult {
        blame: None,
        commit_info: None,
    };
    assert_eq!(to_value(&none).unwrap(), json!({"blame": null}));
}

#[test]
fn git_blame_follow_shapes() {
    // The subscription toggle…
    let p = GitSetBlameFollowParams {
        buffer_id: 3,
        enabled: true,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"buffer_id": 3, "enabled": true})
    );
    assert_eq!(GitSetBlameFollow::NAME, "git/set_blame_follow");

    // …and the push it buys. `blame: None` (no repo / untracked / past EOF) drops off the wire.
    let with = GitBlameChangedParams {
        buffer_id: 3,
        line: 41,
        blame: Some(BlameInfo {
            commit: "a1b2c3d".into(),
            author: "Ada".into(),
            timestamp: 1_700_000_000,
            is_uncommitted: false,
        }),
    };
    let v = to_value(&with).unwrap();
    assert_eq!(v["buffer_id"], 3);
    assert_eq!(v["line"], 41);
    assert_eq!(v["blame"]["author"], "Ada");
    let back: GitBlameChangedParams = from_value(v).unwrap();
    assert_eq!(back.blame.unwrap().commit, "a1b2c3d");

    let without = GitBlameChangedParams {
        buffer_id: 3,
        line: 41,
        blame: None,
    };
    assert_eq!(
        to_value(&without).unwrap(),
        json!({"buffer_id": 3, "line": 41})
    );
    assert_eq!(GitBlameChanged::NAME, "git/blame_changed");
}

#[test]
fn git_repo_identity_shapes() {
    // The ordinary case: git dir and common dir coincide, one root, on a branch with an upstream.
    let ordinary = GitRepoInfo {
        repo_id: "/src/aether".into(),
        git_dir: "/src/aether/.git".into(),
        common_dir: "/src/aether/.git".into(),
        head: GitHead::Branch {
            name: "main".into(),
            upstream: Some("origin/main".into()),
        },
        roots: vec!["/src/aether".into()],
    };
    let v = to_value(&ordinary).unwrap();
    assert_eq!(
        v,
        json!({
            "repo_id": "/src/aether",
            "git_dir": "/src/aether/.git",
            "common_dir": "/src/aether/.git",
            "head": {"state": "branch", "name": "main", "upstream": "origin/main"},
            "roots": ["/src/aether"],
        })
    );
    let back: GitRepoInfo = from_value(v).unwrap();
    assert_eq!(back, ordinary);

    // A linked worktree: distinct id and git dir, but the *common* dir points back at the main
    // repo — that shared value is how a client tells worktree siblings apart from separate repos.
    let worktree = GitRepoInfo {
        repo_id: "/src/aether-worktrees/git-phase-2".into(),
        git_dir: "/src/aether/.git/worktrees/git-phase-2".into(),
        common_dir: "/src/aether/.git".into(),
        head: GitHead::Branch {
            name: "git-phase-2".into(),
            upstream: None,
        },
        roots: vec![],
    };
    let v = to_value(&worktree).unwrap();
    // A never-pushed branch drops `upstream`, and a buffer-only repo drops `roots` entirely.
    assert_eq!(v["head"], json!({"state": "branch", "name": "git-phase-2"}));
    assert!(v.get("roots").is_none());
    assert_eq!(v["common_dir"], "/src/aether/.git");
    assert_eq!(from_value::<GitRepoInfo>(v).unwrap(), worktree);

    // The other two head states are externally tagged on `state` like the first.
    assert_eq!(
        to_value(&GitHead::Detached {
            oid: "a1b2c3d".into()
        })
        .unwrap(),
        json!({"state": "detached", "oid": "a1b2c3d"})
    );
    assert_eq!(
        to_value(&GitHead::Unborn {
            name: "main".into()
        })
        .unwrap(),
        json!({"state": "unborn", "name": "main"})
    );
}

#[test]
fn git_set_baseline_shapes() {
    assert_eq!(GitSetBaseline::NAME, "git/set_baseline");
    assert_eq!(
        to_value(&GitSetBaselineParams {
            repo_id: "/src/aether".into(),
            source: Some(GitBaselineChoice::Rev { rev: "main".into() }),
        })
        .unwrap(),
        json!({"repo_id": "/src/aether", "source": {"kind": "rev", "rev": "main"}})
    );
    // The saved-file baseline carries nothing but its tag — there is no revision to name.
    assert_eq!(
        to_value(&GitSetBaselineParams {
            repo_id: "/src/aether".into(),
            source: Some(GitBaselineChoice::Saved),
        })
        .unwrap(),
        json!({"repo_id": "/src/aether", "source": {"kind": "saved"}})
    );
    // Clearing back to the default is the absent field, not a null.
    assert_eq!(
        to_value(&GitSetBaselineParams {
            repo_id: "/src/aether".into(),
            source: None,
        })
        .unwrap(),
        json!({"repo_id": "/src/aether"})
    );

    // The label is what the user typed; the commit is what it was pinned to.
    let set = GitSetBaselineResult {
        baseline: Some(GitBaselineSource::Rev {
            label: "main".into(),
            commit: "a1b2c3d".into(),
        }),
        buffers: vec![1, 2],
    };
    let v = to_value(&set).unwrap();
    assert_eq!(
        v,
        json!({"baseline": {"kind": "rev", "label": "main", "commit": "a1b2c3d"}, "buffers": [1, 2]})
    );
    assert_eq!(from_value::<GitSetBaselineResult>(v).unwrap(), set);
    let saved = GitSetBaselineResult {
        baseline: Some(GitBaselineSource::Saved),
        buffers: vec![],
    };
    let v = to_value(&saved).unwrap();
    assert_eq!(v, json!({"baseline": {"kind": "saved"}}));
    assert_eq!(from_value::<GitSetBaselineResult>(v).unwrap(), saved);
    assert_eq!(
        to_value(&GitSetBaselineResult {
            baseline: None,
            buffers: vec![],
        })
        .unwrap(),
        json!({})
    );

    // The status bar's copy of it: absent while diffing against the index, so a client that
    // ignores the field keeps reading the same shape it always did.
    let plain = GitBufferStatus::default();
    assert!(to_value(&plain).unwrap().get("baseline").is_none());
    let against_rev = GitBufferStatus {
        baseline: Some(GitBaselineSource::Rev {
            label: "v1.0".into(),
            commit: "9f8e7d6".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        to_value(&against_rev).unwrap()["baseline"],
        json!({"kind": "rev", "label": "v1.0", "commit": "9f8e7d6"})
    );
    let against_saved = GitBufferStatus {
        baseline: Some(GitBaselineSource::Saved),
        ..Default::default()
    };
    assert_eq!(
        to_value(&against_saved).unwrap()["baseline"],
        json!({"kind": "saved"})
    );

    // The refusal that comes with it.
    assert_eq!(
        to_value(ApplyHunkStatus::NotAgainstHead).unwrap(),
        json!("not_against_head")
    );
}

#[test]
fn git_refresh_shapes() {
    assert_eq!(GitRefresh::NAME, "git/refresh");
    assert_eq!(
        to_value(&GitRefreshParams {
            repo_id: "/src/aether".into()
        })
        .unwrap(),
        json!({"repo_id": "/src/aether"})
    );

    // The three outcomes a caller reports to the user in one message.
    let res = GitRefreshResult {
        reloaded: vec![1, 2],
        diverged: vec![3],
        missing: vec![4],
    };
    let v = to_value(&res).unwrap();
    assert_eq!(
        v,
        json!({"reloaded": [1, 2], "diverged": [3], "missing": [4]})
    );
    assert_eq!(from_value::<GitRefreshResult>(v).unwrap(), res);

    // "Nothing moved" is the common answer and costs no bytes.
    assert_eq!(to_value(GitRefreshResult::default()).unwrap(), json!({}));
    assert_eq!(
        from_value::<GitRefreshResult>(json!({})).unwrap(),
        GitRefreshResult::default()
    );
}

#[test]
fn git_apply_hunk_roundtrip() {
    let p = GitApplyHunkParams {
        scope: Default::default(),
        buffer_id: 4,
        action: HunkAction::Stage,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"buffer_id": 4, "action": "stage"})
    );
    assert_eq!(GitApplyHunk::NAME, "git/apply_hunk");

    // Each direction is its own word on the wire: the client picks one per key, and the server
    // never resolves a direction of its own.
    for (action, wire) in [
        (HunkAction::Stage, "stage"),
        (HunkAction::Unstage, "unstage"),
        (HunkAction::Revert, "revert"),
    ] {
        let p = GitApplyHunkParams {
            scope: ApplyScope::File,
            buffer_id: 4,
            action,
        };
        assert_eq!(
            to_value(&p).unwrap(),
            json!({"buffer_id": 4, "action": wire, "scope": "file"})
        );
        let back: GitApplyHunkParams = from_value(to_value(&p).unwrap()).unwrap();
        assert_eq!(back.action, action);
        assert_eq!(back.scope, ApplyScope::File);
    }

    // The status reports what the action did.
    for (status, wire) in [
        (ApplyHunkStatus::Staged, "staged"),
        (ApplyHunkStatus::Unstaged, "unstaged"),
        (ApplyHunkStatus::Reverted, "reverted"),
        (ApplyHunkStatus::DirtyBuffer, "dirty_buffer"),
        // Distinct from `unavailable` on the wire as well as in meaning: the client words that one
        // as "not in a git repository", which the patch view's own refusals must never claim.
        (ApplyHunkStatus::NeedsFile, "needs_file"),
        (ApplyHunkStatus::Unavailable, "unavailable"),
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

/// `staged` is the newest field on the oldest stash call, so both directions matter: it must stay
/// off the wire when false (an older server would reject an unknown field), and a message from a
/// client that has never heard of it must still deserialize.
#[test]
fn git_stash_push_staged_flag_shape() {
    assert_eq!(GitStashPush::NAME, "git/stash_push");
    let plain = GitStashPushParams {
        repo_id: None,
        buffer_id: Some(3),
        message: None,
        staged: false,
    };
    assert_eq!(to_value(&plain).unwrap(), json!({"buffer_id": 3}));

    let staged = GitStashPushParams {
        repo_id: None,
        buffer_id: Some(3),
        message: None,
        staged: true,
    };
    assert_eq!(
        to_value(&staged).unwrap(),
        json!({"buffer_id": 3, "staged": true})
    );

    // Absent reads as false — the whole-tree stash every older client sends.
    let back: GitStashPushParams = from_value(json!({"buffer_id": 3})).unwrap();
    assert!(!back.staged);

    // The refusal for a git too old to have the flag is its own status, not a generic failure.
    let res = GitStashResult {
        status: GitStashStatus::StagedUnsupported,
        ..Default::default()
    };
    assert_eq!(to_value(&res).unwrap()["status"], "staged_unsupported");
    let back: GitStashResult = from_value(to_value(&res).unwrap()).unwrap();
    assert_eq!(back.status, GitStashStatus::StagedUnsupported);
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
fn logical_line_render_baseline_rows_shape() {
    // `baseline_above` is omitted when empty — it rides every rendered line.
    let bare = LogicalLineRender {
        logical_line: 0,
        visual_rows: vec![],
        search_matches: vec![],
        baseline_above: vec![],
        change: Default::default(),
        diagnostics: vec![],
        sneak_targets: vec![],
    };
    let v = to_value(&bare).unwrap();
    assert!(v.get("baseline_above").is_none(), "empty omitted from wire");
    assert!(
        v.get("sneak_targets").is_none(),
        "empty sneak_targets omitted from wire"
    );
    assert!(
        v.get("change").is_none(),
        "an unchanged line omits its change-state entirely — one absent field where there were five"
    );
    assert!(
        v.get("diagnostics").is_none(),
        "empty diagnostics omitted from wire"
    );
    assert!(
        v.get("conflict").is_none(),
        "no conflict omitted from wire — every line of every unconflicted file"
    );
    assert!(
        v.get("patch").is_none(),
        "no patch side omitted from wire — every line of every buffer that isn't a patch"
    );

    let with_del = LogicalLineRender {
        logical_line: 4,
        visual_rows: vec![],
        search_matches: vec![],
        baseline_above: vec![BaselineRow {
            text: "old line".into(),
            stage: DiffStage::Staged,
            emphasis: vec![EmphasisRange { start: 4, end: 8 }],
        }],
        change: LineChange::Changed {
            marker: DiffMarker::Modified,
            stage: DiffStage::Staged,
            emphasis: vec![EmphasisRange { start: 0, end: 3 }],
        },
        diagnostics: vec![DiagnosticSpan {
            start: 4,
            end: 9,
            severity: DiagnosticSeverity::Error,
            message: "unused variable".into(),
        }],
        sneak_targets: vec![],
    };
    let v = to_value(&with_del).unwrap();
    assert_eq!(v["baseline_above"][0]["text"], "old line");
    assert_eq!(v["baseline_above"][0]["stage"], "staged");
    assert_eq!(v["baseline_above"][0]["emphasis"][0]["start"], 4);
    assert_eq!(v["baseline_above"][0]["emphasis"][0]["end"], 8);
    assert!(
        v["baseline_above"][0].get("highlights").is_none(),
        "a baseline row carries no spans — omitted rather than sent empty"
    );
    // The five fields that used to sit side by side here are one tagged value now.
    assert_eq!(v["change"]["kind"], "changed");
    assert_eq!(v["change"]["marker"], "modified");
    assert_eq!(v["change"]["stage"], "staged");
    assert_eq!(v["change"]["emphasis"][0]["start"], 0);
    assert_eq!(v["change"]["emphasis"][0]["end"], 3);
    assert_eq!(v["diagnostics"][0]["start"], 4);
    assert_eq!(v["diagnostics"][0]["end"], 9);
    assert_eq!(v["diagnostics"][0]["severity"], "error");
    assert_eq!(v["diagnostics"][0]["message"], "unused variable");
    let back: LogicalLineRender = from_value(v).unwrap();
    assert_eq!(back.baseline_above.len(), 1);
    assert_eq!(
        back.baseline_above[0],
        BaselineRow {
            text: "old line".into(),
            stage: DiffStage::Staged,
            emphasis: vec![EmphasisRange { start: 4, end: 8 }],
        }
    );
    assert_eq!(
        back.change,
        LineChange::Changed {
            marker: DiffMarker::Modified,
            stage: DiffStage::Staged,
            emphasis: vec![EmphasisRange { start: 0, end: 3 }],
        }
    );
    assert_eq!(back.diagnostics[0].severity, DiagnosticSeverity::Error);
}

#[test]
fn logical_line_render_conflict_shape() {
    use aether_protocol::viewport::ConflictLine;
    // The four sides are snake_case on the wire. A conflicted line carrying no diff marker is no
    // longer something to assert — the blocks are masked out of the file's diff, and `LineChange`
    // now makes that structural rather than a rule two fields had to be trusted to obey.
    let line = |side| LogicalLineRender {
        logical_line: 3,
        visual_rows: vec![],
        search_matches: vec![],
        baseline_above: vec![],
        change: LineChange::Conflict { side },
        diagnostics: vec![],
        sneak_targets: vec![],
    };
    for (side, wire) in [
        (ConflictLine::Marker, "marker"),
        (ConflictLine::Ours, "ours"),
        (ConflictLine::Base, "base"),
        (ConflictLine::Theirs, "theirs"),
    ] {
        let v = to_value(line(side)).unwrap();
        assert_eq!(v["change"]["kind"], "conflict");
        assert_eq!(v["change"]["side"], wire);
        // The variants are exclusive by construction, so there is no marker to omit.
        assert!(v["change"].get("marker").is_none());
        let back: LogicalLineRender = from_value(v).unwrap();
        assert_eq!(back.change.conflict(), Some(side));
    }
}

#[test]
fn logical_line_render_patch_shape() {
    use aether_protocol::viewport::PatchLine;
    // The two sides are snake_case on the wire. That a patch line carries no diff marker is now a
    // property of the type rather than an assertion: a generated patch has no baseline of its own,
    // and `LineChange` has no variant that could express both.
    let line = |side| LogicalLineRender {
        logical_line: 7,
        visual_rows: vec![],
        search_matches: vec![],
        baseline_above: vec![],
        change: LineChange::Patch {
            side,
            stage: DiffStage::Unstaged,
            emphasis: vec![],
        },
        diagnostics: vec![],
        sneak_targets: vec![],
    };
    for (side, wire) in [(PatchLine::Added, "added"), (PatchLine::Removed, "removed")] {
        let v = to_value(line(side)).unwrap();
        assert_eq!(v["change"]["kind"], "patch");
        assert_eq!(v["change"]["side"], wire);
        assert!(v["change"].get("marker").is_none());
        let back: LogicalLineRender = from_value(v).unwrap();
        assert_eq!(back.change.patch_side(), Some(side));
    }
}

#[test]
fn patch_chrome_virtual_row_shape() {
    use aether_protocol::viewport::Highlight;
    // A generated patch's separators ride the same channel as the inline diff's phantom deleted
    // rows, so that chrome costs no buffer lines and therefore no cursor positions. Unlike a
    // deleted row they carry spans, which is what lets a path be styled apart from its counts.
    //
    // A section heading is the enclosing signature alone — git's `@@ -a,b +c,d @@` ranges are
    // dropped, so a patch shows no line numbers anywhere.
    let row = Element::Chrome {
        kind: ChromeKind::HunkHeader,
        rail: RailJoin::Tees,
        children: vec![
            Element::space(1),
            Element::text(
                "fn render_window(",
                vec![Highlight {
                    start: 0,
                    end: 17,
                    kind: "diff.hunk".into(),
                }],
            ),
        ],
    };
    let v = to_value(&row).unwrap();
    assert_eq!(v["node"], "chrome", "the variant tag");
    assert_eq!(v["kind"], "hunk_header");
    assert_eq!(v["rail"], "tees");
    assert!(
        v.get("stage").is_none(),
        "stage is a deletion's business; chrome has no field for it to be meaningless in"
    );
    // The content: chrome's children, laid out left to right, each self-describing under the one
    // `node` tag the whole vocabulary shares. It used to nest a `layout`/`widget` pair inside a
    // `content` object — two enums for one idea, and an editor could not appear among them.
    let children = v["children"].as_array().unwrap();
    assert_eq!(children[0]["node"], "space");
    assert_eq!(children[0]["cols"], 1);
    assert_eq!(children[1]["node"], "text");
    assert_eq!(children[1]["highlights"][0]["kind"], "diff.hunk");
    assert!(
        v.get("content").is_none(),
        "no wrapper object between chrome and what it draws"
    );
    let back: Element = from_value(v).unwrap();
    assert!(matches!(
        back,
        Element::Chrome {
            kind: ChromeKind::HunkHeader,
            ..
        }
    ));
    assert_eq!(back, row);

    // A baseline row is a different type entirely, not a sibling variant: chrome belongs to the
    // view's tree, a phantom deletion belongs to the line it stands above.
    let del = BaselineRow {
        text: "gone".into(),
        stage: DiffStage::Unstaged,
        emphasis: vec![],
    };
    let v = to_value(&del).unwrap();
    assert_eq!(v["text"], "gone");
    assert!(v.get("kind").is_none() && v.get("rail").is_none());
    assert_eq!(from_value::<BaselineRow>(v).unwrap(), del);

    for (kind, wire) in [
        (ChromeKind::FileHeader, "file_header"),
        (ChromeKind::HunkHeader, "hunk_header"),
        (ChromeKind::Rule, "rule"),
        (ChromeKind::Spacer, "spacer"),
        (ChromeKind::Summary, "summary"),
    ] {
        let v = to_value(Element::Chrome {
            kind,
            rail: RailJoin::Opens,
            children: vec![Element::fill('─')],
        })
        .unwrap();
        assert_eq!(v["kind"], wire);
        assert_eq!(v["children"][0]["node"], "fill");
        let back: Element = from_value(v).unwrap();
        assert!(matches!(back, Element::Chrome { kind: k, .. } if k == kind));
    }

    for (rail, wire) in [
        (RailJoin::Opens, "opens"),
        (RailJoin::Tees, "tees"),
        (RailJoin::Closes, "closes"),
        (RailJoin::Detached, "detached"),
    ] {
        let v = to_value(Element::Chrome {
            kind: ChromeKind::Rule,
            rail,
            children: vec![],
        })
        .unwrap();
        assert_eq!(v["rail"], wire);
    }
}

#[test]
fn buffer_status_snapshot_shape() {
    use aether_protocol::lsp::{DiagnosticCounts, LspServerStatus, LspStatus, SymbolCrumb};
    use aether_protocol::picker::SymbolKind;

    // A clean, unbacked buffer: flags false, empty diagnostics and no LSP status drop off the wire.
    let empty = BufferStatusSnapshot::default();
    let v = to_value(&empty).unwrap();
    assert_eq!(v["externally_modified"], false);
    assert_eq!(v["externally_deleted"], false);
    assert!(v.get("diagnostics").is_none(), "empty counts omitted");
    assert!(v.get("lsp_status").is_none(), "no server → omitted");
    assert!(v.get("symbol_path").is_none(), "empty breadcrumb omitted");

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
        symbol_path: vec![
            SymbolCrumb {
                name: "impl Foo".into(),
                kind: SymbolKind::Class,
            },
            SymbolCrumb {
                name: "fn bar".into(),
                kind: SymbolKind::Method,
            },
        ],
    };
    let v = to_value(&full).unwrap();
    assert_eq!(v["externally_modified"], true);
    assert_eq!(v["diagnostics"]["errors"], 2);
    assert_eq!(v["lsp_status"]["name"], "rust-analyzer");
    // Outermost first, and the kind rides each crumb as a snake_case tag.
    assert_eq!(v["symbol_path"][0]["name"], "impl Foo");
    assert_eq!(v["symbol_path"][1]["name"], "fn bar");
    assert_eq!(v["symbol_path"][1]["kind"], "method");
    let back: BufferStatusSnapshot = from_value(v).unwrap();
    assert!(back.externally_modified);
    assert_eq!(back.diagnostics.errors, 2);
    assert_eq!(back.symbol_path.len(), 2);
    assert_eq!(back.lsp_status.unwrap().language, "rust");

    // Absent on the wire (older server) → defaults, so deserialization never fails.
    let bare: BufferStatusSnapshot = from_value(json!({})).unwrap();
    assert!(bare.diagnostics.is_empty() && bare.lsp_status.is_none());
    assert!(bare.symbol_path.is_empty());
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
    use aether_protocol::git::{GitBufferStatus, GitUpstreamStatus};
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
        upstream: None,
        baseline: None,
        conflicts: 0,
        operation: None,
        worktree: false,
    };
    let v = to_value(&s).unwrap();
    assert_eq!(v["branch"], "main");
    // Absent upstream stays absent: "no upstream to compare with" and "level with upstream" are
    // different answers, and a client that can't tell them apart would report a never-pushed
    // branch as in sync.
    assert!(v.get("upstream").is_none());
    assert!(
        v.get("conflicts").is_none(),
        "zero conflicts omitted — the state every unconflicted file is in"
    );
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

    // Divergence rides along when there *is* an upstream, named so a fork workflow can tell
    // `origin/main` from `upstream/main`. Zeros are carried, not omitted: level is a real answer.
    let diverged = GitBufferStatus {
        branch: Some("main".into()),
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 2,
            behind: 5,
        }),
        ..Default::default()
    };
    let v = to_value(&diverged).unwrap();
    assert_eq!(
        v["upstream"],
        json!({"name": "origin/main", "ahead": 2, "behind": 5})
    );
    let back: GitBufferStatus = from_value(v).unwrap();
    let up = back.upstream.expect("upstream survives the round trip");
    assert_eq!((up.ahead, up.behind), (2, 5));
    assert!(!up.is_level());
    assert!(GitUpstreamStatus {
        name: "origin/main".into(),
        ahead: 0,
        behind: 0,
    }
    .is_level());

    // Mid-rebase: the operation and the file's own outstanding conflict count travel together.
    // They answer different questions — the repo is stopped, *this file* still has three blocks —
    // and the status bar shows both.
    let conflicted = GitBufferStatus {
        branch: Some("main".into()),
        conflicts: 3,
        operation: Some(aether_protocol::git::GitRepoOperation::Rebase),
        ..Default::default()
    };
    let v = to_value(&conflicted).unwrap();
    assert_eq!(v["conflicts"], 3);
    assert_eq!(v["operation"], "rebase");
    let back: GitBufferStatus = from_value(v).unwrap();
    assert_eq!(back.conflicts, 3);
}

#[test]
fn git_abort_and_conclude_shapes() {
    use aether_protocol::git::{
        GitAbortOperation, GitAbortOperationParams, GitAbortOperationResult, GitAbortStatus,
        GitCommitResult, GitPrepareCommitResult, GitRepoOperation,
    };
    assert_eq!(GitAbortOperation::NAME, "git/abort_operation");
    // Both hints omitted: the server resolves the repo from the buffer the user is on.
    assert_eq!(
        to_value(GitAbortOperationParams::default()).unwrap(),
        json!({})
    );
    let aborted = GitAbortOperationResult {
        status: GitAbortStatus::Aborted,
        operation: Some(GitRepoOperation::Rebase),
        ..Default::default()
    };
    let v = to_value(&aborted).unwrap();
    assert_eq!(v, json!({"status": "aborted", "operation": "rebase"}));
    let back: GitAbortOperationResult = from_value(v).unwrap();
    assert_eq!(back.status, GitAbortStatus::Aborted);

    // A commit that concluded a rebase which then stopped again: success and a to-do list at once,
    // which is the shape the client's toast branches on.
    let continued = GitCommitResult {
        operation: Some(GitRepoOperation::Rebase),
        conflicts: vec!["a.rs".into()],
        ..Default::default()
    };
    let v = to_value(&continued).unwrap();
    assert_eq!(v["operation"], "rebase");
    assert_eq!(v["conflicts"], json!(["a.rs"]));
    // An ordinary commit carries neither.
    let plain = to_value(GitCommitResult::default()).unwrap();
    assert!(plain.get("operation").is_none() && plain.get("conflicts").is_none());

    // Nothing prepared when conflicts remain: the path is absent, so there is no buffer to open.
    let blocked = GitPrepareCommitResult {
        repo_id: "/repo".into(),
        blocked_by_conflicts: vec!["a.rs".into()],
        ..Default::default()
    };
    let v = to_value(&blocked).unwrap();
    assert_eq!(
        v,
        json!({"repo_id": "/repo", "blocked_by_conflicts": ["a.rs"]})
    );
}

#[test]
fn git_resolve_conflict_shapes() {
    use aether_protocol::git::{
        ConflictSide, GitResolveConflict, GitResolveConflictParams, GitResolveConflictResult,
        ResolveConflictStatus,
    };
    let p = GitResolveConflictParams {
        buffer_id: 4,
        side: ConflictSide::Theirs,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"buffer_id": 4, "side": "theirs"})
    );
    assert_eq!(GitResolveConflict::NAME, "git/resolve_conflict");

    // Both counts ride the result: what just happened, and what is left to do. Zero is omitted, so
    // "nothing left" is an absent field — which is the state that unlocks marking the file
    // resolved, and the client's `remaining == 0` branch has to survive that round trip.
    let done = GitResolveConflictResult {
        cursor: CursorState::default(),
        status: ResolveConflictStatus::Resolved,
        resolved: 2,
        remaining: 0,
    };
    let v = to_value(&done).unwrap();
    assert_eq!(v["status"], "resolved");
    assert_eq!(v["resolved"], 2);
    assert!(v.get("remaining").is_none());
    let back: GitResolveConflictResult = from_value(v).unwrap();
    assert_eq!(back.remaining, 0);
    assert_eq!(back.status, ResolveConflictStatus::Resolved);

    for (side, wire) in [
        (ConflictSide::Ours, "ours"),
        (ConflictSide::Theirs, "theirs"),
        (ConflictSide::Both, "both"),
    ] {
        assert_eq!(to_value(side).unwrap(), json!(wire));
    }
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

    let m = Motion::SelectionEdge {
        edge: SelectionEdge::AfterEnd,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(v, json!({"kind": "selection_edge", "edge": "after_end"}));

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
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "word", "direction": "forward", "count": 2, "boundary": "WORD"})
    );

    let m = Motion::Goto {
        position: LogicalPosition { line: 17, col: 4 },
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "goto", "position": {"line": 17, "col": 4}})
    );

    // The two vertical-row motions are separate variants on the wire, and deliberately so: their
    // `count` fields mean different things (typed rows vs. pages) and the server reads them under
    // different rules. A single variant with a synthesised count is the bug this split fixed.
    let m = Motion::VisualLine {
        viewport_id: 7,
        direction: VerticalDirection::Down,
        count: 100,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "visual_line", "viewport_id": 7, "direction": "down", "count": 100})
    );

    let m = Motion::Page {
        viewport_id: 7,
        direction: VerticalDirection::Up,
        count: 2,
        half: true,
    };
    let v = to_value(&m).unwrap();
    assert_eq!(
        v,
        json!({"kind": "page", "viewport_id": 7, "direction": "up", "count": 2, "half": true})
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
fn cursor_select_word_params_shape() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(CursorSelectWord::NAME, "element/select_word");

    // count == 1 (the default) is omitted on the wire.
    let v = to_value(CursorSelectWordParams {
        buffer_id: 3,
        boundary: WordBoundary::Word,
        extend: true,
        count: 1,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 3, "boundary": "word", "extend": true})
    );

    // A non-default count rides along; BigWord serialises as "WORD".
    let v = to_value(CursorSelectWordParams {
        buffer_id: 3,
        boundary: WordBoundary::BigWord,
        extend: false,
        count: 4,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 3, "boundary": "WORD", "extend": false, "count": 4})
    );

    // Omitted count defaults to 1.
    let p: CursorSelectWordParams = from_value(json!({
        "buffer_id": 3,
        "boundary": "word",
        "extend": false,
    }))
    .unwrap();
    assert_eq!(p.count, 1);
}

#[test]
fn cursor_set_params_granularity() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(CursorSet::NAME, "element/set");

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

    // Full shape: anchor present, extend set. Default `options` is skipped on the wire.
    let v = to_value(SearchSetParams {
        buffer_id: 3,
        query: "foo".into(),
        anchor: Some(LogicalPosition { line: 2, col: 5 }),
        extend: true,
        from_selection: false,
        options: MatchOptions::default(),
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "buffer_id": 3,
            "query": "foo",
            "anchor": {"line": 2, "col": 5},
            "extend": true,
            "from_selection": false,
        })
    );

    // Non-default options serialize as a nested object (case skipped when smart).
    let v = to_value(SearchSetParams {
        buffer_id: 3,
        query: "foo".into(),
        anchor: None,
        extend: false,
        from_selection: false,
        options: MatchOptions {
            case: CaseMode::Sensitive,
            whole_word: true,
            regex: false,
        },
    })
    .unwrap();
    assert_eq!(
        v["options"],
        json!({"case": "sensitive", "whole_word": true})
    );

    // `extend` defaults to false and `options` to all-default when omitted on the wire
    // (back-compat with older clients).
    let p: SearchSetParams =
        from_value(json!({"buffer_id": 3, "query": "foo", "anchor": null})).unwrap();
    assert!(!p.extend);
    assert!(p.anchor.is_none());
    assert_eq!(p.options, MatchOptions::default());
}

#[test]
fn sneak_params_and_result() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(SneakUpdate::NAME, "sneak/update");
    assert_eq!(SneakSelect::NAME, "sneak/select");
    assert_eq!(SneakCancel::NAME, "sneak/cancel");

    let v = to_value(SneakUpdateParams {
        buffer_id: 3,
        viewport_id: 9,
        query: "fu".into(),
        first_line: 25,
        last_line: 60,
        big: false,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 3, "viewport_id": 9, "query": "fu", "first_line": 25, "last_line": 60})
    );
    let big = to_value(SneakUpdateParams {
        buffer_id: 3,
        viewport_id: 9,
        query: "fu".into(),
        first_line: 0,
        last_line: 40,
        big: true,
    })
    .unwrap();
    assert_eq!(big["big"], json!(true));

    // Labels are chars on the wire; empty label set is omitted.
    let v = to_value(SneakUpdateResult {
        labels: vec!['j', 'k'],
        match_count: 2,
    })
    .unwrap();
    assert_eq!(v, json!({"labels": ["j", "k"], "match_count": 2}));
    let deferred = to_value(SneakUpdateResult {
        labels: vec![],
        match_count: 40,
    })
    .unwrap();
    assert!(deferred.get("labels").is_none(), "empty labels omitted");
    assert_eq!(deferred["match_count"], 40);

    // `extend` defaults to false and is omitted.
    let v = to_value(SneakSelectParams {
        buffer_id: 3,
        label: 'j',
        extend: false,
    })
    .unwrap();
    assert_eq!(v, json!({"buffer_id": 3, "label": "j"}));
    let p: SneakSelectParams = from_value(json!({"buffer_id": 3, "label": "k"})).unwrap();
    assert!(!p.extend);
    assert_eq!(p.label, 'k');
    // `extend` serializes when set.
    let v = to_value(SneakSelectParams {
        buffer_id: 3,
        label: 'k',
        extend: true,
    })
    .unwrap();
    assert_eq!(v["extend"], json!(true));
}

#[test]
fn sneak_target_shape() {
    // Labelled target carries the char and a chip spanning the typed prefix.
    let v = to_value(SneakTarget {
        start: 4,
        end: 11,
        prefix_end: 6,
        label: Some('j'),
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"start": 4, "end": 11, "prefix_end": 6, "label": "j"})
    );
    // Unlabelled (deferred): no label, empty chip (prefix_end == start).
    let v = to_value(SneakTarget {
        start: 0,
        end: 3,
        prefix_end: 0,
        label: None,
    })
    .unwrap();
    assert!(v.get("label").is_none(), "None label omitted from wire");
    let back: SneakTarget = from_value(json!({"start": 0, "end": 3, "prefix_end": 0})).unwrap();
    assert_eq!(back.label, None);
    assert_eq!(back.prefix_end, 0);
}

#[test]
fn input_text_params() {
    // `replace_selection: false` is the default and stays off the wire — the typing path's
    // shape is unchanged.
    let v = to_value(InputTextParams {
        buffer_id: 1,
        text: "hi".into(),
        select_pasted: false,
        replace_selection: false,
        at: None,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 1, "text": "hi", "select_pasted": false})
    );

    // The paste-replace path (`Ctrl-Alt-v`) sets it; it rides the wire and round-trips.
    let v = to_value(InputTextParams {
        buffer_id: 1,
        text: "hi".into(),
        select_pasted: true,
        replace_selection: true,
        at: None,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 1, "text": "hi", "select_pasted": true, "replace_selection": true})
    );
    let back: InputTextParams = from_value(v).unwrap();
    assert!(back.replace_selection);

    // Omitted on the wire → defaults to false (older clients).
    let defaulted: InputTextParams =
        from_value(json!({"buffer_id": 1, "text": "hi", "select_pasted": false})).unwrap();
    assert!(!defaulted.replace_selection);
}

#[test]
fn input_newline_and_indent_params_shape() {
    // Enter's caret insert: `park_before: false` stays off the wire, so the typing path's shape
    // is the bare buffer-only one.
    let v = to_value(InputNewlineAndIndentParams {
        buffer_id: 1,
        park_before: false,
    })
    .unwrap();
    assert_eq!(v, json!({"buffer_id": 1}));

    // The un-join gesture sets it; it rides the wire and round-trips.
    let v = to_value(InputNewlineAndIndentParams {
        buffer_id: 1,
        park_before: true,
    })
    .unwrap();
    assert_eq!(v, json!({"buffer_id": 1, "park_before": true}));
    let back: InputNewlineAndIndentParams = from_value(v).unwrap();
    assert!(back.park_before);

    // Omitted on the wire → defaults to false.
    let defaulted: InputNewlineAndIndentParams = from_value(json!({"buffer_id": 1})).unwrap();
    assert!(!defaulted.park_before);
}

#[test]
fn input_surround_params() {
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::input::SurroundTarget;
    assert_eq!(InputSurround::NAME, "element/surround");

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
fn input_transform_case_params() {
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::input::{CaseKind, InputTransformCase, InputTransformCaseParams};
    assert_eq!(InputTransformCase::NAME, "element/transform_case");

    // `kind` serialises snake_case; `scan_at_cursor` is omitted when false (Normal mode),
    // matching `input/adjust_number`.
    let v = to_value(InputTransformCaseParams {
        buffer_id: 3,
        kind: CaseKind::Constant,
        scan_at_cursor: false,
    })
    .unwrap();
    assert_eq!(v, json!({"buffer_id": 3, "kind": "constant"}));

    let back: InputTransformCaseParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.buffer_id, 3);
    assert_eq!(back.kind, CaseKind::Constant);
    assert!(!back.scan_at_cursor);

    // Insert mode sets it; it rides the wire and round-trips.
    let v = to_value(InputTransformCaseParams {
        buffer_id: 3,
        kind: CaseKind::Upper,
        scan_at_cursor: true,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 3, "kind": "upper", "scan_at_cursor": true})
    );
    let back: InputTransformCaseParams = serde_json::from_value(v).unwrap();
    assert!(back.scan_at_cursor);

    // The mnemonic map is the single source of truth shared with the keymap.
    assert_eq!(CaseKind::from_char('i'), Some(CaseKind::Invert));
    assert_eq!(CaseKind::from_char('r'), Some(CaseKind::Reverse));
    assert_eq!(CaseKind::from_char('m'), Some(CaseKind::Randomize));
    assert_eq!(CaseKind::from_char('w'), Some(CaseKind::Words));
    assert_eq!(CaseKind::from_char('z'), None);
}

#[test]
fn toggle_comment_params() {
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::input::{
        CommentStyle, InputToggleComment, SurroundTarget, ToggleCommentParams,
    };
    assert_eq!(InputToggleComment::NAME, "element/toggle_comment");

    // `style` is required and snake_case; `target` defaults to `selection` and is emitted
    // when set (Insert mode sends `line`).
    let v = to_value(ToggleCommentParams {
        buffer_id: 4,
        style: CommentStyle::Line,
        target: SurroundTarget::Selection,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 4, "style": "line", "target": "selection"})
    );

    let v = to_value(ToggleCommentParams {
        buffer_id: 4,
        style: CommentStyle::Block,
        target: SurroundTarget::Line,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({"buffer_id": 4, "style": "block", "target": "line"})
    );
    let back: ToggleCommentParams = from_value(v).unwrap();
    assert_eq!(back.style, CommentStyle::Block);
    assert_eq!(back.target, SurroundTarget::Line);

    // `target` omitted on the wire → Selection.
    let defaulted: ToggleCommentParams =
        from_value(json!({"buffer_id": 1, "style": "line"})).unwrap();
    assert_eq!(defaulted.target, SurroundTarget::Selection);
}

/// `input/tab` carries no payload of its own — the indent step is computed server-side from the
/// buffer's style — so its params are the bare `BufferOnlyParams` it shares with its inverse,
/// `input/backspace`.
#[test]
fn input_tab_method() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(InputTab::NAME, "element/tab");
    assert_eq!(InputBackspace::NAME, "element/backspace");

    let params = to_value(BufferOnlyParams { buffer_id: 7 }).unwrap();
    assert_eq!(params, json!({"buffer_id": 7}));
    let parsed: BufferOnlyParams = from_value(json!({"buffer_id": 7})).unwrap();
    assert_eq!(parsed.buffer_id, 7);
}

/// `input/delete_word` — the Insert-mode `Alt-Backspace` / `Alt-Delete` edit. Direction and
/// boundary are always on the wire (there is no sensible default for either); `count` follows the
/// counted-edit convention and is omitted at 1.
#[test]
fn input_delete_word_method() {
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::input::{InputDeleteWord, InputDeleteWordParams};
    assert_eq!(InputDeleteWord::NAME, "element/delete_word");

    let back = to_value(InputDeleteWordParams {
        buffer_id: 5,
        direction: Direction::Backward,
        boundary: WordBoundary::Word,
        count: 1,
    })
    .unwrap();
    assert_eq!(
        back,
        json!({"buffer_id": 5, "direction": "backward", "boundary": "word"})
    );

    let fwd = to_value(InputDeleteWordParams {
        buffer_id: 5,
        direction: Direction::Forward,
        boundary: WordBoundary::BigWord,
        count: 3,
    })
    .unwrap();
    assert_eq!(
        fwd,
        json!({"buffer_id": 5, "direction": "forward", "boundary": "WORD", "count": 3})
    );

    let parsed: InputDeleteWordParams =
        from_value(json!({"buffer_id": 5, "direction": "backward", "boundary": "word"})).unwrap();
    assert_eq!(parsed.count, 1);
    assert_eq!(parsed.direction, Direction::Backward);
    assert_eq!(parsed.boundary, WordBoundary::Word);
}

#[test]
fn input_adjust_number_methods() {
    use aether_protocol::envelope::RpcMethod;
    assert_eq!(InputAdjustNumber::NAME, "element/adjust_number");

    // Signed delta rides on the wire both ways (increment is `+count`, decrement `-count`).
    // `scan_at_cursor` is omitted when false (Normal mode) and present in Insert mode.
    let inc = to_value(InputAdjustNumberParams {
        buffer_id: 3,
        delta: 1,
        scan_at_cursor: false,
    })
    .unwrap();
    assert_eq!(inc, json!({"buffer_id": 3, "delta": 1}));
    let dec = to_value(InputAdjustNumberParams {
        buffer_id: 3,
        delta: -4,
        scan_at_cursor: false,
    })
    .unwrap();
    assert_eq!(dec, json!({"buffer_id": 3, "delta": -4}));
    let scan = to_value(InputAdjustNumberParams {
        buffer_id: 3,
        delta: 1,
        scan_at_cursor: true,
    })
    .unwrap();
    assert_eq!(
        scan,
        json!({"buffer_id": 3, "delta": 1, "scan_at_cursor": true})
    );

    // The shared counted-edit shape omits `count` when it's 1, and carries it otherwise.
    let one = to_value(CountedEditParams {
        buffer_id: 3,
        count: 1,
    })
    .unwrap();
    assert_eq!(one, json!({"buffer_id": 3}));

    let many = to_value(CountedEditParams {
        buffer_id: 3,
        count: 4,
    })
    .unwrap();
    assert_eq!(many, json!({"buffer_id": 3, "count": 4}));

    // `count` defaults to 1 when omitted on the wire.
    let back: CountedEditParams = serde_json::from_value(json!({"buffer_id": 3})).unwrap();
    assert_eq!(back.count, 1);

    // Undo/redo carry `collapse_selection`; both it and a `count` of 1 are omitted when unset.
    let plain = to_value(UndoRedoParams {
        buffer_id: 3,
        count: 1,
        collapse_selection: false,
    })
    .unwrap();
    assert_eq!(plain, json!({"buffer_id": 3}));

    let collapsing = to_value(UndoRedoParams {
        buffer_id: 3,
        count: 2,
        collapse_selection: true,
    })
    .unwrap();
    assert_eq!(
        collapsing,
        json!({"buffer_id": 3, "count": 2, "collapse_selection": true})
    );

    // Both fields default when omitted on the wire.
    let back: UndoRedoParams = serde_json::from_value(json!({"buffer_id": 3})).unwrap();
    assert_eq!(back.count, 1);
    assert!(!back.collapse_selection);
}

#[test]
fn buffer_open_result_shape() {
    let v = to_value(ViewOpenResult {
        transient: false,
        view_id: aether_protocol::ViewId(42),
        scroll: None,
        buffer: BufferDescription {
            buffer_id: 42,
            language: Some("rust".into()),
            line_count: 100,
            byte_count: 1234,
            revision: 0,
            saved_revision: 0,
            path: None,
            scratch_number: Some(3),
            cursor: Default::default(),
            lsp_server: Some(aether_protocol::lsp::LspServerRef {
                language: "rust".into(),
                workspace_root: "/proj".into(),
            }),
            title: None,
            read_only: false,
            is_patch: false,
        },
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

/// An open answers with the view it presented — always, since a client subscribes to it — and
/// a result from before views were reported reads as view 0, which no view ever is.
#[test]
fn buffer_open_result_reports_its_view() {
    let v = to_value(ViewOpenResult {
        transient: false,
        view_id: aether_protocol::ViewId(7),
        scroll: None,
        buffer: BufferDescription {
            buffer_id: 42,
            language: None,
            line_count: 1,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: None,
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            read_only: false,
            is_patch: false,
        },
    })
    .unwrap();
    assert_eq!(v["view_id"], 7);
    let back: ViewOpenResult = from_value(json!({
        "buffer_id": 42, "line_count": 1, "byte_count": 0, "revision": 0, "saved_revision": 0,
        "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
    }))
    .unwrap();
    assert_eq!(back.view_id, aether_protocol::ViewId(0));
}

/// What an open may ask for beyond a file: a view outright, one of its elements, and a kind of
/// view for a markdown file. All off the wire unless asked — and there is no `buffer_id`: the
/// wire names views.
#[test]
fn view_open_params_carry_a_view_an_element_and_a_kind() {
    let plain = ViewOpenParams {
        path_index: Some(0),
        relative_path: Some("a.md".into()),
        ..Default::default()
    };
    let v = to_value(&plain).unwrap();
    assert!(v.get("view_id").is_none());
    assert!(v.get("element").is_none());
    assert!(v.get("kind").is_none());
    assert!(v.get("buffer_id").is_none());
    let asked = ViewOpenParams {
        view_id: Some(aether_protocol::ViewId(9)),
        element: Some(2),
        kind: Some(aether_protocol::ui::ViewKind::Reader),
        ..Default::default()
    };
    let v = to_value(&asked).unwrap();
    assert_eq!(v["view_id"], 9);
    assert_eq!(v["element"], 2);
    assert_eq!(v["kind"], "reader");
    let back: ViewOpenParams = from_value(json!({"kind": "editor"})).unwrap();
    assert_eq!(back.kind, Some(aether_protocol::ui::ViewKind::Editor));
    assert_eq!(back.view_id, None);
    assert_eq!(back.element, None);
}

#[test]
fn buffer_open_result_restored_scroll() {
    use aether_protocol::viewport::ScrollPosition;
    let v = to_value(ViewOpenResult {
        transient: false,
        view_id: aether_protocol::ViewId(42),
        scroll: Some(ScrollPosition {
            element: 2,
            line: 7,
            sub_row: 0.5,
        }),
        buffer: BufferDescription {
            buffer_id: 42,
            language: None,
            line_count: 1,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: None,
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            read_only: false,
            is_patch: false,
        },
    })
    .unwrap();
    // Content, not a row: the element the viewport's top was in, the line of that element's
    // buffer, and how far into the line's rows it sat.
    assert_eq!(v["scroll"]["element"], 2);
    assert_eq!(v["scroll"]["line"], 7);
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
            message: "path outside workspace".into(),
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
    assert_eq!(WorkspaceList::NAME, "workspace/list");
    assert_eq!(WorkspaceActivate::NAME, "workspace/activate");
    assert_eq!(ViewOpen::NAME, "view/open");
    assert_eq!(CursorMove::NAME, "element/move");
    assert_eq!(InputText::NAME, "element/text");
    assert_eq!(ViewportLinesChanged::NAME, "view/lines_changed");
    assert_eq!(DirectoryList::NAME, "directory/list");
}

#[test]
fn directory_list_params_shape() {
    // The bounded default is the whole-message shape: `unrestricted` is skipped when clear, so
    // every existing caller's wire bytes are unchanged by the flag's existence.
    let v = to_value(DirectoryListParams {
        path: "/home/foo/proj/src".into(),
        unrestricted: false,
    })
    .unwrap();
    assert_eq!(v, json!({ "path": "/home/foo/proj/src" }));

    let v = to_value(DirectoryListParams {
        path: "~/code".into(),
        unrestricted: true,
    })
    .unwrap();
    assert_eq!(v, json!({ "path": "~/code", "unrestricted": true }));

    // And an old client's message still parses — the flag defaults to the bounded mode rather
    // than to the permissive one.
    let back: DirectoryListParams =
        serde_json::from_value(json!({ "path": "/home/foo/proj/src" })).unwrap();
    assert!(!back.unrestricted);
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
fn viewport_lines_changed_params_cursor_shape() {
    let base = ViewportLinesChangedParams {
        buffer: 7,
        viewport_id: 7,
        revision: 42,
        window: sample_window(),
        cursor: None,
    };
    // Without a cursor the field is absent on the wire, and absent deserializes to `None` —
    // older servers/clients interoperate.
    let v = to_value(&base).unwrap();
    assert!(
        v.get("cursor").is_none(),
        "cursor: None should be skipped on the wire"
    );
    let back: ViewportLinesChangedParams = from_value(v).unwrap();
    assert!(back.cursor.is_none());

    let with_cursor = ViewportLinesChangedParams {
        cursor: Some(CursorState {
            position: LogicalPosition { line: 5, col: 3 },
            anchor: LogicalPosition { line: 5, col: 3 },
            match_bracket: None,
            jumplist_position: None,
        }),
        ..base
    };
    let v = to_value(&with_cursor).unwrap();
    assert_eq!(
        v["cursor"],
        json!({
            "position": {"line": 5, "col": 3},
            "anchor": {"line": 5, "col": 3},
        })
    );
    let back: ViewportLinesChangedParams = from_value(v).unwrap();
    assert_eq!(
        back.cursor.unwrap().position,
        LogicalPosition { line: 5, col: 3 }
    );
}

#[test]
fn notification_roundtrip() {
    let n = Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: json!({"viewport_id": 1, "buffer": 3, "revision": 5, "window": {"root": {"node": "editor", "element": 0, "buffer": 3, "rows": 0, "first_row": 0, "first_buffer_line": 0, "lines": []}, "max_line_width": 0}}),
    };
    let s = serde_json::to_string(&n).unwrap();
    let v: serde_json::Value = from_str(&s).unwrap();
    assert_eq!(v["method"], "view/lines_changed");
    assert!(v.get("id").is_none(), "notifications carry no id");
}

#[test]
fn lsp_method_names() {
    assert_eq!(LspRestartServer::NAME, "lsp/restart_server");
    assert_eq!(LspStatusChanged::NAME, "lsp/status_changed");
}

#[test]
fn lsp_document_highlight_shape() {
    assert_eq!(LspDocumentHighlight::NAME, "lsp/document_highlight");
    // Cursor-relative + fire-and-forget: params carry the buffer and the set/clear flag.
    let v = to_value(LspDocumentHighlightParams {
        buffer_id: 7,
        active: true,
    })
    .unwrap();
    assert_eq!(v, json!({"buffer_id": 7, "active": true}));
    // Unit result → serializes to JSON null.
    assert_eq!(to_value(()).unwrap(), json!(null));
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
    // Hover: optional contents + the markdown-kind flag + the readiness of the server that answered.
    let v = to_value(LspHoverResult {
        contents: Some("fn x()".into()),
        markdown: true,
        readiness: LspReadiness::Ready,
    })
    .unwrap();
    assert_eq!(v["contents"], "fn x()");
    assert_eq!(v["markdown"], true);
    assert_eq!(v["readiness"], "ready");
    // `markdown`/`readiness` default when absent (no content → `Ready` so the client says "no info").
    let r: LspHoverResult = serde_json::from_value(json!({ "contents": null })).unwrap();
    assert!(!r.markdown);
    assert_eq!(r.readiness, LspReadiness::Ready);
    // Readiness is flat snake_case on the wire.
    assert_eq!(to_value(LspReadiness::Starting).unwrap(), json!("starting"));
    assert_eq!(
        to_value(LspReadiness::NoServer).unwrap(),
        json!("no_server")
    );
    assert_eq!(
        to_value(LspReadiness::Unavailable).unwrap(),
        json!("unavailable")
    );
    // Goto: optional location with absolute path + byte-col position, plus readiness.
    let r = LspGotoDefinitionResult {
        location: Some(LspLocation {
            path: "/p/src/lib.rs".into(),
            position: LogicalPosition { line: 12, col: 4 },
            end: LogicalPosition { line: 12, col: 9 },
        }),
        readiness: LspReadiness::Ready,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["location"]["path"], "/p/src/lib.rs");
    assert_eq!(v["location"]["position"]["line"], 12);
    assert_eq!(v["location"]["end"]["col"], 9);
    assert_eq!(v["readiness"], "ready");
    let back: LspGotoDefinitionResult = from_value(v).unwrap();
    let loc = back.location.unwrap();
    assert_eq!(loc.position.col, 4);
    assert_eq!(loc.end, LogicalPosition { line: 12, col: 9 });
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
fn viewport_subscribe_params_carry_sticky_diff_view() {
    use aether_protocol::viewport::{
        ScrollPosition, ViewportSubscribe, ViewportSubscribeParams, WrapMode,
    };
    assert_eq!(ViewportSubscribe::NAME, "view/subscribe");
    let p = ViewportSubscribeParams {
        view_id: aether_protocol::ViewId(1),
        cols: 80,
        rows: 24,
        overscan_rows: 0,
        scroll: ScrollPosition {
            element: 0,
            line: 0,
            sub_row: 0.0,
        },
        focus: None,
        wrap: WrapMode::None,
        continuation_marker_width: 0,
        tab_width: 4,
        diff_view: true,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["diff_view"], true);
    // A fresh open names no focus: the server takes it from the scroll's element.
    assert!(v.get("focus").is_none(), "focus: None stays off the wire");
    // The view it subscribes to is named as one.
    assert_eq!(v["view_id"], 1);
    assert!(
        v.get("buffer_id").is_none(),
        "a subscribe names a view, not a buffer"
    );
    // Absent on the wire → defaults off (older clients that don't send the sticky toggle).
    let back: ViewportSubscribeParams = from_value(json!({
        "view_id": 1, "cols": 80, "rows": 24, "overscan_rows": 0,
        "scroll": { "element": 0, "line": 0, "sub_row": 0.0 },
        "wrap": "none", "continuation_marker_width": 0, "tab_width": 4,
    }))
    .unwrap();
    assert!(!back.diff_view);
    assert!(back.focus.is_none());
    // A re-subscribe says which element already holds the cursor.
    let back: ViewportSubscribeParams = from_value(json!({
        "view_id": 1, "cols": 80, "rows": 24, "overscan_rows": 0,
        "scroll": { "element": 2, "line": 9, "sub_row": 0.0 }, "focus": 2,
        "wrap": "none", "continuation_marker_width": 0, "tab_width": 4,
    }))
    .unwrap();
    assert_eq!(back.focus, Some(2));
    assert_eq!((back.scroll.element, back.scroll.line), (2, 9));
}

#[test]
fn lsp_navigate_diagnostic_shape() {
    assert_eq!(LspNavigateDiagnostic::NAME, "lsp/navigate_diagnostic");
    let p = LspNavigateDiagnosticParams {
        buffer_id: 7,
        direction: DiagnosticDirection::Next,
        count: 1,
        extend: false,
    };
    let v = to_value(&p).unwrap();
    // count == 1 and extend == false are the defaults and stay off the wire. Navigation is from the
    // server's cursor, so no position rides on the params.
    assert_eq!(v, json!({"buffer_id": 7, "direction": "next"}));
    // A larger count and extend ride along when set.
    let v2 = to_value(&LspNavigateDiagnosticParams {
        buffer_id: 7,
        direction: DiagnosticDirection::Next,
        count: 2,
        extend: true,
    })
    .unwrap();
    assert_eq!(
        v2,
        json!({"buffer_id": 7, "direction": "next", "count": 2, "extend": true})
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

/// `workspace/changed` is a bare `WorkspaceInfo` — the same payload an RPC result carries, so a
/// client that didn't make the change adopts it through the same path.
#[test]
fn workspace_changed_is_a_workspace_info() {
    use aether_protocol::workspace::{WorkspaceChanged, WorkspaceInfo};
    assert_eq!(WorkspaceChanged::NAME, "workspace/changed");
    let info = WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether".into(),
        paths: vec!["/store/aether-3f9c/feature".into()],
        projects: Vec::new(),
    };
    let v = to_value(&info).unwrap();
    assert_eq!(v["name"], "aether");
    assert_eq!(v["paths"][0], "/store/aether-3f9c/feature");
}

#[test]
fn workspace_info_shape() {
    let p = WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether".into(),
        paths: vec!["/home/joe/x".into()],
        projects: Vec::new(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "aether", "paths": ["/home/joe/x"]}));
}

/// Projects ride on `WorkspaceInfo` but are absent entirely when none are declared, so workspaces
/// predating them keep the exact wire shape asserted above.
#[test]
fn workspace_info_carries_projects() {
    use aether_protocol::workspace::WorkspaceProject;
    let p = WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether".into(),
        paths: vec!["/src/aether".into()],
        projects: vec![
            // `.` is the root itself — a project is a directory, so this is the ordinary form for
            // a single-crate workspace rather than a special case.
            WorkspaceProject {
                path_index: 0,
                relative_path: ".".into(),
                language: "rust".into(),
                error: None,
            },
            WorkspaceProject {
                path_index: 0,
                relative_path: "gone".into(),
                language: "go".into(),
                error: Some("project directory does not exist: /src/aether/gone".into()),
            },
        ],
    };
    let v = to_value(&p).unwrap();
    assert_eq!(
        v["projects"],
        json!([
            {"path_index": 0, "relative_path": ".", "language": "rust"},
            {
                "path_index": 0,
                "relative_path": "gone",
                "language": "go",
                "error": "project directory does not exist: /src/aether/gone",
            },
        ]),
        "a resolving project carries no `error` key",
    );
    let back: WorkspaceInfo = serde_json::from_value(v).unwrap();
    assert_eq!(back.projects, p.projects);
}

#[test]
fn workspace_add_project_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceAddProject, WorkspaceAddProjectParams};
    assert_eq!(WorkspaceAddProject::NAME, "workspace/add_project");

    // Bare form: the language is inferred from the build manifests inside the directory.
    let p = WorkspaceAddProjectParams {
        workspace: "aether".into(),
        path_index: 0,
        relative_path: ".".into(),
        language: None,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"workspace": "aether", "path_index": 0, "relative_path": "."}),
    );

    // Explicit form, for a directory whose manifests don't single out one language, under a second
    // root.
    let p = WorkspaceAddProjectParams {
        workspace: "aether".into(),
        path_index: 1,
        relative_path: "web".into(),
        language: Some("typescript".into()),
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({
            "workspace": "aether",
            "path_index": 1,
            "relative_path": "web",
            "language": "typescript",
        }),
    );
}

#[test]
fn workspace_remove_project_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceRemoveProject, WorkspaceRemoveProjectParams};
    assert_eq!(WorkspaceRemoveProject::NAME, "workspace/remove_project");
    let p = WorkspaceRemoveProjectParams {
        workspace: "aether".into(),
        path_index: 0,
        relative_path: "web".into(),
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"workspace": "aether", "path_index": 0, "relative_path": "web"}),
    );
}

#[test]
fn workspace_infer_language_round_trip() {
    use aether_protocol::workspace::{
        WorkspaceInferLanguage, WorkspaceInferLanguageParams, WorkspaceInferLanguageResult,
    };
    assert_eq!(WorkspaceInferLanguage::NAME, "workspace/infer_language");

    let p = WorkspaceInferLanguageParams {
        workspace: "aether".into(),
        path_index: 1,
        relative_path: "databricks/".into(),
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({"workspace": "aether", "path_index": 1, "relative_path": "databricks/"}),
    );

    // An inferred language is present; "nothing inferred" omits the key entirely.
    let r = WorkspaceInferLanguageResult {
        language: Some("python".into()),
    };
    assert_eq!(to_value(&r).unwrap(), json!({"language": "python"}));
    let r = WorkspaceInferLanguageResult { language: None };
    assert_eq!(to_value(&r).unwrap(), json!({}));
    let back: WorkspaceInferLanguageResult = serde_json::from_value(json!({})).unwrap();
    assert_eq!(back.language, None);
}

#[test]
fn workspace_list_result_shape() {
    use aether_protocol::workspace::WorkspaceListResult;
    let r = WorkspaceListResult {
        workspaces: vec![
            WorkspaceSummary { name: "a".into() },
            WorkspaceSummary { name: "b".into() },
        ],
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v, json!({"workspaces": [{"name": "a"}, {"name": "b"}]}));
}

#[test]
fn workspace_activate_result_wraps_info() {
    use aether_protocol::workspace::WorkspaceActivateResult;
    let r = WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec!["/p".into()],
            projects: Vec::new(),
        },
        last_view_id: None,
        opened: None,
        server_started_at: 0,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["workspace"]["name"], "aether");
    assert_eq!(v["workspace"]["paths"][0], "/p");
    assert!(
        v.get("last_view_id").is_none(),
        "None last_view_id should be skipped"
    );
}

#[test]
fn workspace_create_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceCreate, WorkspaceCreateParams};
    assert_eq!(WorkspaceCreate::NAME, "workspace/create");
    let p = WorkspaceCreateParams {
        name: "newproj".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "newproj"}));
}

#[test]
fn workspace_add_root_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceAddRoot, WorkspaceAddRootParams};
    assert_eq!(WorkspaceAddRoot::NAME, "workspace/add_root");
    let p = WorkspaceAddRootParams {
        workspace: "aether".into(),
        path: "~/src/aether".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"workspace": "aether", "path": "~/src/aether"}));
}

#[test]
fn workspace_rename_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceRename, WorkspaceRenameParams};
    assert_eq!(WorkspaceRename::NAME, "workspace/rename");
    let p = WorkspaceRenameParams {
        workspace: "aether".into(),
        new_name: "aether-next".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"workspace": "aether", "new_name": "aether-next"}));
    // Result is a plain WorkspaceInfo (new name + paths).
    let info = WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether-next".into(),
        paths: vec!["/p".into()],
        projects: Vec::new(),
    };
    assert_eq!(to_value(&info).unwrap()["name"], "aether-next");
}

#[test]
fn workspace_renamed_notification_round_trip() {
    use aether_protocol::envelope::NotificationMethod;
    use aether_protocol::workspace::{WorkspaceRenamed, WorkspaceRenamedParams};
    assert_eq!(WorkspaceRenamed::NAME, "workspace/renamed");
    let p = WorkspaceRenamedParams {
        old_name: "aether".into(),
        new_name: "aether-next".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"old_name": "aether", "new_name": "aether-next"}));
    let back: WorkspaceRenamedParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.new_name, "aether-next");
}

#[test]
fn workspace_delete_params_round_trip() {
    use aether_protocol::workspace::{WorkspaceDelete, WorkspaceDeleteParams};
    assert_eq!(WorkspaceDelete::NAME, "workspace/delete");
    let p = WorkspaceDeleteParams {
        name: "aether".into(),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v, json!({"name": "aether"}));
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
        next_view_id: Some(aether_protocol::ViewId(9)),
    };
    let v = to_value(&full).unwrap();
    assert_eq!(v["closed_buffer_ids"], json!([3, 7]));
    assert_eq!(v["next_view_id"], 9);

    // `next_view_id` is omitted when there's nothing to attach to.
    let none = PathDeleteResult {
        closed_buffer_ids: vec![],
        next_view_id: None,
    };
    assert_eq!(to_value(&none).unwrap().get("next_view_id"), None);
}

#[test]
fn workspace_remove_root_result_shape() {
    use aether_protocol::workspace::{WorkspaceRemoveRoot, WorkspaceRemoveRootResult};
    assert_eq!(WorkspaceRemoveRoot::NAME, "workspace/remove_root");
    let r = WorkspaceRemoveRootResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec!["/p".into()],
            projects: Vec::new(),
        },
        closed_buffer_ids: vec![3, 5],
        next_view_id: Some(aether_protocol::ViewId(7)),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["workspace"]["name"], "aether");
    assert_eq!(v["closed_buffer_ids"], json!([3, 5]));
    assert_eq!(v["next_view_id"], 7);
}

#[test]
fn workspace_remove_root_result_skips_none_next_buffer() {
    use aether_protocol::workspace::WorkspaceRemoveRootResult;
    let r = WorkspaceRemoveRootResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec![],
            projects: Vec::new(),
        },
        closed_buffer_ids: vec![],
        next_view_id: None,
    };
    let v = to_value(&r).unwrap();
    assert!(v.get("next_view_id").is_none());
}

#[test]
fn workspace_activate_result_includes_last_view_id_when_set() {
    use aether_protocol::workspace::WorkspaceActivateResult;
    let r = WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec!["/p".into()],
            projects: Vec::new(),
        },
        last_view_id: Some(aether_protocol::ViewId(7)),
        opened: None,
        server_started_at: 1_700_000_000_000,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["last_view_id"], 7);
    assert_eq!(v["server_started_at"], 1_700_000_000_000_u64);
}

#[test]
fn buffer_open_scratch_form() {
    // Both path_index and relative_path null => scratch buffer.
    let v = to_value(ViewOpenParams {
        transient: None,
        path_index: None,
        relative_path: None,
        language: Some("rust".into()),
        create_if_missing: false,
        jump_to: None,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v["path_index"], serde_json::Value::Null);
    assert_eq!(v["relative_path"], serde_json::Value::Null);
}

#[test]
fn view_closed_notification_shape() {
    use aether_protocol::buffer::BufferLocation;
    use aether_protocol::view::ViewClosedParams;
    use aether_protocol::ViewId;
    // The view's buffer went with it, and there is a next view to switch to.
    let some = to_value(ViewClosedParams {
        view_id: ViewId(4),
        buffer_id: Some(4),
        next_view_id: Some(ViewId(7)),
        next_path: None,
    })
    .unwrap();
    assert_eq!(
        some,
        json!({"view_id": 4, "buffer_id": 4, "next_view_id": 7})
    );
    // A sibling closed alone — the buffer stays for its other view — and no views remain:
    // `buffer_id` and `next_view_id` are omitted, the latter signalling "open a fresh scratch".
    let none = to_value(ViewClosedParams {
        view_id: ViewId(4),
        buffer_id: None,
        next_view_id: None,
        next_path: None,
    })
    .unwrap();
    assert_eq!(none, json!({"view_id": 4}));
    // And it deserializes back when the fields are absent.
    let parsed: ViewClosedParams = from_value(json!({"view_id": 9})).unwrap();
    assert_eq!(parsed.view_id, ViewId(9));
    assert_eq!(parsed.buffer_id, None);
    assert_eq!(parsed.next_view_id, None);
    assert_eq!(parsed.next_path, None);

    // A worktree rebind names the successor by **path** instead — the id it could offer is a
    // dormant placeholder the initiator's own landing view can materialise under a different id.
    let moved = to_value(ViewClosedParams {
        view_id: ViewId(4),
        buffer_id: Some(4),
        next_view_id: None,
        next_path: Some(BufferLocation {
            path_index: 0,
            relative_path: "src/main.rs".into(),
        }),
    })
    .unwrap();
    assert_eq!(
        moved,
        json!({
            "view_id": 4,
            "buffer_id": 4,
            "next_path": { "path_index": 0, "relative_path": "src/main.rs" },
        })
    );
}

#[test]
fn view_set_transient_shape() {
    use aether_protocol::view::{ViewSetTransientParams, ViewSetTransientResult};
    let p = to_value(ViewSetTransientParams {
        view_id: aether_protocol::ViewId(4),
        transient: true,
    })
    .unwrap();
    assert_eq!(p, json!({"view_id": 4, "transient": true}));
    let parsed: ViewSetTransientParams =
        from_value(json!({"view_id": 9, "transient": false})).unwrap();
    assert_eq!(parsed.view_id, aether_protocol::ViewId(9));
    assert!(!parsed.transient);
    let r = to_value(ViewSetTransientResult { transient: false }).unwrap();
    assert_eq!(r, json!({"transient": false}));
}

#[test]
fn buffer_content_shape() {
    use aether_protocol::buffer::{BufferContentParams, BufferContentResult};
    let p = to_value(BufferContentParams { buffer_id: 4 }).unwrap();
    assert_eq!(p, json!({"buffer_id": 4}));
    let r = to_value(BufferContentResult {
        revision: 7,
        text: "# Title\n".into(),
    })
    .unwrap();
    assert_eq!(r, json!({"revision": 7, "text": "# Title\n"}));
}

#[test]
fn syntax_highlight_snippet_shape() {
    use aether_protocol::syntax::{SyntaxHighlightSnippetParams, SyntaxHighlightSnippetResult};
    use aether_protocol::viewport::Highlight;
    let p = to_value(SyntaxHighlightSnippetParams {
        language: "rust".into(),
        text: "fn x() {}".into(),
    })
    .unwrap();
    assert_eq!(p, json!({"language": "rust", "text": "fn x() {}"}));
    let r = to_value(SyntaxHighlightSnippetResult {
        highlights: vec![Highlight {
            start: 0,
            end: 2,
            kind: "keyword".into(),
        }],
    })
    .unwrap();
    assert_eq!(
        r,
        json!({"highlights": [{"start": 0, "end": 2, "kind": "keyword"}]})
    );
}

#[test]
fn buffer_changed_notification_shape() {
    use aether_protocol::buffer::BufferChangedParams;
    let p = to_value(BufferChangedParams {
        buffer_id: 4,
        revision: 9,
    })
    .unwrap();
    assert_eq!(p, json!({"buffer_id": 4, "revision": 9}));
    let parsed: BufferChangedParams = from_value(json!({"buffer_id": 2, "revision": 3})).unwrap();
    assert_eq!(parsed.buffer_id, 2);
    assert_eq!(parsed.revision, 3);
}

#[test]
fn git_show_target_shape() {
    use aether_protocol::git::{GitShowParams, GitShowResult, ShowTarget};

    // One RPC, three targets — tagged, so the shape says which it is rather than leaving the
    // server to infer it from which fields happen to be set.
    for (target, wire) in [
        (
            ShowTarget::Commit {
                rev: "abc1234".into(),
            },
            json!({ "kind": "commit", "rev": "abc1234" }),
        ),
        (
            ShowTarget::File {
                rev: "abc1234".into(),
                path: "src/a.rs".into(),
            },
            json!({ "kind": "file", "rev": "abc1234", "path": "src/a.rs" }),
        ),
        (
            ShowTarget::WorkingChanges,
            json!({ "kind": "working_changes" }),
        ),
    ] {
        assert_eq!(to_value(&target).unwrap(), wire);
        let back: ShowTarget = from_value(wire).unwrap();
        assert_eq!(back, target);
    }

    // The working tree names no revision — which is what stops it being handed to `rev-parse`,
    // and what decides that it must be regenerated rather than attached to.
    assert_eq!(ShowTarget::WorkingChanges.rev(), None);
    assert_eq!(ShowTarget::WorkingChanges.path(), None);

    // Both repo hints are optional: a keystroke sends the buffer, a picker row sends the repo.
    let v = to_value(GitShowParams {
        repo_id: None,
        buffer_id: Some(4),
        target: ShowTarget::WorkingChanges,
        focus_path: None,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "buffer_id": 4, "target": { "kind": "working_changes" } })
    );

    // A clean working tree materialises nothing, and says so by omission — same shape (and same
    // reason) as `git/follow_patch_line` finding nothing to follow.
    let v = to_value(GitShowResult {
        opened: None,
        baseline: None,
    })
    .unwrap();
    assert_eq!(v, json!({}));
    let back: GitShowResult = from_value(v).unwrap();
    assert!(back.opened.is_none());
    assert!(back.baseline.is_none());

    // Empty *because of a pinned baseline* carries it: with no buffer minted, the toast is the
    // only place the reason can be said, and "nothing changed" would be wrong about a dirty tree.
    let v = to_value(GitShowResult {
        opened: None,
        baseline: Some(aether_protocol::git::GitBaselineSource::Rev {
            label: "main".into(),
            commit: "abc1234".into(),
        }),
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "baseline": { "kind": "rev", "label": "main", "commit": "abc1234" } })
    );
    let back: GitShowResult = from_value(v).unwrap();
    assert!(back.opened.is_none());
    assert!(back.baseline.is_some());
}

#[test]
fn follow_patch_line_shape() {
    use aether_protocol::git::{GitFollowPatchLineParams, GitFollowPatchLineResult};
    use aether_protocol::view::{BufferDescription, ViewOpenResult};

    let v = to_value(GitFollowPatchLineParams { buffer_id: 7 }).unwrap();
    assert_eq!(v, json!({ "buffer_id": 7 }), "the cursor stays server-side");

    // Nothing to follow — the cursor was on the metadata block or the message. A quiet no-op, so
    // the field drops off the wire entirely rather than riding as an explicit null.
    let v = to_value(GitFollowPatchLineResult { opened: None }).unwrap();
    assert_eq!(v, json!({}));
    let back: GitFollowPatchLineResult = from_value(v).unwrap();
    assert!(back.opened.is_none());

    // `is_patch` distinguishes a commit's diff from a file at a revision — both read-only, only
    // the first has an index for `Enter` to follow through. Omitted when false, like `read_only`.
    let revision_buffer = |is_patch: bool| ViewOpenResult {
        view_id: aether_protocol::ViewId(3),
        scroll: None,
        transient: true,
        buffer: BufferDescription {
            buffer_id: 3,
            language: None,
            line_count: 1,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: None,
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: Some("abc1234:src/a.rs".into()),
            read_only: true,
            is_patch,
        },
    };
    let v = to_value(revision_buffer(false)).unwrap();
    assert_eq!(v["read_only"], true);
    assert!(
        v.get("is_patch").is_none(),
        "a file at a revision is read-only but not a patch"
    );
    let v = to_value(revision_buffer(true)).unwrap();
    assert_eq!(v["is_patch"], true);
    let back: ViewOpenResult = from_value(v).unwrap();
    assert!(back.is_patch);
}

#[test]
fn nav_goto_params_shape() {
    use aether_protocol::cursor::CursorState;
    use aether_protocol::nav::NavGotoParams;
    // File entry: path fields present, view_id omitted; cursor carries the selection.
    let p = NavGotoParams {
        virtual_key: None,
        view_id: None,
        path_index: Some(0),
        relative_path: Some("src/main.rs".into()),
        cursor: CursorState {
            position: LogicalPosition { line: 9, col: 2 },
            anchor: LogicalPosition { line: 5, col: 0 },
            match_bracket: None,
            jumplist_position: None,
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
    // Round-trips with a bare cursor (no match_bracket/jumplist_position) and a view_id reference.
    let parsed: NavGotoParams = from_value(json!({
        "view_id": 3,
        "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
    }))
    .unwrap();
    assert_eq!(parsed.view_id, Some(aether_protocol::ViewId(3)));
    assert_eq!(parsed.relative_path, None);
}

#[test]
fn nav_step_result_omits_absent_target() {
    use aether_protocol::nav::NavStepResult;
    assert_eq!(to_value(NavStepResult { target: None }).unwrap(), json!({}));
}

#[test]
fn unit_result_round_trips() {
    // ViewClose and ViewportUnsubscribe have Result = (). The JSON unit value is `null`.
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
fn git_checkout_shape() {
    use aether_protocol::git::{
        GitCheckout, GitCheckoutParams, GitCheckoutResult, GitCheckoutStatus, GitHead,
        GitRefreshResult,
    };
    assert_eq!(GitCheckout::NAME, "git/checkout");

    // Switching to an existing branch: `create` is defaulted away.
    let p = GitCheckoutParams {
        repo_id: Some("/home/u/proj".into()),
        branch: "main".into(),
        ..Default::default()
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "repo_id": "/home/u/proj", "branch": "main" })
    );

    // A success carries the new head and nothing else; the refusal fields stay off the wire.
    let ok = GitCheckoutResult {
        status: GitCheckoutStatus::Switched,
        head: Some(GitHead::Branch {
            name: "main".into(),
            upstream: None,
        }),
        ..Default::default()
    };
    let v = to_value(&ok).unwrap();
    assert_eq!(v["status"], "switched");
    assert_eq!(v["head"]["state"], "branch");
    assert!(v.get("blocked").is_none() && v.get("message").is_none());
    assert_eq!(from_value::<GitCheckoutResult>(v).unwrap(), ok);

    // A blocked checkout is an *outcome*, not an error: the buffers ride back so the client can
    // name what to save, and `head` stays absent because nothing moved.
    let blocked = GitCheckoutResult {
        status: GitCheckoutStatus::BlockedByDirtyBuffers,
        blocked: vec![3, 7],
        refreshed: GitRefreshResult::default(),
        ..Default::default()
    };
    let v = to_value(&blocked).unwrap();
    assert_eq!(v["status"], "blocked_by_dirty_buffers");
    assert_eq!(v["blocked"], json!([3, 7]));
    assert!(v.get("head").is_none(), "nothing moved, so no new head");
    assert_eq!(from_value::<GitCheckoutResult>(v).unwrap(), blocked);

    for (s, wire) in [
        (GitCheckoutStatus::Created, "created"),
        (GitCheckoutStatus::AlreadyCheckedOut, "already_checked_out"),
        (GitCheckoutStatus::Refused, "refused"),
    ] {
        assert_eq!(to_value(s).unwrap(), json!(wire));
    }
}

#[test]
fn git_fetch_and_push_shapes() {
    use aether_protocol::git::{
        GitFetch, GitFetchResult, GitFetchStatus, GitPush, GitPushResult, GitPushStatus,
        GitUpstreamStatus,
    };
    assert_eq!(GitFetch::NAME, "git/fetch");
    assert_eq!(GitPush::NAME, "git/push");

    // A no-remote answer is decided without running git, so it carries neither a message nor a
    // divergence — the empty fields stay off the wire.
    let v = to_value(GitFetchResult {
        status: GitFetchStatus::NoRemote,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v, json!({ "status": "no_remote" }));

    // A first push reports the tracking it established. `set_upstream` is a bool that only appears
    // when true, so an ordinary push doesn't carry it.
    let first = GitPushResult {
        status: GitPushStatus::Pushed,
        message: String::new(),
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 0,
            behind: 0,
        }),
        set_upstream: true,
    };
    let v = to_value(&first).unwrap();
    assert_eq!(v["status"], "pushed");
    assert_eq!(v["set_upstream"], json!(true));
    assert_eq!(v["upstream"]["name"], "origin/main");
    assert_eq!(from_value::<GitPushResult>(v).unwrap(), first);

    // A `Behind` refusal keeps *both*: our classification and git's own words. Dropping either
    // would cost the client something — the verb to suggest, or the detail behind it.
    let behind = GitPushResult {
        status: GitPushStatus::Behind,
        message: "hint: Updates were rejected".into(),
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 1,
            behind: 3,
        }),
        set_upstream: false,
    };
    let v = to_value(&behind).unwrap();
    assert_eq!(v["status"], "behind");
    assert_eq!(v["upstream"]["behind"], json!(3));
    assert!(v.get("set_upstream").is_none(), "false stays off the wire");
    assert_eq!(from_value::<GitPushResult>(v).unwrap(), behind);

    for (s, wire) in [
        (GitPushStatus::NothingToPush, "nothing_to_push"),
        (GitPushStatus::DetachedHead, "detached_head"),
        (GitPushStatus::AmbiguousRemote, "ambiguous_remote"),
        (GitPushStatus::NoRemote, "no_remote"),
        (GitPushStatus::Refused, "refused"),
    ] {
        assert_eq!(to_value(s).unwrap(), json!(wire));
    }
}

#[test]
fn git_pull_shape() {
    use aether_protocol::git::{
        GitPull, GitPullResult, GitPullStatus, GitRefreshResult, GitUpstreamStatus,
    };
    assert_eq!(GitPull::NAME, "git/pull");

    // A pull that ran but found nothing carries only its status: no message, no reconciliation,
    // and — being level — a divergence of zeros that still has to appear, because "level" and "no
    // upstream" are different answers everywhere else in this protocol too.
    let level = GitPullResult {
        status: GitPullStatus::UpToDate,
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 0,
            behind: 0,
        }),
        ..Default::default()
    };
    let v = to_value(&level).unwrap();
    assert_eq!(
        v,
        json!({ "status": "up_to_date", "upstream": { "name": "origin/main", "ahead": 0, "behind": 0 } })
    );
    assert_eq!(from_value::<GitPullResult>(v).unwrap(), level);

    // A conflicted pull is the shape the others aren't: it *failed* and still moved the tree, so
    // the reconciliation report and the conflicting paths travel together.
    let conflicted = GitPullResult {
        status: GitPullStatus::Conflicted,
        message: "CONFLICT (content): Merge conflict in a.rs".into(),
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 1,
            behind: 1,
        }),
        blocked: Vec::new(),
        refreshed: GitRefreshResult {
            reloaded: vec![7],
            ..Default::default()
        },
        conflicts: vec!["a.rs".into()],
        operation: Some(aether_protocol::git::GitRepoOperation::Merge),
        index_locked: false,
    };
    let v = to_value(&conflicted).unwrap();
    assert_eq!(v["status"], "conflicted");
    assert_eq!(v["conflicts"], json!(["a.rs"]));
    assert_eq!(v["refreshed"]["reloaded"], json!([7]));
    assert!(v.get("blocked").is_none(), "empty stays off the wire");
    assert_eq!(from_value::<GitPullResult>(v).unwrap(), conflicted);

    // The pre-flight refusal is the mirror image: git never ran, so there is nothing to reconcile
    // and no divergence worth reporting — only who blocked it.
    let blocked = GitPullResult {
        status: GitPullStatus::BlockedByDirtyBuffers,
        blocked: vec![1, 4],
        ..Default::default()
    };
    let v = to_value(&blocked).unwrap();
    assert_eq!(
        v,
        json!({ "status": "blocked_by_dirty_buffers", "blocked": [1, 4] })
    );
    assert_eq!(from_value::<GitPullResult>(v).unwrap(), blocked);

    for (s, wire) in [
        (GitPullStatus::FastForwarded, "fast_forwarded"),
        (GitPullStatus::Merged, "merged"),
        (GitPullStatus::Rebased, "rebased"),
        (GitPullStatus::Diverged, "diverged"),
        (GitPullStatus::OperationInProgress, "operation_in_progress"),
        (GitPullStatus::NoUpstream, "no_upstream"),
        (GitPullStatus::DetachedHead, "detached_head"),
        (GitPullStatus::NoRemote, "no_remote"),
        (GitPullStatus::Refused, "refused"),
        (GitPullStatus::Cancelled, "cancelled"),
    ] {
        assert_eq!(to_value(s).unwrap(), json!(wire));
    }
}

/// A repo stopped part-way through an operation, on the two shapes that carry it: the pull result
/// that refused because of it, and the buffer status the bar reads.
///
/// The wire values are mirrored by hand in `web/src/protocol.ts` (`REPO_OPERATION_LABELS`), which
/// is what makes them worth pinning rather than trusting to derive.
#[test]
fn git_repo_operation_shape() {
    use aether_protocol::git::{GitBufferStatus, GitPullResult, GitPullStatus, GitRepoOperation};

    for (op, wire, label) in [
        (GitRepoOperation::Merge, "merge", "merging"),
        (GitRepoOperation::Rebase, "rebase", "rebasing"),
        (
            GitRepoOperation::CherryPick,
            "cherry_pick",
            "cherry-picking",
        ),
        (GitRepoOperation::Revert, "revert", "reverting"),
        (GitRepoOperation::Bisect, "bisect", "bisecting"),
        (GitRepoOperation::ApplyMailbox, "apply_mailbox", "applying"),
    ] {
        assert_eq!(to_value(op).unwrap(), json!(wire));
        assert_eq!(op.label(), label);
    }

    // A clean repo carries nothing — the field is absent, not `"clean"`, so every existing client
    // reading a status keeps deserializing one.
    let clean = GitBufferStatus::default();
    assert!(to_value(&clean).unwrap().get("operation").is_none());

    let stopped = GitBufferStatus {
        branch: Some("a1b2c3d".into()),
        operation: Some(GitRepoOperation::Rebase),
        ..Default::default()
    };
    let v = to_value(&stopped).unwrap();
    assert_eq!(v["operation"], "rebase");
    assert_eq!(from_value::<GitBufferStatus>(v).unwrap(), stopped);

    // The refusal, plus the stranded-lock flag — a bool that only appears when true, like
    // `set_upstream` on a push.
    let mid = GitPullResult {
        status: GitPullStatus::OperationInProgress,
        operation: Some(GitRepoOperation::Rebase),
        conflicts: vec!["a.rs".into()],
        ..Default::default()
    };
    let v = to_value(&mid).unwrap();
    assert_eq!(
        v,
        json!({
            "status": "operation_in_progress",
            "operation": "rebase",
            "conflicts": ["a.rs"],
        })
    );
    assert_eq!(from_value::<GitPullResult>(v).unwrap(), mid);

    let locked = GitPullResult {
        status: GitPullStatus::Cancelled,
        index_locked: true,
        ..Default::default()
    };
    let v = to_value(&locked).unwrap();
    assert_eq!(v, json!({ "status": "cancelled", "index_locked": true }));
    assert_eq!(from_value::<GitPullResult>(v).unwrap(), locked);
}

/// The in-flight indicator's third kind. A wire value the client switches a label on, so it is
/// pinned like the statuses are.
#[test]
fn git_operation_kind_covers_pull() {
    use aether_protocol::git::GitOperationKind;
    for (kind, wire, label) in [
        (GitOperationKind::Fetch, "fetch", "Fetching"),
        (GitOperationKind::Push, "push", "Pushing"),
        (GitOperationKind::Pull, "pull", "Pulling"),
    ] {
        assert_eq!(to_value(kind).unwrap(), json!(wire));
        assert_eq!(kind.label(), label);
    }
}

#[test]
fn git_delete_branch_shape() {
    use aether_protocol::git::{
        GitDeleteBranch, GitDeleteBranchParams, GitDeleteBranchResult, GitDeleteBranchStatus,
    };
    assert_eq!(GitDeleteBranch::NAME, "git/delete_branch");

    let p = GitDeleteBranchParams {
        repo_id: Some("/home/u/proj".into()),
        branch: "feature".into(),
        force: true,
        ..Default::default()
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "repo_id": "/home/u/proj", "branch": "feature", "force": true })
    );

    let refused = GitDeleteBranchResult {
        status: GitDeleteBranchStatus::NotMerged,
        message: String::new(),
    };
    let v = to_value(&refused).unwrap();
    assert_eq!(v["status"], "not_merged");
    assert!(
        v.get("message").is_none(),
        "a pre-flight refusal has no git output to carry"
    );
    assert_eq!(from_value::<GitDeleteBranchResult>(v).unwrap(), refused);

    for (s, wire) in [
        (GitDeleteBranchStatus::Deleted, "deleted"),
        (GitDeleteBranchStatus::IsCurrentBranch, "is_current_branch"),
        (GitDeleteBranchStatus::Refused, "refused"),
    ] {
        assert_eq!(to_value(s).unwrap(), json!(wire));
    }
}

#[test]
fn picker_item_git_branch_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::GitBranches).unwrap(),
        json!("git_branches")
    );
    // Flat and not a jump target, like LspServers: no headers, no grouping, no cursor centring,
    // and nothing to capture into the jumplist.
    assert!(!PickerKind::GitBranches.groups_by_file());
    assert!(!PickerKind::GitBranches.collapsible());
    assert!(!PickerKind::GitBranches.renders_group_headers());
    assert!(!PickerKind::GitBranches.centers_on_cursor());
    assert!(!PickerKind::GitBranches.captures_to_jumplist());

    // The ordinary row — an unremarkable branch in a single-worktree repo — carries only what it
    // has to. Every decoration field is defaulted away, so the common case stays small.
    let plain = PickerItem::GitBranch {
        repo_id: "/home/u/proj".into(),
        name: "feature".into(),
        is_head: false,
        subject: "Add the thing".into(),
        timestamp: 1_700_000_000,
        upstream: None,
        ahead: 0,
        behind: 0,
        checkout: None,
        detached_at: None,
        match_indices: vec![0, 1],
    };
    let v = to_value(&plain).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "git_branch",
            "repo_id": "/home/u/proj",
            "name": "feature",
            "subject": "Add the thing",
            "timestamp": 1_700_000_000i64,
            "match_indices": [0, 1],
        }),
        "is_head/upstream/ahead/behind/checkout/detached_at all omitted at their defaults"
    );
    assert_eq!(from_value::<PickerItem>(v).unwrap(), plain);

    // The decorated row: current branch, tracking an upstream it has diverged from, and held by a
    // linked worktree. `checkout` is what turns this from a branch row into a worktree row —
    // `Enter` opens that tree instead of moving HEAD, and `Ctrl-d` removes it rather than deleting
    // the branch — and `worktree` is the admin name both of those are keyed on.
    let decorated = PickerItem::GitBranch {
        repo_id: "/home/u/proj".into(),
        name: "main".into(),
        is_head: true,
        subject: "Release".into(),
        timestamp: 1_700_000_001,
        upstream: Some("origin/main".into()),
        ahead: 2,
        behind: 1,
        checkout: Some(BranchCheckout {
            path: "/home/u/trees/feature-auth".into(),
            is_main: false,
            worktree: "feature-auth".into(),
            is_current: true,
            locked: false,
            prunable: false,
        }),
        detached_at: None,
        match_indices: vec![],
    };
    let v = to_value(&decorated).unwrap();
    assert_eq!(v["is_head"], true);
    assert_eq!(v["upstream"], "origin/main");
    assert_eq!(v["ahead"], 2);
    assert_eq!(v["behind"], 1);
    assert_eq!(v["checkout"]["path"], "/home/u/trees/feature-auth");
    assert_eq!(v["checkout"]["worktree"], "feature-auth");
    assert_eq!(v["checkout"]["is_current"], true);
    // The main checkout's admin name is empty and its flags are false, so a row held by it carries
    // `is_main` and nothing else — the same "common case stays small" rule as the plain row.
    assert_eq!(v["checkout"].get("is_main"), None);
    assert_eq!(v["checkout"].get("locked"), None);
    assert_eq!(v["checkout"].get("prunable"), None);
    assert_eq!(from_value::<PickerItem>(v).unwrap(), decorated);

    // A detached worktree: no branch, so `name` is the tree's admin name and `detached_at` carries
    // what it is sitting on. Without such a row the tree would be unreachable in a branch-keyed
    // list — including for the removal that is the only thing left to do with a prunable one.
    let detached = PickerItem::GitBranch {
        repo_id: "/home/u/proj".into(),
        name: "spike".into(),
        is_head: false,
        subject: String::new(),
        timestamp: 0,
        upstream: None,
        ahead: 0,
        behind: 0,
        checkout: Some(BranchCheckout {
            path: "/home/u/trees/spike".into(),
            is_main: false,
            worktree: "spike".into(),
            is_current: false,
            locked: false,
            prunable: true,
        }),
        detached_at: Some("abc1234".into()),
        match_indices: vec![],
    };
    let v = to_value(&detached).unwrap();
    assert_eq!(v["detached_at"], "abc1234");
    assert_eq!(v["checkout"]["prunable"], true);
    assert_eq!(from_value::<PickerItem>(v).unwrap(), detached);
}

#[test]
fn picker_item_git_commit_carries_typed_decorations() {
    use aether_protocol::git::{CommitRef, CommitRefKind};
    use aether_protocol::picker::PickerItem;

    // The ordinary row: nothing points at this commit, which is true of almost every one, so the
    // decorations are omitted entirely rather than riding as an empty array on every row.
    let plain = PickerItem::GitCommit {
        repo_id: "/home/u/proj".into(),
        hash: "20a3a8ae0f0e2a5a0d2b7c1e9f8a6b5c4d3e2f10".into(),
        path: None,
        short_hash: "20a3a8a".into(),
        subject: "Add the thing".into(),
        decorations: vec![],
        match_indices: vec![4, 5],
        hash_match_len: 0,
    };
    let v = to_value(&plain).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "git_commit",
            "repo_id": "/home/u/proj",
            "hash": "20a3a8ae0f0e2a5a0d2b7c1e9f8a6b5c4d3e2f10",
            "short_hash": "20a3a8a",
            "subject": "Add the thing",
            "match_indices": [4, 5],
        }),
        "path/decorations/hash_match_len all omitted at their defaults"
    );
    assert_eq!(from_value::<PickerItem>(v).unwrap(), plain);

    // The decorated row — the tip of a released branch. Each ref travels *typed*, name only: the
    // `HEAD -> ` and `tag: ` literals are the client's to render, and are what let it colour the
    // kinds apart instead of printing one pre-joined string.
    let tip = PickerItem::GitCommit {
        repo_id: "/home/u/proj".into(),
        hash: "20a3a8ae0f0e2a5a0d2b7c1e9f8a6b5c4d3e2f10".into(),
        path: Some("src/main.rs".into()),
        short_hash: "20a3a8a".into(),
        subject: "Release".into(),
        decorations: vec![
            CommitRef {
                kind: CommitRefKind::HeadBranch,
                name: "main".into(),
            },
            CommitRef {
                kind: CommitRefKind::Tag,
                name: "v1.0".into(),
            },
            CommitRef {
                kind: CommitRefKind::Remote,
                name: "origin/main".into(),
            },
        ],
        match_indices: vec![],
        hash_match_len: 7,
    };
    let v = to_value(&tip).unwrap();
    assert_eq!(
        v["decorations"],
        json!([
            {"kind": "head_branch", "name": "main"},
            {"kind": "tag", "name": "v1.0"},
            {"kind": "remote", "name": "origin/main"},
        ])
    );
    assert_eq!(v["path"], "src/main.rs");
    assert_eq!(v["hash_match_len"], 7);
    assert_eq!(from_value::<PickerItem>(v).unwrap(), tip);

    // The text form the `git/show` header prints, which is git's own.
    let labels: Vec<String> = match &tip {
        PickerItem::GitCommit { decorations, .. } => {
            decorations.iter().map(CommitRef::label).collect()
        }
        _ => unreachable!(),
    };
    assert_eq!(labels, ["HEAD -> main", "tag: v1.0", "origin/main"]);
    assert_eq!(
        CommitRef {
            kind: CommitRefKind::Head,
            name: "HEAD".into()
        }
        .label(),
        "HEAD",
        "a detached HEAD decorates its commit on its own"
    );
}

#[test]
fn picker_item_git_change_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    use aether_protocol::viewport::DiffStage;
    assert_eq!(
        to_value(PickerKind::GitChanges).unwrap(),
        json!("git_changes")
    );
    assert_eq!(
        to_value(PickerKind::GitChangesFile).unwrap(),
        json!("git_changes_file")
    );
    // Both changes pickers centre on the cursor's hunk, but only the workspace one renders file
    // headers — the buffer-locked file picker is a single, headerless file.
    assert!(PickerKind::GitChanges.centers_on_cursor());
    assert!(
        PickerKind::GitChanges.groups_by_file() && !PickerKind::GitChangesFile.groups_by_file()
    );
    assert!(PickerKind::GitChangesFile.centers_on_cursor());

    // An unstaged hunk: the default stage is omitted from the wire.
    let item = PickerItem::GitChange {
        path_index: 0,
        relative_path: "src/main.rs".into(),
        hunk_index: 2,
        line: 41,
        stage: DiffStage::Unstaged,
        added: 3,
        removed: 1,
        preview: "    let x = 1;".into(),
        match_indices: vec![0, 1],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({
            "kind": "git_change",
            "path_index": 0,
            "relative_path": "src/main.rs",
            "hunk_index": 2,
            "line": 41,
            "added": 3,
            "removed": 1,
            "preview": "    let x = 1;",
            "match_indices": [0, 1],
        }),
        "the default Unstaged stage is skipped on the wire"
    );
    assert_eq!(from_value::<PickerItem>(v).unwrap(), item);

    // A staged hunk carries its stage explicitly.
    let staged = PickerItem::GitChange {
        path_index: 1,
        relative_path: "lib.rs".into(),
        hunk_index: 0,
        line: 0,
        stage: DiffStage::Staged,
        added: 0,
        removed: 4,
        preview: "gone".into(),
        match_indices: vec![],
    };
    let v = to_value(&staged).unwrap();
    assert_eq!(v["stage"], "staged");
    assert_eq!(from_value::<PickerItem>(v).unwrap(), staged);
}

#[test]
fn picker_item_diagnostic_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::Diagnostics).unwrap(),
        json!("diagnostics")
    );
    assert_eq!(
        to_value(PickerKind::DiagnosticsWorkspace).unwrap(),
        json!("diagnostics_workspace")
    );
    // The workspace picker groups by file; the buffer-scoped one is flat (and centres on neither).
    assert!(
        PickerKind::DiagnosticsWorkspace.groups_by_file()
            && !PickerKind::Diagnostics.groups_by_file()
    );
    let item = PickerItem::Diagnostic {
        path_index: 1,
        relative_path: "src/main.rs".into(),
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
    assert_eq!(v["path_index"], 1);
    assert_eq!(v["relative_path"], "src/main.rs");
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
        is_definition: true,
        match_indices: vec![4, 5],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "reference");
    assert_eq!(v["path"], "/home/u/proj/src/lib.rs");
    assert_eq!(v["display_path"], "src/lib.rs");
    assert_eq!(v["line"], 41);
    assert_eq!(v["col"], 7);
    assert_eq!(v["preview"], "    helper();");
    assert_eq!(v["is_definition"], true);
    assert_eq!(v["match_indices"], json!([4, 5]));
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    // is_definition and match_indices default (false / empty) when omitted, as the other variants do.
    let bare: PickerItem = from_value(json!({
        "kind": "reference", "path": "/a", "display_path": "a", "line": 0, "col": 0, "preview": ""
    }))
    .unwrap();
    assert!(
        matches!(bare, PickerItem::Reference { ref match_indices, is_definition: false, .. } if match_indices.is_empty())
    );
}

#[test]
fn picker_item_symbol_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind, SymbolKind};
    assert_eq!(
        to_value(PickerKind::WorkspaceSymbols).unwrap(),
        json!("workspace_symbols")
    );
    assert_eq!(
        to_value(PickerKind::DocumentSymbols).unwrap(),
        json!("document_symbols")
    );
    let item = PickerItem::Symbol {
        path: "/home/u/proj/src/lib.rs".into(),
        display_path: String::new(),
        line: 10,
        col: 3,
        name: "parse_header".into(),
        symbol_kind: SymbolKind::Function,
        detail: "fn(&[u8]) -> Header".into(),
        depth: 1,
        context: false,
        match_indices: vec![0, 1],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "symbol");
    assert_eq!(v["path"], "/home/u/proj/src/lib.rs");
    assert_eq!(v["line"], 10);
    assert_eq!(v["col"], 3);
    assert_eq!(v["name"], "parse_header");
    assert_eq!(v["symbol_kind"], "function");
    assert_eq!(v["detail"], "fn(&[u8]) -> Header");
    assert_eq!(v["depth"], 1);
    assert!(
        v.get("context").is_none(),
        "context absent on the wire when false"
    );
    assert_eq!(v["match_indices"], json!([0, 1]));
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    // A context (ancestor) row serializes the flag.
    let ctx = PickerItem::Symbol {
        path: "/a".into(),
        display_path: String::new(),
        line: 0,
        col: 0,
        name: "Outer".into(),
        symbol_kind: SymbolKind::Struct,
        detail: String::new(),
        depth: 0,
        context: true,
        match_indices: vec![],
    };
    assert_eq!(to_value(&ctx).unwrap()["context"], true);

    // detail / depth / context / match_indices all default when omitted.
    let bare: PickerItem = from_value(json!({
        "kind": "symbol", "path": "/a", "line": 0, "col": 0, "name": "x", "symbol_kind": "struct"
    }))
    .unwrap();
    assert!(matches!(
        bare,
        PickerItem::Symbol { ref detail, depth: 0, context: false, ref match_indices, .. }
            if detail.is_empty() && match_indices.is_empty()
    ));
    // An out-of-range LSP kind degrades to Unknown.
    assert_eq!(SymbolKind::from_lsp(99), SymbolKind::Unknown);
    assert_eq!(SymbolKind::from_lsp(12), SymbolKind::Function);
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
fn picker_item_keybinding_is_tagged() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    assert_eq!(
        to_value(PickerKind::Keybindings).unwrap(),
        json!("keybindings")
    );
    let item = PickerItem::Keybinding {
        group: "Editing".into(),
        desc: "Delete word back".into(),
        mode: "Any".into(),
        keys: "Ctrl-w".into(),
        match_indices: vec![10, 11],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "keybinding");
    assert_eq!(v["group"], "Editing");
    assert_eq!(v["desc"], "Delete word back");
    assert_eq!(v["mode"], "Any");
    assert_eq!(v["keys"], "Ctrl-w");
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn keybinding_entry_haystack_composes_in_display_order() {
    use aether_protocol::picker::KeybindingEntry;
    // The composition is a wire contract: match_indices index into this exact string. The
    // group is a section header, not row text, so it's absent; default modes (Normal / Any /
    // Application) are elided too.
    let mut e = KeybindingEntry {
        group: "Editing".into(),
        desc: "Delete word back".into(),
        mode: "Any".into(),
        keys: "Ctrl-w".into(),
    };
    assert_eq!(e.haystack(), "Delete word back Ctrl-w");
    // Insert/Search-only bindings spell their mode out.
    e.mode = "Insert".into();
    assert_eq!(e.haystack(), "Delete word back (Insert) Ctrl-w");
    e.mode = "Search".into();
    assert_eq!(e.haystack(), "Delete word back (Search) Ctrl-w");
}

#[test]
fn picker_view_params_keybindings_serialized_and_skipped_when_none() {
    use aether_protocol::picker::{KeybindingEntry, PickerKind, PickerReset, PickerViewParams};
    let p = PickerViewParams {
        view_id: None,
        from_selection: false,
        kind: PickerKind::Keybindings,
        reset: PickerReset::All,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor: None,
        directory_path: None,
        buffer_id: None,
        explorer_roots: false,
        filters: None,
        keybindings: Some(vec![KeybindingEntry {
            group: "App".into(),
            desc: "Show keyboard shortcuts".into(),
            mode: "Application".into(),
            keys: "Space y".into(),
        }]),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["kind"], "keybindings");
    assert_eq!(v["keybindings"][0]["group"], "App");
    assert_eq!(v["keybindings"][0]["keys"], "Space y");
    let back: PickerViewParams = from_value(v).unwrap();
    assert_eq!(back.keybindings.as_deref().map(|k| k.len()), Some(1));

    // Absent on the wire when None (resume/scroll re-views), and deserializes back to None.
    let p = PickerViewParams {
        view_id: None,
        keybindings: None,
        ..p
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("keybindings").is_none(),
        "None keybindings should be skipped"
    );
    let back: PickerViewParams = from_value(v).unwrap();
    assert!(back.keybindings.is_none());
}

#[test]
fn group_spans_are_tagged_and_skipped_when_empty() {
    use aether_protocol::picker::{GroupHeader, GroupSpan, PickerKind, PickerUpdateParams};
    let u = PickerUpdateParams {
        kind: PickerKind::Grep,
        generation: 3,
        offset: 10,
        items: Some(vec![]),
        total_matches: 0,
        total_candidates: 0,
        ticking: false,
        groups: vec![
            GroupSpan {
                start: 0,
                header: GroupHeader::File {
                    path_index: 1,
                    relative_path: "src/a.rs".into(),
                },
                count: None,
                expanded: None,
            },
            GroupSpan {
                start: 4,
                header: GroupHeader::Label {
                    label: "Definition".into(),
                },
                count: None,
                expanded: None,
            },
        ],
        display_offset: Some(11),
        total_display_rows: Some(20),
        focus_run: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let v = to_value(&u).unwrap();
    // Headers are internally tagged, like the items; the display metrics ride unprefixed.
    assert_eq!(v["groups"][0]["start"], 0);
    assert_eq!(v["groups"][0]["header"]["kind"], "file");
    assert_eq!(v["groups"][0]["header"]["relative_path"], "src/a.rs");
    assert_eq!(v["groups"][1]["header"]["kind"], "label");
    assert_eq!(v["groups"][1]["header"]["label"], "Definition");
    assert_eq!(v["display_offset"], 11);
    assert_eq!(v["total_display_rows"], 20);
    let back: PickerUpdateParams = from_value(v).unwrap();
    assert_eq!(back.groups, u.groups);

    // Flat kinds send no spans at all.
    let u = PickerUpdateParams {
        groups: vec![],
        items: None,
        ..u
    };
    let v = to_value(&u).unwrap();
    assert!(
        v.get("groups").is_none(),
        "empty groups skipped on the wire"
    );
    let back: PickerUpdateParams = from_value(v).unwrap();
    assert!(back.groups.is_empty());
}

/// The collapsible-group additions: the `Group` header row item, the span's count/expanded
/// decoration, and the `picker/set_group` wire shapes.
#[test]
fn collapsible_group_wire_shapes() {
    use aether_protocol::picker::{
        GroupHeader, GroupSpan, PickerGroupAction, PickerItem, PickerKind, PickerSetGroupParams,
        PickerSetGroupResult,
    };

    // The header row item: internally tagged `group`, with the nested header's own tag; a
    // collapsed row skips its false `expanded` flag on the wire.
    let item = PickerItem::Group {
        header: GroupHeader::File {
            path_index: 1,
            relative_path: "src/a.rs".into(),
        },
        count: 4,
        expanded: false,
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["kind"], "group");
    assert_eq!(v["header"]["kind"], "file");
    assert_eq!(v["header"]["relative_path"], "src/a.rs");
    assert_eq!(v["count"], 4);
    assert!(v.get("expanded").is_none(), "false expanded skipped");
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    let item = PickerItem::Group {
        header: GroupHeader::Label {
            label: "src/deep.rs".into(),
        },
        count: 2,
        expanded: true,
    };
    let v = to_value(&item).unwrap();
    assert_eq!(v["expanded"], true);
    let back: PickerItem = from_value(v).unwrap();
    assert_eq!(back, item);

    // Spans carry the same decoration for the collapsible kinds; `None` (the derived-header
    // kinds) is skipped, so their wire shape is unchanged.
    let span = GroupSpan {
        start: 0,
        header: GroupHeader::File {
            path_index: 0,
            relative_path: "src/a.rs".into(),
        },
        count: Some(10),
        expanded: Some(true),
    };
    let v = to_value(&span).unwrap();
    assert_eq!(v["count"], 10);
    assert_eq!(v["expanded"], true);
    let back: GroupSpan = from_value(v).unwrap();
    assert_eq!(back, span);
    let span = GroupSpan {
        count: None,
        expanded: None,
        ..span
    };
    let v = to_value(&span).unwrap();
    assert!(v.get("count").is_none() && v.get("expanded").is_none());

    // `picker/set_group`: one tagged action per gesture — expand/collapse a named group, step to
    // the neighbouring one (optionally opening it: the item-level spill), or toggle every group.
    let header = GroupHeader::File {
        path_index: 0,
        relative_path: "src/a.rs".into(),
    };
    let p = PickerSetGroupParams {
        kind: PickerKind::Grep,
        action: PickerGroupAction::Expand {
            header: header.clone(),
        },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["kind"], "grep");
    assert_eq!(v["action"]["action"], "expand");
    assert_eq!(v["action"]["header"]["kind"], "file");
    let back: PickerSetGroupParams = from_value(v).unwrap();
    assert!(matches!(back.action, PickerGroupAction::Expand { .. }));

    let p = PickerSetGroupParams {
        kind: PickerKind::Grep,
        action: PickerGroupAction::Collapse { header },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["action"]["action"], "collapse");
    let back: PickerSetGroupParams = from_value(v).unwrap();
    assert!(matches!(back.action, PickerGroupAction::Collapse { .. }));

    let p = PickerSetGroupParams {
        kind: PickerKind::Grep,
        action: PickerGroupAction::Step {
            direction: aether_protocol::cursor::Direction::Forward,
            expand: true,
        },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["action"]["action"], "step");
    assert_eq!(v["action"]["direction"], "forward");
    assert_eq!(v["action"]["expand"], true);
    let back: PickerSetGroupParams = from_value(v).unwrap();
    assert!(matches!(
        back.action,
        PickerGroupAction::Step {
            expand: true,
            direction: aether_protocol::cursor::Direction::Forward
        }
    ));

    let p = PickerSetGroupParams {
        kind: PickerKind::Grep,
        action: PickerGroupAction::ToggleAll,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["action"], json!({ "action": "toggle_all" }));
    let back: PickerSetGroupParams = from_value(v).unwrap();
    assert!(matches!(back.action, PickerGroupAction::ToggleAll));

    let r = PickerSetGroupResult {
        run: Some(aether_protocol::picker::GroupRunRows {
            header_row: 3,
            len: 7,
        }),
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["run"]["header_row"], 3);
    assert_eq!(v["run"]["len"], 7);
    let r = PickerSetGroupResult { run: None };
    let v = to_value(&r).unwrap();
    assert!(v.get("run").is_none(), "a vanished group answers empty");

    // The focused run's geometry on `picker/update` (two-level navigation math): absolute
    // header row + the item rows following it (0 when collapsed), skipped entirely when absent.
    let run = aether_protocol::picker::GroupRunRows {
        header_row: 4,
        len: 12,
    };
    let v = to_value(run).unwrap();
    assert_eq!(v["header_row"], 4);
    assert_eq!(v["len"], 12);
    let back: aether_protocol::picker::GroupRunRows = from_value(v).unwrap();
    assert_eq!(back, run);
}

/// The collapsible predicate: the file-grouped kinds plus WorkspaceSymbols and Jumplist — and
/// NOT the derived-header kinds (References, Keybindings) nor the headerless buffer-locked
/// GitChangesFile. Every collapsible kind renders group headers.
#[test]
fn collapsible_kinds_are_pinned() {
    use aether_protocol::picker::PickerKind;
    let collapsible = [
        PickerKind::Grep,
        PickerKind::GitChanges,
        PickerKind::DiagnosticsWorkspace,
        PickerKind::WorkspaceSymbols,
        PickerKind::Jumplist,
    ];
    for kind in collapsible {
        assert!(kind.collapsible(), "{kind:?}");
        assert!(kind.renders_group_headers(), "{kind:?}");
    }
    for kind in [
        PickerKind::References,
        PickerKind::Keybindings,
        PickerKind::GitChangesFile,
        PickerKind::Files,
        PickerKind::Diagnostics,
        PickerKind::DocumentSymbols,
    ] {
        assert!(!kind.collapsible(), "{kind:?}");
    }
    // The merged branch picker groups not at all. It used to be two pickers, one of which split its
    // rows into `Worktrees` and `Branches` sections; pinning the checked-out ones to the top of one
    // recency-ordered list replaced that, because sectioning scattered the branches you were
    // actually looking for across two places.
    assert!(!PickerKind::GitBranches.renders_group_headers());
    assert!(!PickerKind::GitBranches.collapsible());
}

#[test]
fn picker_view_params_omit_center_on_when_none() {
    use aether_protocol::picker::{PickerKind, PickerReset, PickerViewParams};
    let p = PickerViewParams {
        view_id: None,
        from_selection: false,
        kind: PickerKind::Files,
        reset: PickerReset::All,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor: None,
        directory_path: None,
        buffer_id: None,
        explorer_roots: false,
        filters: None,
        keybindings: None,
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("center_on").is_none(),
        "None center_on should be skipped"
    );
    assert_eq!(v["kind"], "files");
    assert_eq!(v["reset"], "all");
    assert!(
        v.get("from_selection").is_none(),
        "default from_selection is skipped on the wire"
    );
}

/// The reset scope is a two-valued string on the wire marking *fresh open vs re-view* — not a
/// per-kind policy. There is no longer any kind-dependent branch to pin here: `All` is what every
/// open sends, `Keep` is what every scroll/re-view within one open sends.
#[test]
fn picker_reset_wire_shape() {
    use aether_protocol::picker::{PickerKind, PickerReset};
    assert_eq!(to_value(PickerReset::Keep).unwrap(), json!("keep"));
    assert_eq!(to_value(PickerReset::All).unwrap(), json!("all"));
    // An absent `reset` means "keep" — the scroll/re-view path within one open.
    assert_eq!(
        serde_json::from_value::<PickerReset>(json!("keep")).unwrap(),
        PickerReset::default()
    );

    // Grep no longer opens framed on the cursor's nearest hit — there are no hits to frame.
    assert!(!PickerKind::Grep.centers_on_cursor());
    assert!(PickerKind::Jumplist.centers_on_cursor() && PickerKind::GitChanges.centers_on_cursor());
}

/// Grep is the only picker whose query is recallable — the fuzzy kinds filter a live candidate
/// set, where a stale query means nothing.
#[test]
fn only_grep_maps_to_an_input_history_list() {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::picker::PickerKind;
    assert_eq!(PickerKind::Grep.history_kind(), Some(HistoryKind::Grep));
    for kind in [
        PickerKind::Files,
        PickerKind::Views,
        PickerKind::Explorer,
        PickerKind::GitChanges,
        PickerKind::Workspaces,
    ] {
        assert_eq!(kind.history_kind(), None, "{kind:?} has no query history");
    }
}

/// `history/*` wire shapes: the kind is a snake_case tag and every list defaults, so a file or
/// payload written before a list existed still parses.
#[test]
fn history_wire_shapes_round_trip() {
    use aether_protocol::history::{
        HistoryEntry, HistoryKind, HistoryLists, HistoryRecordParams, HistoryStateResult,
    };
    use aether_protocol::picker::{MatchOptions, PickerFilters};
    assert_eq!(to_value(HistoryKind::Search).unwrap(), json!("search"));
    assert_eq!(to_value(HistoryKind::Grep).unwrap(), json!("grep"));
    assert_eq!(to_value(HistoryKind::Glob).unwrap(), json!("glob"));
    assert_eq!(to_value(HistoryKind::Path).unwrap(), json!("path"));

    // The entry flattens into the params, and default filters vanish from the wire entirely.
    let params = HistoryRecordParams {
        kind: HistoryKind::Grep,
        entry: HistoryEntry::bare("fn resolve"),
    };
    assert_eq!(
        to_value(&params).unwrap(),
        json!({ "kind": "grep", "value": "fn resolve" })
    );
    // A configured entry carries the chip row that produced it.
    let scoped = HistoryRecordParams {
        kind: HistoryKind::Grep,
        entry: HistoryEntry {
            value: "fn resolve".into(),
            filters: PickerFilters {
                regex: true,
                globs: vec!["*.rs".into()],
                ..Default::default()
            },
        },
    };
    assert_eq!(
        to_value(&scoped).unwrap(),
        json!({
            "kind": "grep",
            "value": "fn resolve",
            "filters": { "regex": true, "globs": ["*.rs"] }
        })
    );
    // The search prompt has no scoping, so its entries carry match options only.
    let searched = HistoryEntry::with_options(
        "f.o",
        MatchOptions {
            regex: true,
            ..Default::default()
        },
    );
    assert_eq!(
        to_value(&searched).unwrap(),
        json!({ "value": "f.o", "filters": { "regex": true } })
    );
    assert!(searched.filters.match_options().regex);

    let mut lists = HistoryLists::default();
    lists.record(HistoryKind::Glob, HistoryEntry::bare("*.rs"));
    let result = HistoryStateResult { lists };
    let v = to_value(&result).unwrap();
    assert_eq!(v["lists"]["glob"], json!([{ "value": "*.rs" }]));
    assert_eq!(v["lists"]["grep"], json!([]));
    // Missing lists parse as empty rather than failing.
    let parsed: HistoryStateResult =
        serde_json::from_value(json!({ "lists": { "search": [{ "value": "foo" }] } })).unwrap();
    assert_eq!(parsed.lists.search, vec![HistoryEntry::bare("foo")]);
    assert!(parsed.lists.path.is_empty());
}

#[test]
fn picker_view_params_from_selection_serialized() {
    use aether_protocol::picker::{PickerKind, PickerReset, PickerViewParams};
    // `Space Alt-/`: grep-for-selection rides `from_selection` + the active buffer id.
    let p = PickerViewParams {
        view_id: None,
        from_selection: true,
        kind: PickerKind::Grep,
        reset: PickerReset::Keep,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor: None,
        directory_path: None,
        buffer_id: Some(4),
        explorer_roots: false,
        filters: None,
        keybindings: None,
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["from_selection"], true);
    assert_eq!(v["buffer_id"], 4);
    let back: PickerViewParams = serde_json::from_value(v).unwrap();
    assert!(back.from_selection);
    assert_eq!(back.buffer_id, Some(4));
}

#[test]
fn picker_view_params_center_on_serialized() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerReset, PickerViewParams};
    let p = PickerViewParams {
        view_id: None,
        from_selection: false,
        kind: PickerKind::Files,
        reset: PickerReset::Keep,
        offset: 0,
        limit: 30,
        center_on: Some(PickerItem::File {
            path_index: 0,
            relative_path: "x".into(),
            match_indices: vec![],
            git_status: None,
        }),
        center_on_cursor: None,
        directory_path: None,
        buffer_id: None,
        explorer_roots: false,
        filters: None,
        keybindings: None,
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
        items: Some(vec![PickerItem::File {
            path_index: 0,
            relative_path: "a".into(),
            match_indices: vec![0],
            git_status: None,
        }]),
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
        focus_run: None,
        center_on: None,
        explorer_peek_missing: false,
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
    // center_on is absent on the wire when None (the common case).
    assert!(v["params"].get("center_on").is_none());
}

#[test]
fn picker_update_carries_center_on_symbol() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams, SymbolKind};
    let params = PickerUpdateParams {
        kind: PickerKind::DocumentSymbols,
        generation: 0,
        offset: 0,
        items: Some(vec![]),
        total_matches: 0,
        total_candidates: 0,
        ticking: false,
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
        focus_run: None,
        center_on: Some(Box::new(PickerItem::Symbol {
            path: "/p/a.rs".into(),
            display_path: String::new(),
            line: 3,
            col: 0,
            name: "main".into(),
            symbol_kind: SymbolKind::Function,
            detail: String::new(),
            depth: 0,
            context: false,
            match_indices: vec![],
        })),
        explorer_peek_missing: false,
    };
    let v = to_value(&params).unwrap();
    assert_eq!(v["center_on"]["kind"], "symbol");
    assert_eq!(v["center_on"]["name"], "main");
    let back: PickerUpdateParams = from_value(v).unwrap();
    assert_eq!(back.center_on, params.center_on);
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
    let item = PickerItem::View {
        buffer_id: 7,
        view_id: aether_protocol::ViewId(7),
        view_kind: None,
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
            "kind": "view",
            "buffer_id": 7,
            "view_id": 7,
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
    let scratch = PickerItem::View {
        buffer_id: 9,
        view_id: aether_protocol::ViewId(9),
        view_kind: None,
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
        "kind": "view", "buffer_id": 9, "view_id": 9, "display": "(scratch 1)"
    }))
    .unwrap();
    assert_eq!(back, scratch);
}

#[test]
fn picker_select_result_view_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::View {
        view_id: aether_protocol::ViewId(42),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({"kind": "view", "view_id": 42})
    );
    let at = PickerSelectResult::ViewAt {
        view_id: aether_protocol::ViewId(42),
        position: LogicalPosition { line: 3, col: 1 },
    };
    assert_eq!(
        to_value(&at).unwrap(),
        json!({"kind": "view_at", "view_id": 42, "position": {"line": 3, "col": 1}})
    );
}

#[test]
fn picker_kind_buffers_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(to_value(PickerKind::Views).unwrap(), json!("views"));
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
    // No anchor → a point; the optional field is omitted on the wire.
    let r = PickerSelectResult::FileAt {
        path: "/abs/x.rs".into(),
        position: LogicalPosition { line: 3, col: 7 },
        anchor: None,
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({
            "kind": "file_at",
            "path": "/abs/x.rs",
            "position": {"line": 3, "col": 7},
        })
    );
    // With an anchor → a selection (anchor..position); the field rides along.
    let r = PickerSelectResult::FileAt {
        path: "/abs/x.rs".into(),
        position: LogicalPosition { line: 3, col: 7 },
        anchor: Some(LogicalPosition { line: 3, col: 4 }),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({
            "kind": "file_at",
            "path": "/abs/x.rs",
            "position": {"line": 3, "col": 7},
            "anchor": {"line": 3, "col": 4},
        })
    );

    // A place *inside* a composed view: which element to focus and the buffer it windows — the
    // cursor is set on that buffer, never on the view's own document. `open` is absent while the
    // view is already showing, which is the ordinary case.
    let r = PickerSelectResult::ViewElement {
        element: 3,
        buffer_id: 9,
        position: LogicalPosition { line: 42, col: 0 },
        open: None,
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({
            "kind": "view_element",
            "element": 3,
            "buffer_id": 9,
            "position": {"line": 42, "col": 0},
        })
    );
}

/// `view/save` reports a count, not a revision — there may be many documents, or none.
#[test]
fn view_save_wire_shape() {
    use aether_protocol::buffer::BufferSaveResult;
    use aether_protocol::viewport::{ViewSaveParams, ViewSaveResult};

    let p = ViewSaveParams {
        view_id: aether_protocol::ViewId(7),
        overwrite: false,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "view_id": 7, "overwrite": false })
    );

    // Nothing dirty: no `focused` on the wire at all.
    let r = ViewSaveResult {
        saved: 0,
        focused: None,
    };
    assert_eq!(to_value(&r).unwrap(), json!({ "saved": 0 }));

    // The focused document was one of them, so its own result rides along for the client to fold
    // into the buffer state it already tracks.
    let r = ViewSaveResult {
        saved: 3,
        focused: Some(BufferSaveResult {
            saved_at_unix_ms: 1_700_000_000_000,
            revision: 12,
        }),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({
            "saved": 3,
            "focused": { "saved_at_unix_ms": 1_700_000_000_000u64, "revision": 12 },
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
fn picker_kind_workspaces_is_snake_case() {
    use aether_protocol::picker::PickerKind;
    assert_eq!(
        to_value(PickerKind::Workspaces).unwrap(),
        json!("workspaces")
    );
    assert_eq!(
        from_value::<PickerKind>(json!("workspaces")).unwrap(),
        PickerKind::Workspaces,
    );
}

#[test]
fn picker_item_workspace_is_tagged() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::Workspace {
        name: "aether".into(),
        unsaved: 0,
        match_indices: vec![0, 4],
    };
    let v = to_value(&item).unwrap();
    // `unsaved` is omitted when zero.
    assert_eq!(
        v,
        json!({"kind": "workspace", "name": "aether", "match_indices": [0, 4]})
    );
}

#[test]
fn picker_item_workspace_carries_unsaved_count() {
    use aether_protocol::picker::PickerItem;
    let item = PickerItem::Workspace {
        name: "aether".into(),
        unsaved: 3,
        match_indices: vec![],
    };
    let v = to_value(&item).unwrap();
    assert_eq!(
        v,
        json!({"kind": "workspace", "name": "aether", "unsaved": 3, "match_indices": []})
    );
    // Round-trips back to the same value.
    let back: PickerItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn picker_select_result_workspace_is_tagged() {
    use aether_protocol::picker::PickerSelectResult;
    let r = PickerSelectResult::Workspace {
        name: "aether".into(),
    };
    assert_eq!(
        to_value(&r).unwrap(),
        json!({"kind": "workspace", "name": "aether"})
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
    use aether_protocol::picker::{PickerKind, PickerReset, PickerViewParams};
    let p = PickerViewParams {
        view_id: None,
        from_selection: false,
        kind: PickerKind::Explorer,
        reset: PickerReset::Keep,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor: None,
        directory_path: None,
        buffer_id: None,
        explorer_roots: false,
        filters: None,
        keybindings: None,
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("directory_path").is_none(),
        "None directory_path should be skipped from the wire"
    );
}

#[test]
fn picker_view_params_directory_path_serialized() {
    use aether_protocol::picker::{PickerKind, PickerReset, PickerViewParams};
    let p = PickerViewParams {
        view_id: None,
        from_selection: false,
        kind: PickerKind::Explorer,
        reset: PickerReset::All,
        offset: 0,
        limit: 30,
        center_on: None,
        center_on_cursor: None,
        directory_path: Some("/home/x/proj/src".into()),
        buffer_id: None,
        explorer_roots: false,
        filters: None,
        keybindings: None,
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
        path_filterable: false,
        truncated: false,

        collapsible: false,
        update: None,
    };
    let v = to_value(&r).unwrap();
    assert!(v.get("directory_path").is_none());
    assert!(v.get("directory_parent").is_none());
    assert!(v.get("effective_center_on").is_none());
    assert!(
        v.get("filters").is_none(),
        "all-default filters should be skipped from the wire"
    );
    assert!(
        v.get("path_filterable").is_none(),
        "false (the non-Jumplist default) should be skipped from the wire"
    );
    assert!(
        v.get("update").is_none(),
        "update: None should be skipped from the wire"
    );
    // Absent on the wire deserializes back to false — the pre-flag shape stays valid.
    let back: PickerViewResult = from_value(v).unwrap();
    assert!(!back.path_filterable);
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
        path_filterable: true,

        collapsible: false,
        update: None,
        truncated: false,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["directory_path"], "/proj/src");
    assert_eq!(v["directory_parent"], "/proj");
    assert_eq!(v["path_filterable"], true);
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
        regex: true,
        include_ignored: true,
        include_hidden: true,
        hide_ignored: true,
        hide_hidden: true,
        changed_only: true,
        hide_untracked: true,
        globs: vec!["*.rs".into(), "!*_test.rs".into()],
        directories: vec![
            ScopedPath {
                path_index: 1,
                relative_path: "src/app".into(),
                is_file: false,
            },
            ScopedPath {
                path_index: 0,
                relative_path: String::new(),
                is_file: false,
            },
            ScopedPath {
                path_index: 1,
                relative_path: "src/main.rs".into(),
                is_file: true,
            },
        ],
    };
    let v = to_value(&f).unwrap();
    assert_eq!(
        v,
        json!({
            "case": "insensitive",
            "whole_word": true,
            "regex": true,
            "include_ignored": true,
            "include_hidden": true,
            "hide_ignored": true,
            "hide_hidden": true,
            "changed_only": true,
            "hide_untracked": true,
            "globs": ["*.rs", "!*_test.rs"],
            "directories": [
                // A directory scope omits `is_file` (default false); a file scope carries it.
                {"path_index": 1, "relative_path": "src/app"},
                {"path_index": 0, "relative_path": ""},
                {"path_index": 1, "relative_path": "src/main.rs", "is_file": true},
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
        path_filterable: false,
        truncated: false,

        collapsible: false,
        update: None,
    };
    let v = to_value(&r).unwrap();
    assert_eq!(v["filters"], json!({"whole_word": true}));
}

#[test]
fn view_open_params_view_id_skipped_when_none() {
    use aether_protocol::view::ViewOpenParams;
    let p = ViewOpenParams {
        transient: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: None,
        ..Default::default()
    };
    let v = to_value(&p).unwrap();
    assert!(v.get("view_id").is_none());
    assert_eq!(v["path_index"], 0);
}

#[test]
fn view_open_params_view_id_round_trips() {
    use aether_protocol::view::ViewOpenParams;
    let p = ViewOpenParams {
        transient: None,
        view_id: Some(aether_protocol::ViewId(11)),
        path_index: None,
        relative_path: None,
        language: None,
        create_if_missing: false,
        jump_to: None,
        ..Default::default()
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["view_id"], 11);
    let back: ViewOpenParams = from_value(v).unwrap();
    assert_eq!(back.view_id, Some(aether_protocol::ViewId(11)));
}

#[test]
fn buffer_open_params_jump_to_skipped_when_none() {
    use aether_protocol::view::ViewOpenParams;
    let p = ViewOpenParams {
        transient: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: None,
        ..Default::default()
    };
    let v = to_value(&p).unwrap();
    assert!(v.get("jump_to").is_none());
}

#[test]
fn buffer_open_params_jump_to_round_trips() {
    use aether_protocol::view::ViewOpenParams;
    let p = ViewOpenParams {
        transient: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: Some(LogicalPosition { line: 7, col: 13 }),
        ..Default::default()
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
        path: Some("/p/bar.md".into()),
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["externally_modified"], true);
    assert_eq!(v["externally_deleted"], false);
    assert_eq!(v["path"], "/p/bar.md");
    let p2: BufferStateParams = from_value(v).unwrap();
    assert!(p2.externally_modified);
    assert!(!p2.externally_deleted);
    assert_eq!(p2.path.as_deref(), Some("/p/bar.md"));
    // Back-compat: a payload without `path` (older server) deserializes to `None`.
    let legacy = from_value::<BufferStateParams>(serde_json::json!({
        "buffer_id": 5, "saved_revision": 7,
    }))
    .unwrap();
    assert_eq!(legacy.path, None);
}

// ---- transient buffers ---------------------------------------------------------------------

/// `ViewOpenParams.transient` is a three-state intent: omitted = leave as-is, `true` =
/// transient-if-created, `false` = pin. Pin the skip-when-None shape and the round trip.
#[test]
fn buffer_open_params_transient_shape() {
    use aether_protocol::view::ViewOpenParams;
    let mut p = ViewOpenParams {
        transient: None,
        path_index: Some(0),
        relative_path: Some("x".into()),
        language: None,
        create_if_missing: false,
        jump_to: None,
        ..Default::default()
    };
    let v = to_value(&p).unwrap();
    assert!(
        v.get("transient").is_none(),
        "transient: None should be skipped"
    );

    p.transient = Some(true);
    let v = to_value(&p).unwrap();
    assert_eq!(v["transient"], true);
    let p2: ViewOpenParams = from_value(v).unwrap();
    assert_eq!(p2.transient, Some(true));

    // Missing on the wire deserialises as None (older clients).
    let p3: ViewOpenParams = from_value(json!({"path_index": 0, "relative_path": "x"})).unwrap();
    assert_eq!(p3.transient, None);
}

/// `transient` defaults to false when missing in `ViewOpenResult`, and rides `view/state` — never
/// `buffer/state` — once a view is open. Transience is the view's, and one buffer's views can
/// disagree, so a buffer-addressed push has no single answer to carry.
#[test]
fn transient_is_a_view_fact() {
    use aether_protocol::buffer::BufferStateParams;
    use aether_protocol::view::{ViewOpenResult, ViewState, ViewStateParams};
    let r: ViewOpenResult = from_value(json!({
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

    // A `buffer/state` payload still carrying the old key parses, and drops it.
    let s: BufferStateParams = from_value(json!({
        "buffer_id": 5,
        "saved_revision": 7,
        "saved_at_unix_ms": null,
        "transient": true
    }))
    .unwrap();
    assert!(
        !to_value(&s)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("transient"),
        "buffer/state does not carry a view's transient flag"
    );

    assert_eq!(ViewState::NAME, "view/state");
    let p = ViewStateParams {
        view_id: aether_protocol::ViewId(9),
        transient: true,
    };
    let v = to_value(p).unwrap();
    assert_eq!(v, json!({ "view_id": 9, "transient": true }));
    assert_eq!(from_value::<ViewStateParams>(v).unwrap(), p);
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

#[test]
fn cursor_select_all_params_wire_shape() {
    use aether_protocol::cursor::{CursorSelectAll, CursorSelectAllParams};
    assert_eq!(CursorSelectAll::NAME, "element/select_all");
    let p = CursorSelectAllParams { buffer_id: 7 };
    assert_eq!(to_value(&p).unwrap(), json!({ "buffer_id": 7 }));
    let back: CursorSelectAllParams = from_value(json!({ "buffer_id": 7 })).unwrap();
    assert_eq!(back.buffer_id, 7);
}

#[test]
fn cursor_swap_anchor_params_wire_shape() {
    use aether_protocol::cursor::{CursorSwapAnchor, CursorSwapAnchorParams};
    assert_eq!(CursorSwapAnchor::NAME, "element/swap_anchor");

    // `forward_only == false` is the default and stays off the wire, so pre-flag params still
    // parse and the plain swap's wire shape is unchanged.
    let p = CursorSwapAnchorParams {
        buffer_id: 7,
        forward_only: false,
    };
    assert_eq!(to_value(&p).unwrap(), json!({ "buffer_id": 7 }));
    let back: CursorSwapAnchorParams = from_value(json!({ "buffer_id": 7 })).unwrap();
    assert!(!back.forward_only);

    let p = CursorSwapAnchorParams {
        buffer_id: 7,
        forward_only: true,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "buffer_id": 7, "forward_only": true })
    );
}

#[test]
fn app_settings_wire_shape_and_defaults() {
    use aether_protocol::settings::AppSettings;
    use aether_protocol::viewport::WrapMode;

    // Default settings: soft wrap, ligatures on.
    assert_eq!(AppSettings::default().wrap, WrapMode::Soft);
    assert!(AppSettings::default().ligatures);

    // Wire shape: `wrap` and `theme` serialize as their lowercase tags; `ligatures` as a bool;
    // the two font sizes as numbers under their own keys.
    let s = AppSettings {
        wrap: WrapMode::None,
        ligatures: false,
        editor_font_size: 16,
        ui_font_size: 12,
        hints: false,
        markdown_read: false,
        markdown_width: aether_protocol::settings::MarkdownWidth::Full,
        theme: aether_protocol::settings::ThemeMode::Light,
        git_auto_fetch: true,
        worktree_store: String::new(),
    };
    assert_eq!(
        to_value(&s).unwrap(),
        json!({
            "wrap": "none",
            "ligatures": false,
            "editor_font_size": 16,
            "ui_font_size": 12,
            "hints": false,
            "markdown_read": false,
            "markdown_width": "full",
            "theme": "light",
            "git_auto_fetch": true,
        })
    );

    // A file with only `wrap` set (added before the others) reads back with those defaulting
    // (ligatures on, both font sizes at their defaults, hints on).
    let parsed: AppSettings = from_value(json!({ "wrap": "none" })).unwrap();
    assert_eq!(parsed.wrap, WrapMode::None);
    assert!(parsed.ligatures);
    // Off by default, and — the point of pinning it here — off for an *existing* settings file
    // written before this key existed. Unattended network access must never arrive by upgrade.
    assert!(
        !parsed.git_auto_fetch,
        "background fetch must not switch itself on for an existing settings.toml"
    );
    assert!(!AppSettings::default().git_auto_fetch);
    assert_eq!(
        parsed.editor_font_size,
        aether_protocol::settings::default_editor_font_size()
    );
    assert_eq!(
        parsed.ui_font_size,
        aether_protocol::settings::default_ui_font_size()
    );
    assert!(parsed.hints, "hints default on");
    assert!(parsed.markdown_read, "markdown reading view defaults on");
    assert_eq!(
        parsed.markdown_width,
        aether_protocol::settings::MarkdownWidth::Narrow,
        "reading width defaults narrow — the width the view has always had"
    );
    // Each option's wire tag, pinned: a settings.toml written by one client is read by every other.
    for (width, tag) in [
        (aether_protocol::settings::MarkdownWidth::Narrow, "narrow"),
        (aether_protocol::settings::MarkdownWidth::Wide, "wide"),
        (aether_protocol::settings::MarkdownWidth::Full, "full"),
    ] {
        assert_eq!(to_value(width).unwrap(), json!(tag));
        assert_eq!(
            from_value::<AppSettings>(json!({ "markdown_width": tag }))
                .unwrap()
                .markdown_width,
            width
        );
    }
    assert_eq!(
        parsed.theme,
        aether_protocol::settings::ThemeMode::Dark,
        "theme defaults dark"
    );

    // The pre-split `font_size` key is not read back as either size — it's gone, and a file still
    // carrying it lands on the defaults rather than silently applying the old value to one of them.
    let parsed: AppSettings = from_value(json!({ "font_size": 20 })).unwrap();
    assert_eq!(parsed, AppSettings::default());

    // An empty object (a fresh / older settings.toml with no keys) reads back as defaults — every
    // field carries a serde default so settings can be added without breaking old files.
    let parsed: AppSettings = from_value(json!({})).unwrap();
    assert_eq!(parsed, AppSettings::default());

    // `editor_font_size` was called `buffer_font_size` before views became the user-facing entity.
    // The old key still reads, so an existing settings.toml keeps the size its owner chose instead
    // of silently reverting to the default; the new key is the only one ever written.
    let parsed: AppSettings = from_value(json!({ "buffer_font_size": 20 })).unwrap();
    assert_eq!(parsed.editor_font_size, 20);
    assert!(
        !to_value(&parsed)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("buffer_font_size"),
        "the alias is read-only — serialization uses the new key alone"
    );

    // Full round-trip.
    let back: AppSettings = from_value(to_value(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

#[test]
fn hints_wire_shapes() {
    use aether_protocol::hints::{
        HintEvent, HintRecord, HintsRecord, HintsRecordParams, HintsRecordResult, HintsState,
        HintsStateResult,
    };

    assert_eq!(HintsRecord::NAME, "hints/record");
    assert_eq!(HintsState::NAME, "hints/state");

    // Events serialize as snake_case string tags.
    assert_eq!(to_value(HintEvent::Shown).unwrap(), json!("shown"));
    assert_eq!(to_value(HintEvent::Used).unwrap(), json!("used"));
    assert_eq!(to_value(HintEvent::Followed).unwrap(), json!("followed"));
    assert_eq!(to_value(HintEvent::Dismissed).unwrap(), json!("dismissed"));

    // hints/record params shape.
    let p = HintsRecordParams {
        hint_id: "picker-files".into(),
        event: HintEvent::Followed,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "hint_id": "picker-files", "event": "followed" })
    );

    // The result's `retired` defaults false when absent (an older server saying just `{}`).
    let r: HintsRecordResult = from_value(json!({})).unwrap();
    assert!(!r.retired);

    // Every record field carries a serde default so older files/servers parse forward.
    let rec: HintRecord = from_value(json!({})).unwrap();
    assert_eq!(rec, HintRecord::default());

    // hints/state result shape: retired ids + keyed records; empty object = empty state.
    let empty: HintsStateResult = from_value(json!({})).unwrap();
    assert_eq!(empty, HintsStateResult::default());
    let snap: HintsStateResult = from_value(json!({
        "retired": ["quit"],
        "active": {
            "copy": { "uses": 2, "use_days": 1, "last_used_day": 20646,
                      "last_used_at": 1783966210000u64,
                      "shows_without_follow": 1.5, "last_shown_at": 1783966200000u64 }
        }
    }))
    .unwrap();
    assert_eq!(snap.retired, vec!["quit".to_string()]);
    let copy = snap.active["copy"];
    assert_eq!(copy.uses, 2);
    assert!((copy.shows_without_follow - 1.5).abs() < f32::EPSILON);

    // Full round-trip through the wire encoding.
    let back: HintsStateResult = from_value(to_value(&snap).unwrap()).unwrap();
    assert_eq!(back, snap);
}

#[test]
fn app_info_wire_shapes() {
    use aether_protocol::app::{AppInfo, AppInfoGet, AppInfoParams, AppPaths};

    assert_eq!(AppInfoGet::NAME, "app/info");
    assert_eq!(to_value(AppInfoParams {}).unwrap(), json!({}));

    let info = AppInfo {
        version: "9.9.9".into(),
        commit: Some("abc1234".into()),
        commit_dirty: true,
        debug_build: true,
        appimage: Some("/apps/aether.AppImage".into()),
        profile: "dev".into(),
        port: Some(2385),
        pid: 4242,
        started_at_unix_ms: 1_700_000_000_000,
        uptime_secs: 61,
        idle_timeout_secs: Some(300),
        clients: 2,
        views_open: 5,
        documents_unsaved: 1,
        workspaces_active: 3,
        git_version: Some("git version 2.43.0".into()),
        paths: AppPaths {
            config_dir: Some("/c".into()),
            state_dir: Some("/s".into()),
        },
    };
    let v = to_value(&info).unwrap();
    // `version` must stay top-level and unnested: the web client reads it straight off `/status` to
    // decide its cached bundle is stale, and a *stale* bundle has to keep finding it on a *newer*
    // server's response. Nesting it would break the very check that triggers the reload.
    assert_eq!(v["version"], json!("9.9.9"));
    assert_eq!(v["paths"]["config_dir"], json!("/c"));
    let back: AppInfo = from_value(v).unwrap();
    assert_eq!(back, info);

    // Absent optionals stay off the wire rather than serializing as nulls.
    let bare = AppInfo {
        commit: None,
        appimage: None,
        port: None,
        idle_timeout_secs: None,
        // "git isn't available" — the client renders this absence as a warning row rather than
        // omitting it, so the missing key has to survive the round trip as `None`.
        git_version: None,
        paths: AppPaths::default(),
        ..info
    };
    let v = to_value(&bare).unwrap();
    for key in [
        "commit",
        "appimage",
        "port",
        "idle_timeout_secs",
        "git_version",
    ] {
        assert!(v.get(key).is_none(), "{key} should be omitted when absent");
    }
    assert_eq!(from_value::<AppInfo>(v.clone()).unwrap().git_version, None);
    assert_eq!(v["paths"], json!({}));

    // Every additive field defaults, so a client built against a newer protocol can still read an
    // older server's payload (and `ae server status` keeps working across an upgrade either way).
    let old: AppInfo = from_value(json!({
        "version": "0.1.0",
        "profile": "default",
        "pid": 7,
        "started_at_unix_ms": 0,
        "clients": 0,
        "views_open": 0,
        "documents_unsaved": 0,
        "workspaces_active": 0
    }))
    .unwrap();
    assert_eq!(old.commit, None);
    assert!(!old.commit_dirty);
    assert!(!old.debug_build);
    assert_eq!(old.uptime_secs, 0);
    assert_eq!(old.paths, AppPaths::default());
}

#[test]
fn jumplist_wire_shapes() {
    use aether_protocol::cursor::{CursorState, JumplistPosition};
    use aether_protocol::jumplist::{
        JumplistCapture, JumplistCaptureParams, JumplistClear, JumplistClearParams,
        JumplistClearResult, JumplistStep, JumplistStepParams, JumplistStepResult,
        JumplistStepScope, JumplistStepTarget,
    };
    use aether_protocol::picker::{PickerItem, PickerKind};

    assert_eq!(JumplistCapture::NAME, "jumplist/capture");
    assert_eq!(JumplistClear::NAME, "jumplist/clear");
    assert_eq!(JumplistStep::NAME, "jumplist/step");
    // The "re-view your open picker" push. Payload-free by design — see the type's docs — so an
    // empty object must parse, and must keep parsing if a field is ever added.
    assert_eq!(
        aether_protocol::jumplist::JumplistChanged::NAME,
        "jumplist/changed"
    );
    assert_eq!(
        to_value(aether_protocol::jumplist::JumplistChangedParams {}).unwrap(),
        json!({})
    );
    from_value::<aether_protocol::jumplist::JumplistChangedParams>(json!({})).unwrap();

    // jumplist/capture params: the highlighted item rides verbatim (the picker/select shape);
    // capture doesn't navigate, so there's no buffer_id.
    let p = JumplistCaptureParams {
        kind: PickerKind::Grep,
        item: PickerItem::GrepHit {
            path_index: 0,
            relative_path: "src/main.rs".into(),
            line: 4,
            col: 2,
            preview: "let x = 1;".into(),
            match_indices: vec![4],
        },
    };
    let v = to_value(&p).unwrap();
    assert_eq!(v["kind"], "grep");
    assert_eq!(v["item"]["kind"], "grep_hit");
    assert_eq!(v["item"]["relative_path"], "src/main.rs");

    // The capture result: entry count + the highlighted row's 0-based position.
    let r: aether_protocol::jumplist::JumplistCaptureResult =
        from_value(json!({ "total": 17, "index": 3 })).unwrap();
    assert_eq!((r.total, r.index), (17, 3));

    // jumplist/clear params: just the buffer to re-decorate a cursor for, omitted when there
    // isn't one — so an empty object is a valid clear.
    assert_eq!(
        to_value(JumplistClearParams { buffer_id: Some(7) }).unwrap(),
        json!({ "buffer_id": 7 })
    );
    assert_eq!(
        to_value(JumplistClearParams { buffer_id: None }).unwrap(),
        json!({})
    );
    let parsed: JumplistClearParams = from_value(json!({})).unwrap();
    assert!(parsed.buffer_id.is_none());

    // The clear result: the discarded count, and the caller's cursor with the `k/N` stamp already
    // gone. The cursor is skipped entirely when there was no buffer to decorate.
    let r = JumplistClearResult {
        cleared: 0,
        cursor: None,
    };
    assert_eq!(to_value(&r).unwrap(), json!({ "cleared": 0 }));
    let r: JumplistClearResult = from_value(json!({
        "cleared": 5,
        "cursor": { "position": {"line": 1, "col": 2}, "anchor": {"line": 1, "col": 2} }
    }))
    .unwrap();
    assert_eq!(r.cleared, 5);
    assert!(
        r.cursor
            .expect("a decorated cursor")
            .jumplist_position
            .is_none(),
        "the stamp is gone with the list"
    );

    // jumplist/step params: count stays off the wire at 1, `open` at false, and `scope` at Full;
    // all default back.
    let p = JumplistStepParams {
        buffer_id: 3,
        direction: Direction::Forward,
        count: 1,
        scope: JumplistStepScope::Full,
        open: false,
    };
    assert_eq!(
        to_value(&p).unwrap(),
        json!({ "buffer_id": 3, "direction": "forward" })
    );
    let parsed: JumplistStepParams = from_value(json!({
        "buffer_id": 3, "direction": "backward", "count": 2, "open": true
    }))
    .unwrap();
    assert_eq!(parsed.count, 2);
    assert!(parsed.open);
    assert_eq!(
        parsed.scope,
        JumplistStepScope::Full,
        "scope defaults to Full"
    );

    // The file-scoped variant (`Alt-]`) serializes its tag.
    let scoped = JumplistStepParams {
        buffer_id: 3,
        direction: Direction::Forward,
        count: 1,
        scope: JumplistStepScope::CurrentFile,
        open: false,
    };
    assert_eq!(
        to_value(&scoped).unwrap(),
        json!({ "buffer_id": 3, "direction": "forward", "scope": "current_file" })
    );

    // Step result: `Moved` is internally tagged (`status`), so the target's fields sit alongside
    // the tag. `anchor: None` and `opened: None` stay off the wire.
    let t = JumplistStepTarget {
        path: Some("/proj/src/main.rs".into()),
        view_id: None,
        position: Some(LogicalPosition { line: 4, col: 9 }),
        anchor: Some(LogicalPosition { line: 4, col: 2 }),
        index: 3,
        total: 17,
        opened: None,
        seat: None,
        skipped: 0,
    };
    let v = to_value(JumplistStepResult::Moved(Box::new(t))).unwrap();
    assert_eq!(
        v,
        json!({
            "status": "moved",
            "path": "/proj/src/main.rs",
            "position": {"line": 4, "col": 9},
            "anchor": {"line": 4, "col": 2},
            "index": 3,
            "total": 17,
        })
    );
    // Captured from a composed view still open: the element rides alongside the buffer and line,
    // so `]` seats the cursor in the view's window onto that file rather than opening the file over
    // the view. Absent on the wire when there is no element to seat in.
    use aether_protocol::viewport::ViewSeat;
    let seated = JumplistStepTarget {
        path: None,
        view_id: Some(aether_protocol::ViewId(9)),
        position: Some(LogicalPosition { line: 42, col: 0 }),
        anchor: None,
        index: 2,
        total: 4,
        opened: None,
        seat: Some(ViewSeat {
            element: 3,
            buffer_id: 9,
        }),
        skipped: 0,
    };
    assert_eq!(
        to_value(JumplistStepResult::Moved(Box::new(seated))).unwrap(),
        json!({
            "status": "moved",
            "view_id": 9,
            "position": {"line": 42, "col": 0},
            "index": 2,
            "total": 4,
            "seat": {"element": 3, "buffer_id": 9},
        })
    );

    // A whole-target step (a captured file or scratch): no position on the wire, and a pathless one
    // identifies by `view_id` instead — exactly one of the two is present.
    let whole_file = JumplistStepTarget {
        path: Some("/proj/src/main.rs".into()),
        view_id: None,
        position: None,
        anchor: None,
        index: 1,
        total: 4,
        opened: None,
        seat: None,
        skipped: 0,
    };
    assert_eq!(
        to_value(JumplistStepResult::Moved(Box::new(whole_file))).unwrap(),
        json!({
            "status": "moved",
            "path": "/proj/src/main.rs",
            "index": 1,
            "total": 4,
        })
    );
    let scratch = JumplistStepTarget {
        path: None,
        view_id: Some(aether_protocol::ViewId(9)),
        position: None,
        anchor: None,
        index: 2,
        total: 4,
        opened: None,
        seat: None,
        skipped: 0,
    };
    assert_eq!(
        to_value(JumplistStepResult::Moved(Box::new(scratch))).unwrap(),
        json!({
            "status": "moved",
            "view_id": 9,
            "index": 2,
            "total": 4,
        })
    );
    // The boundary / empty outcomes are bare tags.
    assert_eq!(
        to_value(JumplistStepResult::AtEnd).unwrap(),
        json!({ "status": "at_end" })
    );
    assert_eq!(
        to_value(JumplistStepResult::NoneInFile).unwrap(),
        json!({ "status": "none_in_file" })
    );
    assert_eq!(
        to_value(JumplistStepResult::Empty).unwrap(),
        json!({ "status": "empty" })
    );

    // The cursor's jumplist_position stamp: `{current, total}`, skipped when None.
    let c = CursorState {
        position: LogicalPosition { line: 4, col: 9 },
        anchor: LogicalPosition { line: 4, col: 2 },
        match_bracket: None,
        jumplist_position: Some(JumplistPosition {
            current: 3,
            total: 17,
        }),
    };
    let v = to_value(c).unwrap();
    assert_eq!(v["jumplist_position"], json!({"current": 3, "total": 17}));
    let bare = to_value(CursorState::default()).unwrap();
    assert!(
        bare.get("jumplist_position").is_none(),
        "jumplist_position: None should be skipped"
    );

    // The Jumplist picker's row shape: positional identity, line number, flat display text.
    let v = to_value(PickerItem::JumplistEntry {
        index: 4,
        line: Some(5),
        display: "let x = 1;".into(),
        match_indices: vec![0, 1],
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "kind": "jumplist_entry", "index": 4, "line": 5, "display": "let x = 1;", "match_indices": [0, 1] })
    );
    // A whole-target row (captured from the Files or view picker) carries no line at all —
    // the shells render nothing in its place rather than a fictional line 1.
    let v = to_value(PickerItem::JumplistEntry {
        index: 0,
        line: None,
        display: "src/main.rs".into(),
        match_indices: vec![],
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "kind": "jumplist_entry", "index": 0, "display": "src/main.rs", "match_indices": [] })
    );
    assert_eq!(to_value(PickerKind::Jumplist).unwrap(), json!("jumplist"));
}

#[test]
fn block_edit_wire_shapes() {
    use aether_protocol::cursor::VerticalDirection;
    use aether_protocol::input::{BlockEditResult, BlockUnit, MoveBlockParams, PasteBlockParams};

    // The move params: unit rides as a snake_case string.
    let v = to_value(MoveBlockParams {
        buffer_id: 3,
        direction: VerticalDirection::Down,
        unit: BlockUnit::Paragraph,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "buffer_id": 3, "direction": "down", "unit": "paragraph" })
    );
    assert_eq!(to_value(BlockUnit::Block).unwrap(), json!("block"));

    let v = to_value(PasteBlockParams {
        buffer_id: 3,
        text: "New block.".into(),
        replace: true,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "buffer_id": 3, "text": "New block.", "replace": true })
    );

    // The open params: direction as a bare flag, like the depth pair's `deeper`.
    use aether_protocol::input::OpenBlockParams;
    let v = to_value(OpenBlockParams {
        buffer_id: 3,
        above: true,
    })
    .unwrap();
    assert_eq!(v, json!({ "buffer_id": 3, "above": true }));

    // The shared result: reason and text (delete's clipboard) skip when absent.
    let v = to_value(BlockEditResult {
        buffer: 3,
        applied: false,
        reason: None,
        revision: 7,
        cursor: CursorState::default(),
        text: None,
    })
    .unwrap();
    assert!(v.get("reason").is_none(), "quiet refusal has no reason");
    assert!(v.get("text").is_none());
    let v = to_value(BlockEditResult {
        buffer: 3,
        applied: false,
        reason: Some("Front matter stays at the top".into()),
        revision: 7,
        cursor: CursorState::default(),
        text: Some("Beta.\n".into()),
    })
    .unwrap();
    assert_eq!(v["reason"], json!("Front matter stays at the top"));
    assert_eq!(v["text"], json!("Beta.\n"));

    // The checkbox params: `set` names the state wanted (Ctrl-a / Ctrl-Alt-a) and is absent for a
    // plain flip (Enter), which is also how an older client's payload decodes.
    use aether_protocol::input::ToggleTaskParams;
    let v = to_value(ToggleTaskParams {
        buffer_id: 3,
        set: Some(true),
    })
    .unwrap();
    assert_eq!(v, json!({ "buffer_id": 3, "set": true }));
    let v = to_value(ToggleTaskParams {
        buffer_id: 3,
        set: None,
    })
    .unwrap();
    assert_eq!(v, json!({ "buffer_id": 3 }));
    let p: ToggleTaskParams = serde_json::from_value(json!({ "buffer_id": 3 })).unwrap();
    assert_eq!(p.set, None, "a missing `set` flips");
}

#[test]
fn worktree_add_params_round_trip() {
    use aether_protocol::git::GitWorktreeAddParams;
    // The minimal shape: resolution left to the server, an existing branch.
    let v = to_value(GitWorktreeAddParams {
        branch: "feature/auth".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v, json!({ "branch": "feature/auth" }));
    // `create_branch` is the only flag, and it is absent when false.
    let v = to_value(GitWorktreeAddParams {
        repo_id: Some("/src/aether".into()),
        buffer_id: Some(7),
        branch: "wip".into(),
        create_branch: true,
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "repo_id": "/src/aether",
            "buffer_id": 7,
            "branch": "wip",
            "create_branch": true,
        })
    );
}

#[test]
fn worktree_add_result_shape() {
    use aether_protocol::git::{
        GitHead, GitWorktreeAddResult, GitWorktreeAddStatus, GitWorktreeRow,
    };
    // A plain success carries only the status and the row — every count and flag is skipped when
    // it has nothing to say, so the common case stays small.
    let v = to_value(GitWorktreeAddResult {
        status: GitWorktreeAddStatus::Created,
        worktree: Some(GitWorktreeRow {
            name: "feature-auth".into(),
            path: "/store/aether-3f9c/feature-auth".into(),
            head: Some(GitHead::Branch {
                name: "feature/auth".into(),
                upstream: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        v,
        json!({
            "status": "created",
            "worktree": {
                "name": "feature-auth",
                "path": "/store/aether-3f9c/feature-auth",
                "head": { "state": "branch", "name": "feature/auth" },
            },
        })
    );
    // The main worktree has no admin name and a prunable row has no head — both are *absent*, and
    // the row must decode that way rather than needing a sentinel.
    let row: GitWorktreeRow =
        serde_json::from_value(json!({ "path": "/src/aether", "is_main": true })).unwrap();
    assert_eq!(row.name, "");
    assert_eq!(row.head, None);
    assert!(row.is_main);
}

#[test]
fn worktree_remove_itemises_what_is_at_risk() {
    use aether_protocol::git::{
        GitWorktreeAtRisk, GitWorktreeRemoveResult, GitWorktreeRemoveStatus,
    };
    let v = to_value(GitWorktreeRemoveResult {
        status: GitWorktreeRemoveStatus::Dirty,
        at_risk: Some(GitWorktreeAtRisk {
            modified: 2,
            untracked: 1,
            operation_in_progress: false,
        }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        v,
        json!({ "status": "dirty", "at_risk": { "modified": 2, "untracked": 1 } })
    );
    // A clean removal says nothing further — no zeroed counts to read as "1 file at risk".
    let v = to_value(GitWorktreeRemoveResult {
        status: GitWorktreeRemoveStatus::Removed,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(v, json!({ "status": "removed" }));
}

#[test]
fn app_settings_carry_a_path_and_stay_backward_compatible() {
    use aether_protocol::settings::AppSettings;
    // The empty default is skipped, so a settings file that has never set a store round-trips
    // byte-identically to one written before the key existed.
    let v = to_value(AppSettings::default()).unwrap();
    assert!(v.get("worktree_store").is_none());
    // And an older file (no key at all) still parses — every field carries a serde default.
    let old = json!({ "wrap": "soft", "ligatures": true });
    let parsed: AppSettings = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.worktree_store, "");

    let with = AppSettings {
        worktree_store: "/mnt/fast/worktrees".into(),
        ..Default::default()
    };
    assert_eq!(
        to_value(&with).unwrap()["worktree_store"],
        json!("/mnt/fast/worktrees")
    );
}

/// `MUTATES_TEXT` is what the client's request funnel refuses a read-only buffer on, so the two
/// deliberate answers are worth pinning: the flag means "changes the text of the buffer it
/// *names*", not "writes text somewhere".
///
/// `git/apply_hunk` is the exception that makes the distinction load-bearing. Invoked on a patch
/// view it stages into a *different* buffer, so marking it — as a sweep over "everything that
/// writes text" would — silently breaks staging from the working-changes view, the one thing a
/// read-only buffer is legitimately the subject of.
///
/// Asserted in `const` blocks: the flags are compile-time facts, so getting one wrong should fail
/// the build rather than one test run. Nothing here executes.
// A constant asserted to be what it is reads to clippy as a tautology, which is the point here.
// The pinned toolchain's clippy fires on that even inside `const {}`; newer ones don't — so this
// is `allow` rather than `expect`, which would itself warn on the toolchains that stay quiet.
#[allow(clippy::assertions_on_constants)]
#[test]
fn mutates_text_marks_the_buffer_a_method_names() {
    use aether_protocol::buffer::{BufferContent, BufferCut};
    use aether_protocol::cursor::CursorMove;
    use aether_protocol::envelope::RpcMethod;
    use aether_protocol::git::{GitApplyHunk, GitResolveConflict};
    use aether_protocol::input::{EditUndo, InputMoveLines, InputText};

    const { assert!(InputText::MUTATES_TEXT) };
    const { assert!(InputMoveLines::MUTATES_TEXT, "the whole point") };
    const { assert!(EditUndo::MUTATES_TEXT) };
    const {
        assert!(
            BufferCut::MUTATES_TEXT,
            "not every mutator returns EditResult"
        )
    };
    const {
        assert!(
            GitResolveConflict::MUTATES_TEXT,
            "rewrites the buffer it names"
        )
    };

    const {
        assert!(
            !GitApplyHunk::MUTATES_TEXT,
            "staging from a patch view writes a different buffer than the one it names"
        )
    };
    const {
        assert!(
            !CursorMove::MUTATES_TEXT,
            "reading a revision must still work"
        )
    };
    const { assert!(!BufferContent::MUTATES_TEXT) };
}

// ---- the generic round trip ---------------------------------------------------------------------

/// Serialize → deserialize → serialize, and assert the JSON is identical both times.
///
/// Catches the asymmetric-serde family — a field that serializes under one name and deserializes
/// under another, a `skip_serializing_if` with no matching `default`, an enum whose tagging differs
/// by direction — without needing `PartialEq` on the type, which most params structs don't derive.
/// It does **not** replace the golden-JSON tests above: those pin the shape a client actually sees,
/// and would still fail if a field were renamed on both sides at once.
/// Round-trip `value`, and assert its wire object has exactly these keys.
///
/// The key set is the half [`round_trips`] cannot see: it re-serializes what it just deserialized,
/// so a field *rename* passes straight through — both directions rename together. Names are a live
/// contract because the browser client parses them by hand in TypeScript (`buffer_id`,
/// `viewport_id`, `relative_path` and friends appear as string literals there), where a Rust-side
/// rename compiles cleanly on both Rust sides and breaks only at runtime, in the shell no Rust test
/// covers.
fn wire_keys<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T, expected: &[&str]) {
    round_trips(value);
    let v = to_value(value).expect("serializes");
    let mut got: Vec<&str> = v
        .as_object()
        .expect("params and results are objects")
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "the wire key set changed");
}

fn round_trips<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) {
    let first = to_value(value).expect("serializes");
    let parsed: T = from_value(first.clone()).expect("deserializes from its own output");
    let second = to_value(&parsed).expect("re-serializes");
    assert_eq!(first, second, "round trip changed the wire shape");
}

// ---- viewport/* results (the highest-bandwidth type on the wire) --------------------------------

fn sample_window() -> aether_protocol::viewport::Window {
    use aether_protocol::viewport::{Highlight, Segment, Window, WrappedRow};
    Window {
        other_elements_dirty: false,
        max_line_width: 88,
        git_status: None,
        root: Element::Editor {
            element: 0,
            buffer: 7,
            rows: 130,
            first_row: ElementRow(5),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            first_buffer_line: 4,
            lines: vec![LogicalLineRender {
                logical_line: 4,
                visual_rows: vec![WrappedRow {
                    byte_offset: 0,
                    continuation_indent: 0,
                    segments: vec![Segment {
                        text: "fn main() {".into(),
                        highlights: vec![Highlight {
                            start: 0,
                            end: 2,
                            kind: "keyword".into(),
                        }],
                    }],
                }],
                search_matches: Vec::new(),
                baseline_above: Vec::new(),
                change: Default::default(),
                diagnostics: Vec::new(),
                sneak_targets: Vec::new(),
            }],
        },
    }
}

/// Every rendered row, highlight and diff marker a client ever sees rides this type, and until now
/// nothing pinned its shape. All four viewport methods return it.
#[test]
fn viewport_window_result_wire_shape() {
    use aether_protocol::viewport::ViewportWindowResult;
    let r = ViewportWindowResult {
        window: sample_window(),
    };
    let v = to_value(&r).unwrap();
    // No view-space geometry on the window itself: the client lays the view out from the tree,
    // where each editor says how tall it is and where its loaded slice starts within it.
    for gone in [
        "first_view_line",
        "last_view_line_exclusive",
        "view_line_count",
        "max_scroll_view_line",
        "total_visual_rows",
        "first_visual_row",
    ] {
        assert!(
            v["window"].get(gone).is_none(),
            "{gone} is not a wire field"
        );
    }
    assert_eq!(v["window"]["max_line_width"], 88);
    assert_eq!(v["window"]["root"]["rows"], 130);
    assert_eq!(v["window"]["root"]["first_row"], 5);
    assert_eq!(v["window"]["root"]["first_buffer_line"], 4);
    assert!(
        v["window"].get("git_status").is_none(),
        "absent outside a repo rather than null"
    );
    assert_eq!(
        v["window"]["root"]["node"], "editor",
        "an ordinary buffer is one element"
    );
    let row = &v["window"]["root"]["lines"][0]["visual_rows"][0];
    assert_eq!(row["byte_offset"], 0);
    assert_eq!(row["segments"][0]["text"], "fn main() {");
    assert_eq!(row["segments"][0]["highlights"][0]["kind"], "keyword");
    // Empty per-line extras stay off the wire — they ride every rendered line, so this is the
    // difference between a compact frame and a bloated one.
    let line = &v["window"]["root"]["lines"][0];
    for absent in [
        "search_matches",
        "baseline_above",
        "change",
        "diagnostics",
        "sneak_targets",
    ] {
        assert!(
            line.get(absent).is_none(),
            "{absent} should be skipped when empty"
        );
    }
    round_trips(&r);
}

/// The viewport methods' params.
#[test]
fn viewport_params_round_trip() {
    use aether_protocol::viewport::{
        ScrollPosition, SliceRequest, ViewportResizeParams, ViewportSetWrapParams, ViewportWindow,
        ViewportWindowParams,
    };
    wire_keys(
        &ViewportResizeParams {
            viewport_id: 3,
            cols: 100,
            rows: 40,
        },
        &["viewport_id", "cols", "rows"],
    );
    // The one scroll request: the slices the client's viewport reaches, each by row within its
    // element, plus where the top is as content so a reopen can restore it.
    assert_eq!(ViewportWindow::NAME, "view/window");
    let anchor = ScrollPosition {
        element: 1,
        line: 12,
        sub_row: 0.5,
    };
    let slice = SliceRequest {
        element: 1,
        from_row: ElementRow(40),
        rows: 60,
    };
    wire_keys(&anchor, &["element", "line", "sub_row"]);
    wire_keys(&slice, &["element", "from_row", "rows"]);
    let p = ViewportWindowParams {
        viewport_id: 3,
        anchor,
        slices: vec![slice],
    };
    wire_keys(&p, &["viewport_id", "anchor", "slices"]);
    let v = to_value(&p).unwrap();
    assert_eq!(
        v["slices"][0]["from_row"], 40,
        "an element row is a bare number"
    );
    round_trips(&p);
    wire_keys(
        &ViewportSetWrapParams {
            viewport_id: 3,
            wrap: aether_protocol::viewport::WrapMode::Soft,
        },
        &["viewport_id", "wrap"],
    );
}

/// Who lays an editor element out rides the wire only when it is the client — the ordinary
/// server-wrapped editor is unchanged — and the browser mirror declares the key.
#[test]
fn an_editor_says_when_the_client_lays_it_out() {
    use aether_protocol::ui::{Element, LayoutOwner};
    let editor = |laid_out_by| Element::Editor {
        element: 0,
        buffer: 1,
        rows: 4,
        first_row: ElementRow(0),
        laid_out_by,
        first_buffer_line: 0,
        lines: Vec::new(),
    };
    let server = to_value(editor(LayoutOwner::Server)).unwrap();
    assert!(
        server.get("laid_out_by").is_none(),
        "the default stays off the wire"
    );
    let client = to_value(editor(LayoutOwner::Client)).unwrap();
    assert_eq!(client["laid_out_by"], "client");
    let back: Element = from_value(client).unwrap();
    assert!(matches!(
        back,
        Element::Editor {
            laid_out_by: LayoutOwner::Client,
            ..
        }
    ));
    let ts = include_str!("../../../web/src/protocol.ts");
    assert!(
        ts.contains("laid_out_by?:"),
        "web/src/protocol.ts must declare the editor node's `laid_out_by`"
    );
}

/// A step's `Gone` outcome and the count of entries a landing stepped over — both new, both
/// mirrored nowhere but here.
#[test]
fn jumplist_step_reports_gone_entries() {
    use aether_protocol::jumplist::JumplistStepResult;
    let v = to_value(JumplistStepResult::Gone {
        index: 2,
        total: 4,
        skipped: 1,
        opened: None,
    })
    .unwrap();
    assert_eq!(v["status"], "gone");
    assert_eq!(v["skipped"], 1);
    assert!(v.get("opened").is_none());
    let back: JumplistStepResult = from_value(v).unwrap();
    assert!(back.moved().is_none(), "gone is not a move");
    // An ordinary step passed over nothing, and says nothing about it.
    let plain = to_value(JumplistStepResult::Moved(Box::new(
        aether_protocol::jumplist::JumplistStepTarget {
            path: None,
            view_id: Some(aether_protocol::ViewId(3)),
            position: None,
            anchor: None,
            index: 1,
            total: 1,
            opened: None,
            seat: None,
            skipped: 0,
        },
    )))
    .unwrap();
    assert!(plain.get("skipped").is_none(), "zero stays off the wire");
    let gone_row =
        to_value(aether_protocol::picker::PickerSelectResult::Gone { open: None }).unwrap();
    assert_eq!(gone_row["kind"], "gone");
}

/// `view/navigate_change` is what both `c` and `o` send, whatever the view: its grain and extend
/// flag stay off the wire at their defaults, and an older server reads a plain `c`.
#[test]
fn navigate_change_params_keep_their_defaults_off_the_wire() {
    use aether_protocol::viewport::{
        FocusStep, NavigateGrain, ViewportNavigateChange, ViewportNavigateChangeParams,
    };
    assert_eq!(ViewportNavigateChange::NAME, "view/navigate_change");
    let plain = ViewportNavigateChangeParams {
        viewport_id: 3,
        direction: FocusStep::Next,
        count: None,
        grain: NavigateGrain::Change,
        extend: false,
    };
    wire_keys(&plain, &["viewport_id", "direction"]);
    let shifted = ViewportNavigateChangeParams {
        viewport_id: 3,
        direction: FocusStep::Previous,
        count: Some(2),
        grain: NavigateGrain::Outline,
        extend: true,
    };
    wire_keys(
        &shifted,
        &["viewport_id", "direction", "count", "grain", "extend"],
    );
    let v = to_value(&shifted).unwrap();
    assert_eq!(v["grain"], "outline");
    assert_eq!(v["direction"], "previous");
    round_trips(&shifted);
}

/// The remaining methods that had no wire coverage at all.
#[test]
fn previously_unpinned_params_round_trip() {
    use aether_protocol::buffer::{BufferCopyParams, BufferSaveParams, CopyScope};
    use aether_protocol::cursor::CursorUndoParams;
    use aether_protocol::git::{GitCancelParams, GitResetParams};
    use aether_protocol::picker::PickerHideParams;
    use aether_protocol::search::{SearchClearParams, SearchStepParams};

    wire_keys(
        &BufferSaveParams {
            buffer_id: 1,
            path_index: Some(0),
            relative_path: Some("a/b.rs".into()),
            overwrite: true,
        },
        &["buffer_id", "path_index", "relative_path", "overwrite"],
    );
    // `view/close`: the composite result carries the follow-on open, and both optional halves
    // stay off the wire when the close was a plain one.
    {
        use aether_protocol::view::{ViewCloseParams, ViewCloseResult};
        let plain = ViewCloseResult {
            next_view_id: None,
            opened: None,
        };
        let v = to_value(&plain).unwrap();
        assert_eq!(v, json!({}), "a plain close is an empty object");
        round_trips(&plain);
        wire_keys(
            &ViewCloseParams {
                view_id: aether_protocol::ViewId(7),
                open_next: true,
            },
            &["view_id", "open_next"],
        );
    }
    wire_keys(
        &BufferCopyParams {
            buffer_id: 1,
            scope: CopyScope::Selection,
        },
        &["buffer_id", "scope"],
    );
    wire_keys(
        &CursorUndoParams {
            buffer_id: 1,
            count: 1,
        },
        &["buffer_id"],
    );
    wire_keys(
        &GitCancelParams {
            repo_id: "/repo".into(),
        },
        &["repo_id"],
    );
    wire_keys(
        &GitResetParams {
            repo_id: Some("/repo".into()),
            buffer_id: None,
            rev: "HEAD~1".into(),
        },
        &["repo_id", "rev"],
    );
    wire_keys(
        &PickerHideParams {
            kind: aether_protocol::picker::PickerKind::Files,
        },
        &["kind"],
    );
    wire_keys(&SearchClearParams { buffer_id: 1 }, &["buffer_id"]);
    // `search/step` skips its defaults hard: `direction` is absent when Forward and `options` when
    // default, while `extend` is never skipped. Both branches are pinned, so a name can't drift on
    // the side that happens not to be exercised.
    wire_keys(
        &SearchStepParams {
            buffer_id: 1,
            direction: aether_protocol::cursor::Direction::Forward,
            extend: false,
            count: 2,
            set_query: Some("needle".into()),
            options: MatchOptions::default(),
        },
        &["buffer_id", "extend", "count", "set_query"],
    );
    wire_keys(
        &SearchStepParams {
            buffer_id: 1,
            direction: aether_protocol::cursor::Direction::Backward,
            extend: true,
            count: 1,
            set_query: None,
            options: MatchOptions {
                regex: true,
                ..MatchOptions::default()
            },
        },
        &["buffer_id", "direction", "extend", "options"],
    );
}

/// The browser shell hand-mirrors the wire types in TypeScript, and `tsc` cannot know when the Rust
/// side renames a field — it only checks the mirror against itself. So a rename lands, the bundle
/// builds, the types check, and the web client silently reads `undefined` for a field that moved.
///
/// That is not hypothetical: `Window` gained view-space names (`first_view_line`, `view_line_count`,
/// `max_scroll_view_line`) and `Element::Editor`'s line became `first_buffer_line`, while
/// `web/src/protocol.ts` still declared every old name and type-checked clean.
///
/// One-directional, like the theme's CSS check: the mirror may carry extra fields (it declares
/// client-only shapes too), but every key Rust actually *serialises* must appear in it.
#[test]
fn the_typescript_mirror_declares_every_field_the_window_puts_on_the_wire() {
    use aether_protocol::viewport::{Element, Window};

    let ts = include_str!("../../../web/src/protocol.ts");
    let window = Window {
        other_elements_dirty: false,
        max_line_width: 0,
        // `None` would be skipped, and a field that never serialises cannot be checked.
        git_status: Some(Default::default()),
        root: Element::Editor {
            element: 0,
            buffer: 1,
            rows: 1,
            first_row: ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            first_buffer_line: 0,
            lines: Vec::new(),
        },
    };
    let value = to_value(&window).unwrap();
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "root")
        .cloned()
        .collect();
    // The editor node's own keys travel inside `root`, and are just as easy to miss.
    keys.extend(
        value["root"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| *k != "node")
            .cloned(),
    );

    let missing: Vec<&String> = keys
        .iter()
        .filter(|k| !ts.contains(&format!("{k}:")) && !ts.contains(&format!("{k}?:")))
        .collect();
    assert!(
        missing.is_empty(),
        "wire fields with no declaration in web/src/protocol.ts: {missing:?}"
    );
}

/// Every subscribe carries the focus the server resolved — element 0 for an ordinary view, the
/// element under the scroll for a composed one.
///
/// Unconditional, because the client mirrors the focused element and has to start from the server's
/// value whichever it is; an answer omitted "when there is nothing to reconcile" left a subscribe
/// that landed in a patch's own text with the two sides on different elements. For a composed view
/// it is also what stops a client holding two line spaces at once — its cursor in the view's own
/// document while every rendered line belongs to a file. Pinned here because the browser shell
/// hand-mirrors these types and `tsc` cannot see a Rust rename.
#[test]
fn every_subscribe_carries_the_focus_it_resolved() {
    use aether_protocol::viewport::{ViewportFocusElementResult, ViewportSubscribeResult, Window};
    let window = || Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::Editor {
            element: 0,
            buffer: 9,
            rows: 1,
            first_row: ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            first_buffer_line: 17,
            lines: vec![],
        },
    };

    let focus_on = |element: u32| ViewportFocusElementResult {
        element,
        buffer: aether_protocol::view::BufferDescription {
            buffer_id: 9,
            language: None,
            line_count: 40,
            byte_count: 400,
            revision: 1,
            saved_revision: 1,
            path: Some("/repo/a.rs".into()),
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            read_only: false,
            is_patch: false,
        },
        buffer_status: Default::default(),
    };

    let ordinary = ViewportSubscribeResult {
        viewport_id: 1,
        window: window(),
        buffer_status: Default::default(),
        focus: focus_on(0),
    };
    let v = to_value(&ordinary).unwrap();
    assert_eq!(
        v["focus"]["element"], 0,
        "an ordinary view still names its one element: {v}"
    );

    let composed = ViewportSubscribeResult {
        viewport_id: 1,
        window: window(),
        buffer_status: Default::default(),
        focus: focus_on(2),
    };
    let v = to_value(&composed).unwrap();
    assert_eq!(v["focus"]["element"], 2);
    assert_eq!(v["focus"]["buffer"]["buffer_id"], 9);
    let back: ViewportSubscribeResult = from_value(v).unwrap();
    assert_eq!(back.focus.element, 2);
}

/// `view/window_at_cursor` takes nothing but the viewport: both halves of the question — where the
/// cursor is and how the view is laid out — live on the server, so no coordinate crosses the wire.
#[test]
fn window_at_cursor_names_no_coordinates() {
    use aether_protocol::viewport::{ViewportWindowAtCursor, ViewportWindowAtCursorParams};
    assert_eq!(ViewportWindowAtCursor::NAME, "view/window_at_cursor");
    let v = to_value(ViewportWindowAtCursorParams { viewport_id: 3 }).unwrap();
    assert_eq!(v, json!({ "viewport_id": 3 }));
}
