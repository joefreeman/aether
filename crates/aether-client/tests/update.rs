//! The sans-IO payoff (docs/client-core.md): the update loop tested as a pure state
//! machine — key events in, `Effect::Request`s out, canned JSON results back in — with no
//! transport, no mock, no async runtime.

use aether_client::effect::{Effect, Effects, ShellAction, ToastKind};
use aether_client::keymap::{KeyCode, Mods};
use aether_client::session::Session;
use aether_client::transport::RpcError;
use serde_json::json;

const ROWS: u32 = 40;

fn session() -> Session {
    Session::placeholder()
}

fn key(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::NONE, Some(c.to_string()), ROWS)
}

fn ctrl(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::CTRL, None, ROWS)
}

/// The single `Effect::Request` in `fx` (panics otherwise — these tests pin exact traffic).
fn the_request(fx: &Effects) -> (u64, &'static str, serde_json::Value) {
    let mut reqs = fx.0.iter().filter_map(|e| match e {
        Effect::Request {
            token,
            method,
            params,
        } => Some((*token, *method, params.clone())),
        _ => None,
    });
    let req = reqs.next().expect("an Effect::Request");
    assert!(reqs.next().is_none(), "exactly one request expected");
    req
}

fn has_error_toast(fx: &Effects) -> bool {
    fx.0.iter()
        .any(|e| matches!(e, Effect::Toast(_, ToastKind::Error)))
}

#[test]
fn insert_entry_is_one_selection_edge_request() {
    let mut s = session();
    let fx = key(&mut s, 'i');
    assert_eq!(s.mode, aether_client::session::Mode::Insert);

    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(
        params["motion"],
        json!({"kind": "selection_edge", "edge": "start"})
    );
    assert_eq!(params["extend_selection"], json!(false));

    // The canned result lands as the cursor.
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "position": {"line": 2, "col": 5},
            "anchor": {"line": 2, "col": 5},
        })),
    );
    assert_eq!(s.buffer.cursor.position.line, 2);
    assert_eq!(s.buffer.cursor.position.col, 5);
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::RevealCursor(_))),
        "a cursor move reveals the cursor"
    );
}

/// The reveal style of the single `RevealCursor` effect in `fx`, if any.
fn reveal_style(fx: &Effects) -> Option<aether_client::effect::RevealStyle> {
    fx.0.iter().find_map(|e| match e {
        Effect::RevealCursor(style) => Some(*style),
        _ => None,
    })
}

#[test]
fn ordinary_motion_follows_but_goto_line_jumps() {
    use aether_client::effect::RevealStyle;
    let cursor = json!({ "position": {"line": 9, "col": 0}, "anchor": {"line": 9, "col": 0} });

    // A plain motion (`j`) reveals as a Follow — minimal scroll.
    let mut s = session();
    let token = the_request(&key(&mut s, 'j')).0;
    let fx = s.on_rpc_result(token, Ok(cursor.clone()));
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Follow));

    // Go-to-line (`g`) is a targeted jump — reveals as a Jump (rest a quarter down).
    let mut s = session();
    let token = the_request(&key(&mut s, 'g')).0;
    let fx = s.on_rpc_result(token, Ok(cursor));
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
}

#[test]
fn goto_line_from_end_counts_up_from_the_bottom() {
    use aether_protocol::viewport::Window;
    // The client needs the buffer's line count (carried on the window) to count from the bottom.
    let mut s = session();
    s.window = Some(Window {
        first_logical_line: 0,
        last_logical_line_exclusive: 40,
        line_count: 100,
        max_scroll_logical_line: 60,
        total_visual_rows: 100,
        first_visual_row: 0,
        max_line_width: 0,
        git_status: None,
        lines: vec![],
    });

    let goto_line = |s: &mut Session| -> u64 {
        let fx = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
        let (_, method, params) = the_request(&fx);
        assert_eq!(method, "cursor/move");
        assert_eq!(params["motion"]["kind"], "goto");
        params["motion"]["position"]["line"].as_u64().unwrap()
    };

    // Bare `Alt-g` (count 1) lands on the last line (index 99).
    assert_eq!(goto_line(&mut s), 99);
    // `3 Alt-g` is three lines up from the end: 100 - 3 = 97.
    let _ = key(&mut s, '3');
    assert_eq!(goto_line(&mut s), 97);
}

#[test]
fn search_and_diagnostic_navigation_reveal_as_jumps() {
    use aether_client::effect::RevealStyle;
    use aether_client::update::Event;

    // Search next/prev (`n`/`N`) jumps to the match.
    let mut s = session();
    let fx = s.on_event(Event::SearchNav(Ok(serde_json::from_value(json!({
        "cursor": { "position": {"line": 20, "col": 0}, "anchor": {"line": 20, "col": 0} },
        "summary": { "buffer_id": 0, "total": 3, "truncated": false, "current_index": 1 },
    }))
    .unwrap())));
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));

    // Diagnostic next/prev (`d`/`Alt-d`) jumps to the diagnostic.
    let mut s = session();
    let fx = s.on_event(Event::DiagNav(Ok(serde_json::from_value(json!({
        "cursor": { "position": {"line": 31, "col": 2}, "anchor": {"line": 31, "col": 2} },
        "moved": true,
    }))
    .unwrap())));
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
}

#[test]
fn shift_extends_hunk_and_diagnostic_navigation() {
    // Plain `c`/`d` collapse to the target (no extend on the wire); Shift grows the selection.
    let press = |c: char, mods: Mods| -> serde_json::Value {
        let mut s = session();
        let fx = s.on_key(KeyCode::Char(c), mods, None, ROWS);
        the_request(&fx).2
    };

    // `c` → git/navigate_hunk, no extend; `Shift-c` → extend: true.
    assert_eq!(press('c', Mods::NONE)["extend"], json!(null));
    assert_eq!(press('c', Mods::SHIFT)["extend"], json!(true));
    // `Alt-c` (prev) likewise gains extend under Shift-Alt.
    let shift_alt = Mods {
        shift: true,
        ..Mods::ALT
    };
    assert_eq!(press('c', shift_alt)["extend"], json!(true));

    // Same for diagnostics (`d` → lsp/navigate_diagnostic).
    assert_eq!(press('d', Mods::NONE)["extend"], json!(null));
    assert_eq!(press('d', Mods::SHIFT)["extend"], json!(true));
    assert_eq!(press('d', shift_alt)["extend"], json!(true));
}

#[test]
fn shift_extends_symbol_navigation() {
    let press = |mods: Mods| -> serde_json::Value {
        let mut s = session();
        let fx = s.on_key(KeyCode::Char('o'), mods, None, ROWS);
        let (_, method, params) = the_request(&fx);
        assert_eq!(method, "cursor/move");
        params
    };
    let shift_alt = Mods {
        shift: true,
        ..Mods::ALT
    };
    // `o`/`Alt-o` move; `Shift-o`/`Shift-Alt-o` extend the selection (same motion, extend flag set).
    assert_eq!(
        press(Mods::NONE)["motion"]["kind"],
        json!("next_navigation_unit")
    );
    assert_eq!(press(Mods::NONE)["extend_selection"], json!(false));
    assert_eq!(press(Mods::SHIFT)["extend_selection"], json!(true));
    assert_eq!(
        press(Mods::ALT)["motion"]["kind"],
        json!("prev_navigation_unit")
    );
    assert_eq!(press(Mods::ALT)["extend_selection"], json!(false));
    assert_eq!(press(shift_alt)["extend_selection"], json!(true));
}

#[test]
fn nav_back_into_the_same_buffer_reveals_as_a_jump() {
    use aether_client::effect::RevealStyle;
    use aether_client::update::Event;

    // A back/forward jump that lands in the buffer we're already on is a move, not a switch:
    // it must reposition the cursor and reveal it (Jump scroll), not resubscribe — otherwise the
    // restored scroll predates the jump and the cursor lands off-screen.
    let mut s = session();
    s.buffer.buffer_id = 7;
    let same_buffer_open = json!({
        "buffer_id": 7,
        "language": null,
        "line_count": 200,
        "byte_count": 4000,
        "revision": 1,
        "saved_revision": 1,
        "path": "/p/foo.rs",
        "cursor": { "position": {"line": 150, "col": 3}, "anchor": {"line": 150, "col": 3} },
    });
    let fx = s.on_event(Event::NavDone {
        forward: false,
        result: Ok(serde_json::from_value(json!({ "target": same_buffer_open })).unwrap()),
    });
    assert_eq!(s.buffer.cursor.position.line, 150);
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
    // A same-buffer move keeps the viewport binding rather than resubscribing.
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a same-buffer nav jump must not resubscribe"
    );

    // A jump into a DIFFERENT buffer still resubscribes (full switch).
    let mut s = session();
    s.buffer.buffer_id = 7;
    let other_open = json!({
        "buffer_id": 9,
        "language": null,
        "line_count": 10,
        "byte_count": 100,
        "revision": 1,
        "saved_revision": 1,
        "path": "/p/bar.rs",
        "cursor": { "position": {"line": 2, "col": 0}, "anchor": {"line": 2, "col": 0} },
    });
    let fx = s.on_event(Event::NavDone {
        forward: false,
        result: Ok(serde_json::from_value(json!({ "target": other_open })).unwrap()),
    });
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a cross-buffer nav jump resubscribes"
    );
}

#[test]
fn goto_definition_lands_the_identifier_selected() {
    use aether_client::update::Event;
    use aether_protocol::lsp::LspGotoDefinitionResult;
    let mut s = session();
    s.project_paths = vec!["/p".into()];

    // A definition with a real identifier span opens the buffer as a selection: cursor on the
    // span's last char, anchor at its start — like the outline / references pickers.
    let with_span: LspGotoDefinitionResult = serde_json::from_value(json!({
        "location": {
            "path": "/p/src/lib.rs",
            "position": { "line": 10, "col": 4 },
            "end": { "line": 10, "col": 9 },
        },
        "readiness": "ready",
    }))
    .unwrap();
    let fx = s.on_event(Event::Definition(Ok(with_span)));
    let params = find_request(&fx, "buffer/open").expect("goto-def opens the target buffer");
    assert_eq!(
        params["jump_to"],
        json!({ "line": 10, "col": 9 }),
        "cursor on the identifier's last char"
    );
    assert_eq!(
        params["jump_to_anchor"],
        json!({ "line": 10, "col": 4 }),
        "anchor at the identifier's start"
    );

    // No distinct span (end == position): a point cursor, no anchor.
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let point: LspGotoDefinitionResult = serde_json::from_value(json!({
        "location": {
            "path": "/p/src/lib.rs",
            "position": { "line": 3, "col": 0 },
            "end": { "line": 3, "col": 0 },
        },
        "readiness": "ready",
    }))
    .unwrap();
    let fx = s.on_event(Event::Definition(Ok(point)));
    let params = find_request(&fx, "buffer/open").expect("goto-def opens the target buffer");
    assert_eq!(params["jump_to"], json!({ "line": 3, "col": 0 }));
    assert!(
        params["jump_to_anchor"].is_null(),
        "a zero-width span lands a point, not a selection"
    );
}

#[test]
fn goto_definition_into_the_same_buffer_glides_not_resubscribes() {
    use aether_client::effect::RevealStyle;
    use aether_client::update::Event;

    // Goto-definition / picker opens funnel through `Event::Switched`. Landing in the buffer we're
    // already on must glide to the target (Jump reveal) like a grep hit or nav step — not tear down
    // and rebuild the whole window. This is the generalisation: one `adopt_navigation` path.
    let mut s = session();
    s.buffer.buffer_id = 4;
    let same = json!({
        "buffer_id": 4,
        "language": null,
        "line_count": 300,
        "byte_count": 6000,
        "revision": 2,
        "saved_revision": 2,
        "path": "/p/foo.rs",
        "cursor": { "position": {"line": 250, "col": 8}, "anchor": {"line": 250, "col": 8} },
    });
    let fx = s.on_event(Event::Switched(Ok(serde_json::from_value(same).unwrap())));
    assert_eq!(s.buffer.cursor.position.line, 250);
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a same-buffer goto-def must not resubscribe"
    );

    // A definition in another file is still a full switch.
    let mut s = session();
    s.buffer.buffer_id = 4;
    let other = json!({
        "buffer_id": 8,
        "language": null,
        "line_count": 10,
        "byte_count": 100,
        "revision": 1,
        "saved_revision": 1,
        "path": "/p/bar.rs",
        "cursor": { "position": {"line": 1, "col": 0}, "anchor": {"line": 1, "col": 0} },
    });
    let fx = s.on_event(Event::Switched(Ok(serde_json::from_value(other).unwrap())));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a cross-buffer goto-def resubscribes"
    );
}

#[test]
fn save_as_prompt_is_value_synced_not_keycode_edited() {
    use aether_client::chips::ChipEditorField;
    use aether_client::save_as::SaveAsEditor;
    use aether_client::session::Prompt;
    let mut s = session();
    // The save-as prompt's text is owned by each shell's input; the core only stores the value
    // and handles command keys. A typed char reaching the core must NOT edit the value.
    s.prompt = Some(Prompt::SaveAs(Box::new(SaveAsEditor::new(
        "notes".into(),
        ChipEditorField::Path,
        0,
    ))));
    let _ = key(&mut s, 'x');
    match &s.prompt {
        Some(Prompt::SaveAs(ed)) => {
            assert_eq!(
                ed.input.text, "notes",
                "the core must not key-edit the save-as value"
            );
        }
        other => panic!("expected the save-as prompt to stay open, got {other:?}"),
    }
    // The shell's value-sync entry point is what changes the text.
    s.save_as_set_input("notes.md".into());
    match &s.prompt {
        Some(Prompt::SaveAs(ed)) => assert_eq!(ed.input.text, "notes.md"),
        other => panic!("expected the save-as prompt, got {other:?}"),
    }
    // Esc is a command the core owns: it closes the prompt.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.prompt.is_none(), "Esc closes the save-as prompt");
}

#[test]
fn save_as_completes_dir_and_files_then_saves_the_literal_path() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    // `Space Alt-s` opens the save-as prompt and fires a directory/list for the root (empty path).
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('s'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "directory/list").expect("open fires a directory/list");
    assert_eq!(params["path"], json!("/p"));

    // The listing lands with a directory and a file — unlike the dir-scope chip, files are kept.
    let _ = s.on_event(Event::SaveAsListing {
        abs: "/p".into(),
        result: Ok(DirectoryListResult {
            path: "/p".into(),
            parent: None,
            entries: vec![
                DirectoryEntry {
                    name: "src".into(),
                    is_dir: true,
                },
                DirectoryEntry {
                    name: "main.rs".into(),
                    is_dir: false,
                },
            ],
        }),
    });

    // A directory ghost ends in `/`; a file ghost does not.
    let _ = s.save_as_set_input("s".into());
    let ghost = match &s.prompt {
        Some(Prompt::SaveAs(ed)) => ed.path_ghost(),
        other => panic!("expected save-as, got {other:?}"),
    };
    assert_eq!(
        ghost.as_deref(),
        Some("rc/"),
        "directory ghost keeps the slash"
    );
    let _ = s.save_as_set_input("m".into());
    let ghost = match &s.prompt {
        Some(Prompt::SaveAs(ed)) => ed.path_ghost(),
        _ => unreachable!(),
    };
    assert_eq!(ghost.as_deref(), Some("ain.rs"), "file ghost has no slash");

    // Enter saves the *literal* typed path (not the highlighted suggestion).
    let _ = s.save_as_set_input("notes.md".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let params = find_request(&fx, "buffer/save").expect("Enter saves");
    assert_eq!(params["relative_path"], json!("notes.md"));
    assert_eq!(params["path_index"], json!(0));
    assert!(s.prompt.is_none(), "the prompt closes on submit");
}

/// Saving-as onto an existing file: the first request carries `overwrite: false`; the server's
/// `WOULD_OVERWRITE` refusal raises a confirm, and accepting retries with the flag set.
#[test]
fn save_as_overwrite_confirms_then_retries_with_the_flag_set() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_client::update::Event;
    use aether_protocol::error::ErrorCode;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, None, ROWS);
    let _ = s.save_as_set_input("existing.md".into());

    // Enter saves with the confirm flag unset.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let params = find_request(&fx, "buffer/save").expect("Enter saves");
    assert_eq!(params["overwrite"], json!(false));
    let token = match fx.0.iter().find_map(|e| match e {
        Effect::Request { token, method, .. } if *method == "buffer/save" => Some(*token),
        _ => None,
    }) {
        Some(t) => t,
        None => unreachable!(),
    };
    assert!(s.prompt.is_none(), "the save-as prompt closes on submit");

    // The server refuses: the file already exists. The client raises an overwrite confirmation.
    let _ = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "buffer/save",
            code: ErrorCode::WOULD_OVERWRITE.code(),
            message: "exists".into(),
        }),
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::Overwrite { path },
            ..
        }) => assert_eq!(path.as_deref(), Some("existing.md")),
        other => panic!("expected an overwrite confirm, got {other:?}"),
    }

    // Accepting retries the save with `overwrite: true`.
    let fx = s.on_event(Event::PromptAccept);
    let params = find_request(&fx, "buffer/save").expect("the confirmed save retries");
    assert_eq!(params["overwrite"], json!(true));
    assert_eq!(params["relative_path"], json!("existing.md"));
}

/// Declining the overwrite confirm re-opens the save-as prompt pre-filled, so a tweak and re-save
/// is one gesture (and re-fetches the directory listing for the ghost).
#[test]
fn declining_save_as_overwrite_reopens_the_prompt_prefilled() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;
    use aether_protocol::error::ErrorCode;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, None, ROWS);
    let _ = s.save_as_set_input("existing.md".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let token = match fx.0.iter().find_map(|e| match e {
        Effect::Request { token, method, .. } if *method == "buffer/save" => Some(*token),
        _ => None,
    }) {
        Some(t) => t,
        None => unreachable!(),
    };
    let _ = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "buffer/save",
            code: ErrorCode::WOULD_OVERWRITE.code(),
            message: "exists".into(),
        }),
    );
    // Decline → the prompt returns pre-filled, and re-issues the directory/list for the ghost.
    let fx = s.on_event(Event::PromptCancel);
    assert!(
        find_request(&fx, "directory/list").is_some(),
        "reopening re-fetches the listing"
    );
    match &s.prompt {
        Some(Prompt::SaveAs(ed)) => assert_eq!(ed.input.text, "existing.md"),
        other => panic!("expected the save-as prompt to reopen, got {other:?}"),
    }
}

/// On a `[y/N]` confirm, only `y`/`Y` accepts; Enter (and anything else) declines — honouring the
/// capital `N`, so Enter never runs the destructive action.
#[test]
fn confirm_enter_declines_and_only_y_accepts() {
    use aether_client::session::{ConfirmAction, ConfirmKind, Prompt};
    let stage = |s: &mut Session| {
        s.prompt = Some(Prompt::Confirm {
            kind: ConfirmKind::DiscardOnReload,
            action: ConfirmAction::ReloadDiscard,
        });
    };

    // Enter dismisses the confirm without running the action.
    let mut s = session();
    stage(&mut s);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(s.prompt.is_none(), "Enter dismisses the confirm");
    assert!(
        find_request(&fx, "buffer/reload").is_none(),
        "Enter must not run the destructive action"
    );

    // `y` accepts → the action runs (reload forced).
    stage(&mut s);
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    assert!(s.prompt.is_none());
    let params = find_request(&fx, "buffer/reload").expect("`y` runs the confirmed action");
    assert_eq!(params["force"], json!(true));

    // `Y` (shifted) accepts too.
    stage(&mut s);
    let fx = s.on_key(KeyCode::Char('Y'), Mods::NONE, Some("Y".into()), ROWS);
    assert!(
        find_request(&fx, "buffer/reload").is_some(),
        "`Y` also accepts"
    );
}

/// A `buffer/state` push carrying a *new* path (a save-as on the shared buffer from another
/// client) is adopted: this client follows the rename, re-deriving its project-relative label. An
/// unchanged path (in-place save / reload) leaves the label alone.
#[test]
fn buffer_state_push_follows_a_save_as_rename() {
    use aether_client::update::Event;
    use aether_protocol::buffer::{BufferState, BufferStateParams};
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.buffer_id = 10;
    s.buffer.path = Some("/p/foo.md".into());
    s.buffer.label = "foo.md".into();

    let push = |path: Option<&str>| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: BufferState::NAME.into(),
            params: serde_json::to_value(BufferStateParams {
                buffer_id: 10,
                saved_revision: 3,
                saved_at_unix_ms: Some(1),
                externally_modified: false,
                externally_deleted: false,
                transient: false,
                path: path.map(Into::into),
            })
            .unwrap(),
        })
    };

    // Another client saved-as foo.md -> sub/bar.md: we follow, relabelling to the new rel path.
    let _ = s.on_event(push(Some("/p/sub/bar.md")));
    assert_eq!(s.buffer.path.as_deref(), Some("/p/sub/bar.md"));
    assert_eq!(s.buffer.label, "sub/bar.md");

    // An in-place save (same path) is a no-op for the label; a legacy push (no path) too.
    let _ = s.on_event(push(Some("/p/sub/bar.md")));
    assert_eq!(s.buffer.label, "sub/bar.md");
    let _ = s.on_event(push(None));
    assert_eq!(s.buffer.path.as_deref(), Some("/p/sub/bar.md"));
    assert_eq!(s.buffer.label, "sub/bar.md");
}

#[test]
fn project_renamed_push_adopts_the_new_name() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::project::{ProjectRenamed, ProjectRenamedParams};
    let push = |old: &str, new: &str| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: ProjectRenamed::NAME.into(),
            params: serde_json::to_value(ProjectRenamedParams {
                old_name: old.into(),
                new_name: new.into(),
            })
            .unwrap(),
        })
    };
    let mut s = session();
    s.project = "aether".into();
    // A rename of our active project is adopted locally (drives display + reconnect baseline).
    let _ = s.on_event(push("aether", "aether-next"));
    assert_eq!(s.project, "aether-next");
    // A push that doesn't match our project (stale / not ours) is ignored.
    let _ = s.on_event(push("something-else", "whatever"));
    assert_eq!(s.project, "aether-next");
}

#[test]
fn streaming_grep_view_snapshot_does_not_wipe_pushed_rows() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams, PickerViewResult};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Grep, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.generation = 5;
        p.offset = 0;
        p.items.clear();
    }
    let hit = |line: u32| PickerItem::GrepHit {
        path_index: 0,
        relative_path: "a.rs".into(),
        line,
        col: 0,
        preview: "x".into(),
        match_indices: vec![],
    };
    let update = |items: Option<Vec<PickerItem>>, matches: u32| PickerUpdateParams {
        kind: PickerKind::Grep,
        generation: 5,
        offset: 0,
        items,
        total_matches: matches,
        total_candidates: matches,
        ticking: true,
        grep_display_offset: Some(0),
        grep_total_display_rows: Some(matches + 1),
        center_on: None,
        explorer_peek_missing: false,
    };
    // A streaming `picker/update` push lands first with real hits.
    assert!(s
        .picker
        .as_mut()
        .unwrap()
        .apply_update(update(Some(vec![hit(1), hit(2)]), 2)));
    assert_eq!(s.picker.as_ref().unwrap().items.len(), 2);
    // The `picker/view` response carries a stale, empty snapshot (taken before the hits landed).
    // It must not wipe the rows the push already delivered.
    let view = PickerViewResult {
        query: "foo".into(),
        generation: 5,
        total_candidates: 2,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: Default::default(),
        update: Some(update(Some(vec![]), 0)),
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: false,
        result: Ok(view),
    });
    assert_eq!(
        s.picker.as_ref().unwrap().items.len(),
        2,
        "an empty view snapshot must not wipe rows a push already delivered"
    );
}

#[test]
fn grep_count_only_ticks_keep_the_window_then_the_first_batch_replaces_it() {
    // The grep streaming sequence at the core: the previous query's hits stay put through the
    // initial count-only tick (`items: None`) and the throttled count ticks while the new search
    // runs, then the first real batch replaces them — so the list never blanks mid-type.
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Grep, None, None, false);
    let hit = |path: &str, line: u32| PickerItem::GrepHit {
        path_index: 0,
        relative_path: path.into(),
        line,
        col: 0,
        preview: "x".into(),
        match_indices: vec![],
    };
    let gen = s.picker.as_ref().unwrap().generation;
    let tick = |items: Option<Vec<PickerItem>>, matches: u32| PickerUpdateParams {
        kind: PickerKind::Grep,
        generation: gen,
        offset: 0,
        items,
        total_matches: matches,
        total_candidates: matches,
        ticking: true,
        grep_display_offset: Some(0),
        grep_total_display_rows: Some(matches),
        center_on: None,
        explorer_peek_missing: false,
    };
    // The previous query's window.
    assert!(s
        .picker
        .as_mut()
        .unwrap()
        .apply_update(tick(Some(vec![hit("old.rs", 1), hit("old.rs", 2)]), 2)));

    // New query's initial count-only tick (items: None, count reset to 0): keep the window AND its
    // geometry. Zeroing total_matches/total_display_rows here would collapse the shells' viewport
    // (iced list height, web spacer, TUI scrollbar) and flash the kept rows away for a frame.
    assert!(s.picker.as_mut().unwrap().apply_update(tick(None, 0)));
    {
        let p = s.picker.as_ref().unwrap();
        assert_eq!(
            p.items.len(),
            2,
            "the count-only tick keeps the previous window rather than blanking it"
        );
        assert_eq!(
            p.total_matches, 2,
            "the prior count is kept, not reset to 0"
        );
        assert_eq!(
            p.total_display_rows, 2,
            "the prior display geometry is kept so the viewport doesn't collapse"
        );
    }
    // A throttled count tick as hits stream in elsewhere (count climbs, still None): still kept.
    assert!(s.picker.as_mut().unwrap().apply_update(tick(None, 7)));
    assert_eq!(s.picker.as_ref().unwrap().items.len(), 2);
    assert_eq!(s.picker.as_ref().unwrap().total_matches, 7);

    // The first batch that touches the window replaces the stale rows.
    assert!(s
        .picker
        .as_mut()
        .unwrap()
        .apply_update(tick(Some(vec![hit("new.rs", 9)]), 7)));
    let items = &s.picker.as_ref().unwrap().items;
    assert_eq!(items.len(), 1);
    assert!(
        matches!(&items[0], PickerItem::GrepHit { relative_path, .. } if relative_path == "new.rs")
    );
}

#[test]
fn picker_query_change_keeps_stale_window_until_the_new_push_lands() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    let file = |name: &str| PickerItem::File {
        path_index: 0,
        relative_path: name.into(),
        match_indices: vec![],
        git_status: None,
    };
    let gen0 = s.picker.as_ref().unwrap().generation;
    let window = |generation: u64, items: Vec<PickerItem>, total: u32| PickerUpdateParams {
        kind: PickerKind::Files,
        generation,
        offset: 0,
        items: Some(items),
        total_matches: total,
        total_candidates: 3,
        ticking: false,
        grep_display_offset: None,
        grep_total_display_rows: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    // Seed a window of results, as the server's push would.
    assert!(s.picker.as_mut().unwrap().apply_update(window(
        gen0,
        vec![file("a.rs"), file("b.rs")],
        2
    )));

    // Typing must NOT clear the window — the stale rows stay on screen (no empty flash) until the
    // fresh push replaces them. A new query is in flight (ticking) and re-filters via picker/query.
    let fx = s.picker_set_query("a".into());
    let p = s.picker.as_ref().unwrap();
    assert_eq!(
        p.items.len(),
        2,
        "the previous query's window is kept until the new one arrives"
    );
    assert!(p.ticking, "the picker shows it is searching");
    assert_eq!(p.offset, 0);
    let gen1 = p.generation;
    assert!(
        gen1 > gen0,
        "the generation bumped to invalidate stale pushes"
    );
    assert!(find_request(&fx, "picker/query").is_some());

    // The fresh push (new generation, offset 0) replaces the window atomically.
    assert!(s
        .picker
        .as_mut()
        .unwrap()
        .apply_update(window(gen1, vec![file("a.rs")], 1)));
    assert_eq!(s.picker.as_ref().unwrap().items.len(), 1);
}

#[test]
fn chip_editor_is_value_synced_not_keycode_edited() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Grep, None, None, false);
    // Alt-g opens the glob filter editor (a chip-editor line).
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    let glob_open = |s: &Session| -> String {
        s.picker
            .as_ref()
            .unwrap()
            .chip_editor
            .as_ref()
            .expect("glob editor open")
            .input
            .text
            .clone()
    };
    assert_eq!(glob_open(&s), "");
    // A typed char reaching the core must NOT edit the value — that's the shell input's job.
    let _ = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()), ROWS);
    assert_eq!(
        glob_open(&s),
        "",
        "the core must not key-edit the chip editor"
    );
    // The shell's value-sync entry point drives it.
    let _ = s.chip_editor_set_input("*.rs".into());
    assert_eq!(glob_open(&s), "*.rs");
    // Esc is a command the core owns: it closes the editor.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.picker.as_ref().unwrap().chip_editor.is_none());
}

#[test]
fn picker_query_is_value_synced_and_chip_row_gestures_work() {
    use aether_client::chips::ChipValue;
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Grep, None, None, false);
    // The shell's input owns query typing and syncs the value; the core re-filters on it.
    let fx = s.picker_set_query("foo".into());
    assert_eq!(s.picker.as_ref().unwrap().query, "foo");
    assert!(
        find_request(&fx, "picker/query").is_some(),
        "a query change re-filters via picker/query"
    );
    // Add a filter chip (Alt-w → whole-word), then drive the chip-row gesture the shell forwards
    // only from the query start: Left selects the rightmost chip.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None, ROWS);
    assert!(s
        .picker
        .as_ref()
        .unwrap()
        .chips
        .iter()
        .any(|c| matches!(c, ChipValue::Word)));
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().chip_selected, Some(0));
    // Typing while a chip is selected deselects it and lands the char in the query (append).
    let _ = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()), ROWS);
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.chip_selected, None, "typing deselects the chip");
    assert_eq!(p.query, "foox", "the typed char lands in the query");
}

#[test]
fn lsp_picker_centers_on_the_current_buffers_server() {
    use aether_protocol::lsp::LspServerRef;
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.lsp_server = Some(LspServerRef {
        language: "rust".into(),
        workspace_root: "/p".into(),
    });
    let fx = s.open_picker(PickerKind::LspServers, None, None, false);
    let params = find_request(&fx, "picker/view").expect("LSP picker opens via picker/view");
    // The view is anchored on the active buffer's own server (matched by language + workspace).
    assert_eq!(params["center_on"]["kind"], "lsp_server");
    assert_eq!(params["center_on"]["language"], "rust");
    assert_eq!(params["center_on"]["workspace_root"], "/p");
}

#[test]
fn closing_the_lsp_dialog_returns_to_the_picker() {
    use aether_client::session::Prompt;
    use aether_protocol::lsp::LspStatus;
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::LspServers, None, None, false);
    {
        let p = s.picker.as_mut().expect("picker open");
        p.items = vec![PickerItem::LspServer {
            name: "rust-analyzer".into(),
            language: "rust".into(),
            workspace_root: "/p".into(),
            root_label: String::new(),
            status: LspStatus::Ready,
            progress: vec![],
            match_indices: vec![],
        }];
        p.selected = 0;
    }
    // Enter drills into the detail dialog, but the picker stays open underneath.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(matches!(s.prompt, Some(Prompt::LspInfo(_))), "dialog opens");
    assert!(
        s.picker.is_some(),
        "the LSP picker stays open underneath the dialog"
    );
    // Closing the dialog (Esc) returns to the picker rather than the editor.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.prompt.is_none(), "dialog closed");
    assert!(s.picker.is_some(), "back at the LSP picker, not the editor");
}

#[test]
fn lsp_dialog_working_field_tracks_live_picker_progress() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::lsp::{LspProgress, LspStatus};
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdate, PickerUpdateParams};

    let server = |pct: u32| PickerItem::LspServer {
        name: "rust-analyzer".into(),
        language: "rust".into(),
        workspace_root: "/p".into(),
        root_label: String::new(),
        status: LspStatus::Ready,
        progress: vec![LspProgress {
            title: "Indexing".into(),
            message: None,
            percentage: Some(pct),
        }],
        match_indices: vec![],
    };

    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::LspServers, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![server(0)];
        p.selected = 0;
    }
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);

    // The LSP picker refreshes with new progress (a `report` — no `lsp/status_changed`); the open
    // dialog's Working line must follow it, not freeze at the opening 0% snapshot.
    let generation = s.picker.as_ref().unwrap().generation;
    let update = PickerUpdateParams {
        kind: PickerKind::LspServers,
        generation,
        offset: 0,
        items: Some(vec![server(50)]),
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        grep_display_offset: None,
        grep_total_display_rows: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let _ = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(&update).unwrap(),
    }));
    match &s.prompt {
        Some(Prompt::LspInfo(info)) => assert_eq!(
            info.progress.first().and_then(|p| p.percentage),
            Some(50),
            "the dialog's Working % tracks the live picker progress"
        ),
        other => panic!("expected the LSP dialog still open, got {other:?}"),
    }
}

#[test]
fn lsp_info_restart_is_ctrl_r_not_plain_r() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;
    use aether_protocol::lsp::{LspServerStatus, LspStatus};
    let status = || {
        Box::new(LspServerStatus {
            name: "rust-analyzer".into(),
            language: "rust".into(),
            workspace_root: "/p".into(),
            status: LspStatus::Ready,
            progress: vec![],
        })
    };

    // Plain `r` just closes the dialog — it must NOT restart (that was the old binding).
    let mut s = session();
    s.prompt = Some(Prompt::LspInfo(status()));
    let fx = s.on_key(KeyCode::Char('r'), Mods::NONE, Some("r".into()), ROWS);
    assert!(s.prompt.is_none(), "any non-Ctrl key closes the dialog");
    assert!(
        find_request(&fx, "lsp/restart_server").is_none(),
        "plain r no longer restarts"
    );

    // Ctrl-r restarts the server AND keeps the dialog open, showing Restarting immediately.
    s.prompt = Some(Prompt::LspInfo(status()));
    let fx = s.on_key(KeyCode::Char('r'), Mods::CTRL, None, ROWS);
    assert!(
        find_request(&fx, "lsp/restart_server").is_some(),
        "Ctrl-r restarts"
    );
    match &s.prompt {
        Some(Prompt::LspInfo(info)) => {
            assert!(
                matches!(info.status, LspStatus::Restarting),
                "the dialog stays open and shows Restarting"
            );
        }
        other => panic!("expected the LSP dialog to stay open, got {other:?}"),
    }

    // A subsequent `lsp/status_changed` for that server live-updates the open dialog (→ Ready).
    let ready = LspServerStatus {
        name: "rust-analyzer".into(),
        language: "rust".into(),
        workspace_root: "/p".into(),
        status: LspStatus::Ready,
        progress: vec![],
    };
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::lsp::LspStatusChanged;
    let _ = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: LspStatusChanged::NAME.into(),
        params: serde_json::to_value(&ready).unwrap(),
    }));
    match &s.prompt {
        Some(Prompt::LspInfo(info)) => {
            assert!(
                matches!(info.status, LspStatus::Ready),
                "dialog reflects the live status"
            );
        }
        other => panic!("expected the LSP dialog still open, got {other:?}"),
    }
}

#[test]
fn editing_is_refused_while_disconnected_and_insert_drops_on_disconnect() {
    use aether_client::session::{ConnState, Mode};
    use aether_client::update::Event;

    // Boot-connecting (or any non-Connected state): pressing `i` must NOT enter Insert — a live
    // insert cursor that silently drops keystrokes reads as a hang. It stays Normal with a hint.
    let mut s = session();
    s.conn = ConnState::Connecting;
    let fx = key(&mut s, 'i');
    assert_eq!(s.mode, Mode::Normal, "insert is refused while connecting");
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast(_, ToastKind::Info))),
        "a hint explains why nothing happened"
    );
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "no RPC is attempted while disconnected"
    );

    // A mid-session disconnect drops out of Insert so the cursor doesn't sit in a dead insert mode.
    let mut s = session();
    let _ = key(&mut s, 'i'); // connected → enters Insert
    assert_eq!(s.mode, Mode::Insert);
    let _ = s.on_event(Event::ConnectionLost);
    assert_eq!(
        s.mode,
        Mode::Normal,
        "losing the connection drops out of Insert"
    );
    assert!(matches!(s.conn, ConnState::Reconnecting { .. }));
}

#[test]
fn glob_editor_live_previews_results_and_reverts_on_cancel() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    // Open the glob editor — no chip committed yet, so nothing narrows.
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    // Typing a glob folds the would-commit value into the live filters → a re-query carrying it,
    // even though no chip has been committed.
    let fx = s.chip_editor_set_input("*.rs".into());
    let params = find_request(&fx, "picker/query").expect("the glob preview re-queries");
    assert_eq!(params["filters"]["globs"], json!(["*.rs"]));
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "the preview is in-flight only — nothing committed"
    );
    // Cancelling reverts the results to the committed (empty) set — the glob drops off the wire
    // (an empty `globs` is omitted by `skip_serializing_if`).
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    let params = find_request(&fx, "picker/query").expect("cancel reverts the preview");
    assert_eq!(params["filters"]["globs"], json!(null));
    assert!(s.picker.as_ref().unwrap().chip_editor.is_none());
}

#[test]
fn degenerate_glob_preview_does_not_requery() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    // "*" normalizes away (match-everything) → the effective set is unchanged → no wasted
    // re-query (and no blank-and-refetch flash).
    let fx = s.chip_editor_set_input("*".into());
    assert!(
        find_request(&fx, "picker/query").is_none(),
        "an effective-no-op edit must not re-query"
    );
}

#[test]
fn dir_editor_holds_while_listing_pending_then_previews_on_load() {
    use aether_client::update::Event;
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    // Alt-p opens the path-scope editor and fires a directory/list for the root.
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None, ROWS);
    // Type a leaf before the listing lands: the path's validity is unknown, so results are
    // held — no re-query flapping them wider for a frame.
    let fx = s.chip_editor_set_input("sr".into());
    assert!(
        find_request(&fx, "picker/query").is_none(),
        "a non-empty path with a pending listing holds the results"
    );
    // The listing resolves; "sr" prefixes "src" → the would-commit scope applies live.
    let fx = s.on_event(Event::PickerChipListing {
        abs: "/p".into(),
        result: Ok(DirectoryListResult {
            path: "/p".into(),
            parent: None,
            entries: vec![
                DirectoryEntry {
                    name: "src".into(),
                    is_dir: true,
                },
                DirectoryEntry {
                    name: "docs".into(),
                    is_dir: true,
                },
            ],
        }),
    });
    let params =
        find_request(&fx, "picker/query").expect("the scope applies once the listing loads");
    assert_eq!(
        params["filters"]["directories"],
        json!([{"path_index": 0, "relative_path": "src"}])
    );
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "still a preview — the dir chip commits on Enter"
    );
}

#[test]
fn invalid_dir_path_preview_contributes_nothing() {
    use aether_client::update::Event;
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None, ROWS);
    let _ = s.chip_editor_set_input("zzz".into());
    // The listing lands with no directory the leaf prefixes → the path is invalid → the preview
    // contributes nothing (results show as if the half-typed chip weren't there).
    let fx = s.on_event(Event::PickerChipListing {
        abs: "/p".into(),
        result: Ok(DirectoryListResult {
            path: "/p".into(),
            parent: None,
            entries: vec![DirectoryEntry {
                name: "src".into(),
                is_dir: true,
            }],
        }),
    });
    // Effective set equals the committed (empty) set, which is already running → no re-query.
    assert!(
        find_request(&fx, "picker/query").is_none(),
        "an invalid path leaves the effective filters unchanged"
    );
}

#[test]
fn space_alt_c_opens_the_buffer_locked_changes_picker() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.path = Some("/p/src/main.rs".into());
    // `Space Alt-c`: the modal file-changes picker — its own kind, locked to the active buffer via
    // `buffer_id` (intrinsic, like Diagnostics), not a filter chip.
    let fx = s.open_picker(PickerKind::GitChangesFile, None, None, false);
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("git_changes_file"));
    assert_eq!(
        params["buffer_id"],
        json!(s.buffer.buffer_id),
        "locked to the active buffer"
    );
    assert!(
        params["filters"].is_null(),
        "no filter chips — the scope is intrinsic"
    );
}

#[test]
fn space_alt_f_seeds_a_removable_directory_chip() {
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.path = Some("/p/src/main.rs".into());
    // `Space Alt-f`: Files pre-scoped to the buffer's directory as an ordinary, composable dir chip.
    let fx = s.open_files_in_buffer_dir();
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("files"));
    assert_eq!(
        params["filters"]["directories"],
        json!([{"path_index": 0, "relative_path": "src"}]),
        "a normal dir chip (no scope override) for the buffer's directory"
    );
}

#[test]
fn space_alt_f_unscoped_for_scratch_buffer() {
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.path = None; // scratch buffer — no directory to scope to
    let fx = s.open_files_in_buffer_dir();
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert!(
        params["filters"].is_null(),
        "a scratch buffer opens the whole workspace"
    );
}

#[test]
fn space_alt_g_opens_grep_from_selection() {
    // `Space Alt-g`: open Grep asking the server to seed the query from the buffer's selection.
    // The client carries no selection text — it just sets `from_selection` + the buffer id and
    // lets the server slice + search (the query/generation ride back via the `PickerViewed` echo).
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    s.buffer.path = Some("/p/src/main.rs".into());
    let fx = s.open_grep_from_selection();
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("grep"));
    assert_eq!(params["from_selection"], json!(true));
    assert_eq!(
        params["buffer_id"],
        json!(s.buffer.buffer_id),
        "the active buffer rides along so the server can slice its selection"
    );
    assert!(
        params["filters"].is_null(),
        "no dir scope — grep-for-selection is workspace-wide, sticky filters aside"
    );
    // Not a cursor-centred resume: a fresh search has no cached hits to land on.
    assert!(params
        .get("center_on_cursor")
        .map(|v| v.is_null())
        .unwrap_or(true));
}

#[test]
fn search_query_is_value_synced_not_keycode_edited() {
    use aether_client::session::Mode;
    let mut s = session();
    let _ = key(&mut s, '/'); // enter search
    assert_eq!(s.mode, Mode::Search);
    // A typed char reaching the core must NOT edit the query — text is the shell's input's job.
    let _ = key(&mut s, 'a');
    assert_eq!(
        s.search.query, "",
        "the core must not key-edit the search query"
    );
    // The shell's value-sync entry point drives it and re-runs the incremental search.
    let _ = s.search_set_query("ab".into());
    assert_eq!(s.search.query, "ab");
    // Esc is a command the core owns: it aborts search.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(s.mode, Mode::Normal, "Esc aborts search");
}

#[test]
fn search_option_toggles_cycle_and_ride_the_request() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::CaseMode;
    let mut s = session();
    let _ = key(&mut s, '/'); // enter search
    let _ = s.search_set_query("foo".into());

    // Alt-e toggles regex; the new query goes back out with the options in the params.
    let fx = s.on_key(KeyCode::Char('e'), Mods::ALT, None, ROWS);
    assert!(s.search.options.regex, "Alt-e enables regex");
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params["options"], json!({"regex": true}));

    // Alt-w toggles whole-word; Alt-c cycles smart -> sensitive -> insensitive -> smart.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None, ROWS);
    assert!(s.search.options.whole_word);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS);
    assert_eq!(s.search.options.case, CaseMode::Sensitive);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS);
    assert_eq!(s.search.options.case, CaseMode::Insensitive);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS);
    assert_eq!(
        s.search.options.case,
        CaseMode::Smart,
        "third Alt-c returns to smart"
    );

    // Esc restores the pre-prompt options (a cancelled search reverts its toggles too).
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(
        s.search.options,
        aether_protocol::picker::MatchOptions::default()
    );
}

#[test]
fn search_chip_row_select_navigate_cycle_remove() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::CaseMode;
    let mut s = session();
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("foo".into());
    // Enable case (sensitive) and whole-word via the Alt-chords → two chips, none selected.
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS);
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None, ROWS);
    assert_eq!(s.search.option_chips().len(), 2);
    assert_eq!(s.search.chip_selected, None);

    // Left at the query start steps into the row, selecting the rightmost (word) chip; Left again
    // walks to the case chip; Right walks back.
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None, ROWS);
    assert_eq!(s.search.chip_selected, Some(1));
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None, ROWS);
    assert_eq!(s.search.chip_selected, Some(0));
    let _ = s.on_key(KeyCode::Right, Mods::NONE, None, ROWS);
    assert_eq!(s.search.chip_selected, Some(1));

    // Enter on the word chip toggles it off — the chip vanishes, selection clamps onto the case chip.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(!s.search.options.whole_word);
    assert_eq!(s.search.option_chips().len(), 1);
    assert_eq!(s.search.chip_selected, Some(0));

    // Enter on the case chip cycles it (sensitive → insensitive); it stays present and selected.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert_eq!(s.search.options.case, CaseMode::Insensitive);
    assert_eq!(s.search.chip_selected, Some(0));

    // Backspace removes the selected case chip; the row empties and selection clears.
    let _ = s.on_key(KeyCode::Backspace, Mods::NONE, None, ROWS);
    assert_eq!(s.search.options.case, CaseMode::Smart);
    assert!(s.search.option_chips().is_empty());
    assert_eq!(s.search.chip_selected, None);

    // Esc with no chip selected aborts search as usual.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(s.mode, aether_client::session::Mode::Normal);
}

#[test]
fn count_prefix_rides_the_request() {
    let mut s = session();
    let _ = key(&mut s, '3');
    // Ctrl-g = join lines; the count lives in the params, not a client loop.
    let fx = ctrl(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/join_lines");
    assert_eq!(params["count"], json!(3));
}

#[test]
fn undo_result_updates_revision_and_cursor() {
    let mut s = session();
    let fx = ctrl(&mut s, 'z');
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "edit/undo");
    assert!(params.get("count").is_none(), "count 1 stays off the wire");

    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "applied": true,
            "revision": 7,
            "cursor": {"position": {"line": 1, "col": 0}, "anchor": {"line": 1, "col": 0}},
        })),
    );
    assert_eq!(s.buffer.revision, 7);
    assert_eq!(s.buffer.cursor.position.line, 1);
}

#[test]
fn rpc_error_surfaces_as_an_error_toast() {
    let mut s = session();
    let fx = ctrl(&mut s, 'z');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "edit/undo",
            code: 0,
            message: "boom".into(),
        }),
    );
    assert!(has_error_toast(&fx));
}

#[test]
fn unknown_token_is_ignored() {
    let mut s = session();
    let fx = s.on_rpc_result(999, Ok(json!({})));
    assert!(fx.0.is_empty(), "nothing parked under that token");
}

#[test]
fn connection_loss_drops_in_flight_results() {
    let mut s = session();
    let fx = ctrl(&mut s, 'z');
    let (token, _, _) = the_request(&fx);

    let fx = s.on_event(aether_client::update::Event::ConnectionLost);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Reconnect { attempt: 0 })),
        "loss schedules the first reconnect dial"
    );

    // The old connection's result arrives late: silently dropped, no stray error toast.
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "edit/undo",
            code: 0,
            message: "connection closed".into(),
        }),
    );
    assert!(fx.0.is_empty());
}

#[test]
fn disconnected_drops_server_requests_but_allows_quit() {
    use aether_client::update::Event;

    // A motion that would hit the server (`j` → cursor/move) emits no request while the socket is
    // down — the gate now lives at the point of issue, not a blanket key block.
    let mut s = session();
    let _ = s.on_event(Event::ConnectionLost);
    let fx = key(&mut s, 'j');
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "server requests are dropped while disconnected"
    );

    // ...but client-only actions still run, so the user can always quit (`Space q` → Exit).
    let mut s = session();
    let _ = s.on_event(Event::ConnectionLost);
    let _ = key(&mut s, ' '); // leader
    let fx = key(&mut s, 'q');
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "quit works while disconnected"
    );
}

#[test]
fn requests_are_emitted_in_dispatch_order() {
    // Sequenced flows lean on the ordering contract (requests hit the wire in emission
    // order); pin that a multi-effect dispatch keeps its tokens ascending.
    let mut s = session();
    let fx = key(&mut s, 'i'); // one request
    let (t1, _, _) = the_request(&fx);
    s.mode = aether_client::session::Mode::Normal; // back out without a round-trip
    let fx = ctrl(&mut s, 'z');
    let (t2, _, _) = the_request(&fx);
    assert!(t2 > t1, "tokens are allocated in emission order");
}

#[test]
fn primed_switch_adopts_summary_from_the_response_not_a_push() {
    // A grep jump (`<`/`>` or Enter on a hit) primes the new buffer's search server-side. The
    // match count rides the switch response (`BufferOpenResult::search_summary`) rather than the
    // `search/state_changed` push, because that push races the switch on the client: arriving
    // before the switch, its `buffer_id` guard fails and it's dropped. Here NO push is delivered,
    // so the count must already be live purely from the response.
    use aether_client::update::Event;
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::search::SearchSummary;

    let mut s = session();
    let open = BufferOpenResult {
        buffer_id: 7,
        language: None,
        line_count: 20,
        byte_count: 100,
        revision: 0,
        saved_revision: 0,
        path: Some("/proj/b.rs".into()),
        scratch_number: None,
        cursor: Default::default(),
        scroll: None,
        lsp_server: None,
        transient: true,
        search_summary: Some(SearchSummary {
            buffer_id: 7,
            total: 4,
            truncated: false,
            current_index: 1,
        }),
    };
    let opts = aether_protocol::picker::MatchOptions {
        case: aether_protocol::picker::CaseMode::Sensitive,
        whole_word: true,
        regex: false,
    };
    let _ = s.on_event(Event::SwitchedPrimed(Ok(Some((
        "needle".into(),
        opts,
        open,
    )))));

    assert!(
        s.search.active,
        "the primed search is active after the switch"
    );
    assert_eq!(s.search.query, "needle");
    assert_eq!(
        s.search.options, opts,
        "the grep result's match options ride the primed switch"
    );
    let summary = s
        .search
        .summary
        .expect("the match count rode the switch response");
    assert_eq!(summary.total, 4);
    assert_eq!(summary.current_index, 1);
}

#[test]
fn picker_view_response_renders_items_without_the_push() {
    // Reopening the Grep picker resumes server-side state at a generation ahead of the freshly
    // created local picker (generation 0). The items ride the `picker/view` response
    // (`PickerViewResult::update`) so they render atomically with adopting that generation — the
    // separate `picker/update` push can arrive first, when the generation still differs and the
    // staleness guard drops it, leaving the restored query but no rows. Here NO push is delivered.
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams, PickerViewResult};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Grep, None, None, false);
    assert!(
        s.picker.is_some(),
        "open_picker creates the local picker state"
    );

    let update = PickerUpdateParams {
        kind: PickerKind::Grep,
        generation: 9,
        offset: 0,
        items: Some(vec![PickerItem::GrepHit {
            path_index: 0,
            relative_path: "a.rs".into(),
            line: 3,
            col: 1,
            preview: "let x = 1;".into(),
            match_indices: vec![],
        }]),
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        grep_display_offset: None,
        grep_total_display_rows: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let r = PickerViewResult {
        query: "x".into(),
        generation: 9, // server's resumed generation; the local picker is still at 0
        total_candidates: 1,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: Default::default(),
        update: Some(update),
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(r),
    });

    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.generation, 9, "adopts the resumed generation");
    assert_eq!(p.query, "x", "restores the resumed query");
    assert_eq!(
        p.items.len(),
        1,
        "items render from the response, not a racing push"
    );
}

#[test]
fn grep_open_does_not_reset_scroll_but_fresh_pickers_do() {
    // A fresh picker (Files) resets the list to the top on open. Grep preserves state and resumes
    // onto its saved selection — often deep in the results — where `effective_center_on` drives a
    // reveal; emitting a scroll reset there would snap the window back to the top, blanking the view.
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let fx = s.open_picker(PickerKind::Grep, None, None, false);
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::PickerScrollReset)),
        "grep (state-preserving) open must not reset the scroll — it resumes onto its selection"
    );

    let mut s = session();
    let fx = s.open_picker(PickerKind::Files, None, None, false);
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::PickerScrollReset)),
        "a fresh Files picker resets the scroll to the top on open"
    );
}

#[test]
fn pointer_press_then_drag_extends_from_the_press_anchor() {
    // The shell resolves screen cells to buffer positions and feeds them in; the core owns the
    // selection: the press records the drag anchor + granularity (the click streak), and the drag
    // extends from that anchor with the same granularity until release.
    use aether_protocol::cursor::Granularity;
    use aether_protocol::LogicalPosition;

    let mut s = session();
    let press = LogicalPosition { line: 3, col: 5 };
    let fx = s.pointer_press(press, Granularity::Word, false);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/set");
    assert_eq!(params["position"], json!({"line": 3, "col": 5}));
    assert_eq!(params["anchor"], json!({"line": 3, "col": 5}));
    assert_eq!(
        params["granularity"],
        json!("word"),
        "double-click selects by word"
    );

    // Drag to a new cell: position moves, anchor + granularity stay from the press.
    let fx = s.pointer_drag(LogicalPosition { line: 4, col: 0 });
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/set");
    assert_eq!(params["position"], json!({"line": 4, "col": 0}));
    assert_eq!(
        params["anchor"],
        json!({"line": 3, "col": 5}),
        "drag keeps the press anchor"
    );
    assert_eq!(
        params["granularity"],
        json!("word"),
        "drag keeps the press granularity"
    );

    // The cursor result lands and reveals.
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "position": {"line": 3, "col": 9},
            "anchor": {"line": 3, "col": 5},
        })),
    );
    assert_eq!(s.buffer.cursor.position.col, 9);
    assert!(fx.0.iter().any(|e| matches!(e, Effect::RevealCursor(_))));

    // Release ends the drag — a further drag is inert.
    s.pointer_release();
    let fx = s.pointer_drag(LogicalPosition { line: 9, col: 0 });
    assert!(fx.0.is_empty(), "no cursor/set after release");
}

#[test]
fn shift_pointer_press_extends_from_the_existing_anchor() {
    // A non-extend press collapses the selection to the click (anchor == position); an extend
    // (shift-click) press keeps the current anchor so the selection grows to the click instead.
    use aether_protocol::cursor::Granularity;
    use aether_protocol::LogicalPosition;

    let mut s = session();
    let fx = s.pointer_press(LogicalPosition { line: 5, col: 0 }, Granularity::Char, true);
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["position"], json!({"line": 5, "col": 0}));
    // The placeholder session's cursor anchor is the origin; extend keeps it.
    assert_eq!(
        params["anchor"],
        json!({"line": 0, "col": 0}),
        "shift-click keeps the prior anchor"
    );
}

#[test]
fn pointer_selection_in_insert_mode_drops_to_normal() {
    // A selection can't coexist with the insert-mode bar caret (the inclusive endpoint and the
    // between-chars caret render in different cells), so a pointer gesture that creates a
    // selection leaves Insert. A plain single click only repositions the caret and stays.
    use aether_client::session::Mode;
    use aether_protocol::cursor::Granularity;
    use aether_protocol::LogicalPosition;

    // Single click (Char, no extend) → point cursor, stays in Insert.
    let mut s = session();
    let _ = key(&mut s, 'i');
    assert_eq!(s.mode, Mode::Insert);
    let _ = s.pointer_press(
        LogicalPosition { line: 2, col: 3 },
        Granularity::Char,
        false,
    );
    assert_eq!(
        s.mode,
        Mode::Insert,
        "single click only repositions the caret"
    );

    // Double click (Word) → immediate selection, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(
        LogicalPosition { line: 2, col: 3 },
        Granularity::Word,
        false,
    );
    assert_eq!(s.mode, Mode::Normal, "double-click selects a word → Normal");

    // Shift-click (extend) → selection from the existing anchor, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(LogicalPosition { line: 2, col: 3 }, Granularity::Char, true);
    assert_eq!(
        s.mode,
        Mode::Normal,
        "shift-click extends a selection → Normal"
    );

    // Char drag past the press anchor → selection, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(
        LogicalPosition { line: 2, col: 3 },
        Granularity::Char,
        false,
    );
    assert_eq!(
        s.mode,
        Mode::Insert,
        "the press alone hasn't selected anything yet"
    );
    let _ = s.pointer_drag(LogicalPosition { line: 2, col: 7 });
    assert_eq!(s.mode, Mode::Normal, "dragging out a selection → Normal");
}

/// Find the first `Effect::Request` whose method matches (the multi-request flows — re-list,
/// create — emit more than one, so `the_request`'s exactly-one assertion doesn't fit).
fn find_request<'a>(fx: &'a Effects, method: &str) -> Option<&'a serde_json::Value> {
    fx.0.iter().find_map(|e| match e {
        Effect::Request {
            method: m, params, ..
        } if *m == method => Some(params),
        _ => None,
    })
}

/// The text handed to a `WriteClipboard` effect, if any.
fn written_clipboard(fx: &Effects) -> Option<String> {
    fx.0.iter().find_map(|e| match e {
        Effect::WriteClipboard(t) => Some(t.clone()),
        _ => None,
    })
}

#[test]
fn explorer_tab_applies_common_prefix_completion() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj".into());
        p.query = "aet".into();
        p.items = vec![
            PickerItem::DirEntry {
                name: "aether-server".into(),
                is_dir: true,
                match_indices: vec![],
                git_status: None,
            },
            PickerItem::DirEntry {
                name: "aether-tui".into(),
                is_dir: true,
                match_indices: vec![],
                git_status: None,
            },
        ];
        p.total_matches = 2;
        p.offset = 0;
    }
    // Tab extends the query by the shared remainder (`her-`), then re-queries.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "aether-");
    let requery = find_request(&fx, "picker/query").expect("tab re-queries");
    assert_eq!(requery["query"], json!("aether-"));
}

#[test]
fn explorer_alt_backspace_unwinds_breadcrumb_before_chips() {
    use aether_client::chips::ChipValue;
    use aether_client::keymap::Mods;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src/sub".into());
        p.directory_parent = Some("/proj/src".into());
        p.chips = vec![ChipValue::Hidden { hide: true }];
        p.query.clear();
    }
    // With a deeper directory *and* a chip, Alt-Backspace ascends the breadcrumb (closest to the
    // cursor) and leaves the chip — it has its own toggle binding.
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None, ROWS);
    let view = find_request(&fx, "picker/view").expect("ascends via picker/view");
    assert_eq!(view["directory_path"], json!("/proj/src"));
    assert_eq!(
        s.picker.as_ref().unwrap().chips.len(),
        1,
        "the chip survives — the breadcrumb unwinds first"
    );

    // At a (single) root top — no parent — the breadcrumb is exhausted, so the next press falls
    // through to popping the chip.
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj".into());
        p.directory_parent = None;
        p.query.clear();
    }
    let _ = s.on_key(KeyCode::Backspace, Mods::ALT, None, ROWS);
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "with no breadcrumb left, Alt-Backspace removes the chip"
    );
}

#[test]
fn git_changes_opens_without_reset_to_resume_query_and_filters() {
    use aether_protocol::picker::PickerKind;
    // GitChanges preserves its query + filters server-side across opens (like Grep), so the client
    // opens it with `reset: false` — the server keeps the prior state and re-snapshots candidates.
    let mut s = session();
    let fx = s.open_picker(PickerKind::GitChanges, None, None, false);
    let view = find_request(&fx, "picker/view").expect("opens via picker/view");
    assert_eq!(view["kind"], json!("git_changes"));
    assert_eq!(
        view["reset"],
        json!(false),
        "GitChanges resumes its server-side query + filters"
    );
}

#[test]
fn explorer_delete_confirms_then_trashes_and_relists() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "old".into();
        p.items = vec![PickerItem::DirEntry {
            name: "old.rs".into(),
            is_dir: false,
            match_indices: vec![],
            git_status: None,
        }];
        p.selected = 0;
        p.offset = 0;
        p.total_matches = 1;
    }
    // Delete only stages a confirm — nothing is sent yet.
    let fx = s.picker_stage_delete();
    assert!(fx.0.is_empty(), "delete stages a confirm, sends nothing");
    match &s.prompt {
        Some(Prompt::Confirm { kind, .. }) => match kind {
            ConfirmKind::Delete { noun, name } => {
                assert_eq!(*noun, "file");
                assert_eq!(name, "old.rs");
            }
            other => panic!("expected a delete confirm, got {other:?}"),
        },
        other => panic!("expected a confirm prompt, got {other:?}"),
    }
    // `y` accepts → `path/delete` with the absolute path.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let path_del = find_request(&fx, "path/delete").expect("path/delete fired");
    assert_eq!(path_del["path"], json!("/proj/src/old.rs"));
    let token = match fx.0.iter().find_map(|e| match e {
        Effect::Request { token, method, .. } if *method == "path/delete" => Some(*token),
        _ => None,
    }) {
        Some(t) => t,
        None => unreachable!(),
    };
    // The result re-lists the still-open Explorer via `picker/query`, keeping the query (so the
    // user stays where they were filtering) — the re-query re-reads the dir server-side.
    let fx = s.on_rpc_result(token, Ok(json!({"closed_buffer_ids": []})));
    let requery = find_request(&fx, "picker/query").expect("a successful delete re-queries");
    assert_eq!(
        requery["query"],
        json!("old"),
        "the query is preserved across the delete"
    );
    assert_eq!(
        s.picker.as_ref().unwrap().query,
        "old",
        "the picker still holds the query"
    );
}

#[test]
fn projects_delete_confirms_then_deletes_and_guards_active() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    s.project = "current".into();
    let _ = s.open_picker(PickerKind::Projects, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![
            PickerItem::Project {
                name: "current".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            },
            PickerItem::Project {
                name: "other".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            },
        ];
        p.selected = 0; // the active project
        p.offset = 0;
        p.total_matches = 2;
    }
    // Ctrl-d on the *active* project refuses client-side — no confirm, no request.
    let fx = s.picker_stage_delete();
    assert!(s.prompt.is_none(), "active project can't be staged");
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast(_, ToastKind::Error))),
        "refusing the active project surfaces an error toast"
    );

    // Move to a non-active project: Ctrl-d stages a confirm, sends nothing yet.
    s.picker.as_mut().unwrap().selected = 1;
    let fx = s.picker_stage_delete();
    assert!(fx.0.is_empty(), "delete stages a confirm, sends nothing");
    match &s.prompt {
        Some(Prompt::Confirm { kind, .. }) => match kind {
            ConfirmKind::DeleteProject { name } => assert_eq!(name, "other"),
            other => panic!("expected a delete-project confirm, got {other:?}"),
        },
        other => panic!("expected a confirm prompt, got {other:?}"),
    }
    // `y` accepts → `project/delete { name }`.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let del = find_request(&fx, "project/delete").expect("project/delete fired");
    assert_eq!(del["name"], json!("other"));

    // A server "active in another window" refusal surfaces a clean, tailored toast — not the raw
    // `RpcError` Display (no "RPC … returned error -32005:" prefix).
    let token = fx
        .0
        .iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "project/delete" => Some(*token),
            _ => None,
        })
        .expect("project/delete token");
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "project/delete",
            code: aether_protocol::error::ErrorCode::ACTIVE_PROJECT_PREVENTS_DELETE.code(),
            message: "project other is active — switch to another project before deleting it"
                .into(),
        }),
    );
    let msg =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Toast(m, ToastKind::Error) => Some(m.clone()),
                _ => None,
            })
            .expect("an error toast");
    assert!(
        msg.contains("another window"),
        "tailored message, got {msg:?}"
    );
    assert!(!msg.contains("RPC"), "no raw RpcError prefix, got {msg:?}");
}

#[test]
fn buffers_picker_ctrl_d_closes_in_place() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{BufferDirtyState, PickerItem, PickerKind};

    fn buf(buffer_id: u64, display: &str, status: BufferDirtyState) -> PickerItem {
        PickerItem::Buffer {
            buffer_id,
            display: display.into(),
            status,
            path_index: None,
            relative_path: None,
            match_indices: vec![],
            transient: false,
        }
    }

    let mut s = session();
    // The active editor buffer is id 0 (placeholder default).
    let _ = s.open_picker(PickerKind::Buffers, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![
            buf(0, "active.rs", BufferDirtyState::Clean),
            buf(7, "background.rs", BufferDirtyState::Clean),
            buf(9, "dirty.rs", BufferDirtyState::Unsaved),
        ];
        p.offset = 0;
        p.total_matches = 3;
        p.selected = 1; // a clean background buffer
    }

    // Clean background buffer: closes immediately, no prompt, and *doesn't* switch the editor.
    let fx = s.picker_close_buffer();
    assert!(s.prompt.is_none(), "clean close needs no confirm");
    let close = find_request(&fx, "buffer/close").expect("buffer/close fired");
    assert_eq!(close["buffer_id"], json!(7));
    assert_eq!(
        close["open_next"],
        json!(false),
        "closing a background buffer leaves the editor put"
    );
    assert!(
        s.picker.is_some(),
        "the picker stays open — it re-lists from the server push"
    );

    // The active buffer: closing it must attach the successor (open_next), so the editor doesn't
    // sit on a closed buffer.
    s.picker.as_mut().unwrap().selected = 0;
    let fx = s.picker_close_buffer();
    assert!(s.prompt.is_none());
    let close = find_request(&fx, "buffer/close").expect("buffer/close fired");
    assert_eq!(close["buffer_id"], json!(0));
    assert_eq!(
        close["open_next"],
        json!(true),
        "closing the active buffer opens its MRU successor"
    );

    // A dirty buffer: Ctrl-d stages a discard confirm and sends nothing yet.
    s.picker.as_mut().unwrap().selected = 2;
    let fx = s.picker_close_buffer();
    assert!(
        fx.0.is_empty(),
        "dirty close stages a confirm, sends nothing"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::DiscardOnClose { label },
            ..
        }) => assert_eq!(label, "dirty.rs"),
        other => panic!("expected a discard-on-close confirm, got {other:?}"),
    }
    // `y` accepts → buffer/close { buffer_id: 9, open_next: false } (id 9 isn't the active buffer).
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let close = find_request(&fx, "buffer/close").expect("buffer/close fired on confirm");
    assert_eq!(close["buffer_id"], json!(9));
    assert_eq!(close["open_next"], json!(false));
}

#[test]
fn explorer_create_makes_a_file_with_create_if_missing() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    s.project_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "new.rs".into();
    }
    let fx = s.explorer_create_from_query();
    let open = find_request(&fx, "buffer/open").expect("buffer/open fired");
    assert_eq!(open["create_if_missing"], json!(true));
    assert_eq!(open["relative_path"], json!("src/new.rs"));
    assert_eq!(open["path_index"], json!(0));
}

#[test]
fn explorer_create_with_trailing_slash_makes_a_directory() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    s.project_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "sub/".into();
    }
    let fx = s.explorer_create_from_query();
    let mk = find_request(&fx, "directory/create").expect("directory/create fired");
    assert_eq!(mk["path"], json!("/proj/src/sub"));
    assert!(
        find_request(&fx, "buffer/open").is_none(),
        "a trailing slash creates a dir, not a file"
    );
}

/// Selecting the synthetic "+ Create …" row (the affordance that replaced the old Ctrl-n) runs the
/// create: a click on its absolute index routes through `picker_accept` → create-on-save.
#[test]
fn selecting_the_create_row_creates_the_file() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.project_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "new.rs".into();
        // One existing entry that the query doesn't match — the create row sits at index 1.
        p.apply_update(PickerUpdateParams {
            kind: PickerKind::Explorer,
            generation: p.generation,
            offset: 0,
            items: Some(vec![PickerItem::DirEntry {
                name: "lib.rs".into(),
                is_dir: false,
                match_indices: vec![],
                git_status: None,
            }]),
            total_matches: 1,
            total_candidates: 1,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        });
        assert_eq!(p.create_row_index(), Some(1));
    }
    // Click the create row (absolute index 1) → highlight it and accept.
    let fx = s.on_event(Event::PickerClicked(1));
    let open = find_request(&fx, "buffer/open").expect("buffer/open fired");
    assert_eq!(open["create_if_missing"], json!(true));
    assert_eq!(open["relative_path"], json!("src/new.rs"));
}

#[test]
fn percent_selects_whole_buffer() {
    // `%` is Shift-5: iced and the web report it with `shift: true`, so the binding must tolerate
    // Shift (IgnoreShift), not require exact no-mods — otherwise it'd only work in the terminal.
    let mut s = session();
    let shifted = Mods {
        shift: true,
        ..Mods::NONE
    };
    let fx = s.on_key(KeyCode::Char('%'), shifted, Some("%".to_string()), ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/select_all");
    assert!(params["buffer_id"].is_number());
}

#[test]
fn toggle_wrap_flips_between_soft_and_none() {
    use aether_protocol::viewport::WrapMode;
    let mut s = session();
    assert_eq!(s.wrap, WrapMode::Soft); // placeholder default
                                        // Pure state — the shell follows with a viewport/set_wrap, so no effects here.
    let fx = s.toggle_wrap();
    assert_eq!(s.wrap, WrapMode::None);
    assert!(fx.0.is_empty(), "toggle_wrap emits no effects");
    s.toggle_wrap();
    assert_eq!(s.wrap, WrapMode::Soft);
}

#[test]
fn tab_triggers_hover() {
    let mut s = session();
    // Tab fires Hover directly — no leader chord.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "lsp/hover");
}

/// The single Info-toast message in `fx`, if any.
fn info_toast(fx: &Effects) -> Option<String> {
    fx.0.iter().find_map(|e| match e {
        Effect::Toast(m, ToastKind::Info) => Some(m.clone()),
        _ => None,
    })
}

#[test]
fn hover_reports_server_readiness_instead_of_a_blank_no_info() {
    // A ready server with no content for the cursor → the genuine "nothing here" message.
    let mut s = session();
    let token = the_request(&s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS)).0;
    let fx = s.on_rpc_result(token, Ok(json!({ "contents": null, "readiness": "ready" })));
    assert_eq!(info_toast(&fx).as_deref(), Some("No hover info"));

    // A server still starting → say so, not "No hover info".
    let token = the_request(&s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS)).0;
    let fx = s.on_rpc_result(
        token,
        Ok(json!({ "contents": null, "readiness": "starting" })),
    );
    assert_eq!(
        info_toast(&fx).as_deref(),
        Some("Language server still starting")
    );

    // A crashed/stopped server → "unavailable".
    let token = the_request(&s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS)).0;
    let fx = s.on_rpc_result(
        token,
        Ok(json!({ "contents": null, "readiness": "unavailable" })),
    );
    assert_eq!(
        info_toast(&fx).as_deref(),
        Some("Language server unavailable")
    );
}

#[test]
fn space_j_shows_diagnostic_at_cursor() {
    // Space j → diagnostic at cursor. With no diagnostics loaded it reports "none" via a toast
    // (resolved locally — no RPC), which still proves the chord reaches `show_diagnostic`.
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('j'), Mods::NONE, Some("j".to_string()), ROWS);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast(_, ToastKind::Info))),
        "Space j with no diagnostics toasts an info message"
    );
}

#[test]
fn space_m_shows_blame_commit() {
    // Space m → blame the cursor line (round-trip resolves the commit's details).
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('m'), Mods::NONE, Some("m".to_string()), ROWS);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "git/blame_line");
}

#[test]
fn font_size_setting_steps_and_persists() {
    use aether_client::keymap::{KeyCode, Mods};
    use aether_client::session::AppSettingId;
    use aether_client::update::Event;
    use aether_protocol::settings::AppSettings;
    use aether_protocol::viewport::WrapMode;

    // Persisted font size is adopted into the session (render-only, like ligatures — no effects).
    let mut s = session();
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        wrap: WrapMode::Soft,
        ligatures: true,
        font_size: 16,
    })));
    assert_eq!(s.font_size, 16, "persisted font size is adopted");
    assert!(
        fx.0.is_empty(),
        "font size is render-only — no reflow effect"
    );

    // The Font size row sits in the app-settings overlay. Activating it (Enter/Space/click) steps
    // to the next preset and persists via settings/set.
    s.open_app_settings();
    let rows = s.app_setting_rows();
    let idx = rows
        .iter()
        .position(|r| matches!(r.id, AppSettingId::FontSize))
        .expect("a Font size row");
    let fx = s.app_settings_toggle(idx);
    assert_eq!(s.font_size, 18, "16 → next preset 18");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["font_size"], json!(18));

    // Left steps down to the previous preset (no wrap), also persisting.
    let fx = s.on_key(KeyCode::Left, Mods::NONE, None, ROWS);
    assert_eq!(s.font_size, 16, "Left steps down a preset");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["font_size"], json!(16));
}

#[test]
fn space_k_toggles_keep_and_guards_unsaved() {
    let mut s = session();

    // Clean transient buffer: Space k pins it permanent (transient: false).
    s.buffer.transient = true;
    s.buffer.revision = 3;
    s.buffer.saved_revision = 3;
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    let params = find_request(&fx, "buffer/set_transient").expect("Space k toggles transient");
    assert_eq!(params["buffer_id"], json!(s.buffer.buffer_id));
    assert_eq!(
        params["transient"],
        json!(false),
        "pins the transient buffer permanent"
    );

    // Clean permanent buffer: Space k releases it back to transient.
    s.buffer.transient = false;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    let params = find_request(&fx, "buffer/set_transient").expect("toggles the other way");
    assert_eq!(params["transient"], json!(true));

    // Dirty permanent buffer: Space k refuses to make it transient — silent no-op, no RPC.
    s.buffer.transient = false;
    s.buffer.revision = 5;
    s.buffer.saved_revision = 3;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    assert!(
        find_request(&fx, "buffer/set_transient").is_none(),
        "an unsaved buffer can't be made transient"
    );
    assert!(fx.0.is_empty(), "the refusal is a silent no-op");

    // A dirty *transient* buffer can still be pinned permanent — that's safe (stops it auto-closing
    // with the unsaved edits), so the guard only blocks the make-transient direction.
    s.buffer.transient = true;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    let params = find_request(&fx, "buffer/set_transient").expect("dirty transient can be pinned");
    assert_eq!(params["transient"], json!(false));
}

#[test]
fn reload_moved_to_space_alt_k() {
    let mut s = session();
    s.buffer.path = Some("/p/file.rs".into()); // reload needs a file-backed buffer

    // Reload now lives on Space Alt-k.
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert!(
        find_request(&fx, "buffer/reload").is_some(),
        "Space Alt-k reloads"
    );

    // ...and its old home, Space a, no longer reloads.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()), ROWS);
    assert!(
        find_request(&fx, "buffer/reload").is_none(),
        "Space a is no longer bound to reload"
    );
}

#[test]
fn space_a_copies_relative_and_absolute_paths() {
    let mut s = session();
    s.project_paths = vec!["/proj".into()];
    s.buffer.path = Some("/proj/src/main.rs".into());

    // Space a → project-relative path.
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()), ROWS);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("src/main.rs"));

    // Space Alt-a → absolute path.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('a'), Mods::ALT, None, ROWS);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("/proj/src/main.rs"));
}

#[test]
fn copy_path_warns_for_scratch_buffer() {
    let mut s = session();
    s.buffer.path = None; // a scratch buffer
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()), ROWS);
    assert!(
        written_clipboard(&fx).is_none(),
        "no path — nothing is copied"
    );
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast(_, ToastKind::Warning))),
        "a scratch buffer warns instead"
    );
}

// ---- application settings (Space .) -----------------------------------------------------------

#[test]
fn app_settings_overlay_opens_via_leader_dot() {
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    s.on_key(KeyCode::Char('.'), Mods::NONE, Some('.'.to_string()), ROWS);
    assert!(
        s.app_settings.is_some(),
        "Space . opens the app-settings overlay"
    );
    // The project-settings overlay (Space ,) is a distinct chord.
    assert!(s.project_settings.is_none());
}

#[test]
fn app_settings_esc_closes_the_overlay() {
    let mut s = session();
    s.open_app_settings();
    assert!(s.app_settings.is_some());
    s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.app_settings.is_none());
}

#[test]
fn app_settings_toggle_persists_and_reflows() {
    use aether_client::keymap::Action;
    use aether_protocol::viewport::WrapMode;

    let mut s = session();
    assert_eq!(s.wrap, WrapMode::Soft);
    s.open_app_settings();
    // Enter on the (single) soft-wrap row.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);

    // Persists the *post-flip* value (off) so disk matches the wrap the shell is about to apply.
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["wrap"], json!("none"));

    // Reflow: capture an anchor, then hand the shell the existing wrap-toggle action.
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::SaveContentAnchor)),
        "captures a content anchor before the reflow"
    );
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::ToggleWrap))),
        "delegates the reflow to the shell's wrap path"
    );
}

#[test]
fn app_settings_click_toggles_row_and_moves_focus() {
    let mut s = session();
    s.open_app_settings();
    // A click on row 0's checkbox toggles it and parks the selection there (so a later keypress
    // agrees on the row), persisting + reflowing exactly like the keyboard path.
    let fx = s.app_settings_toggle(0);
    assert_eq!(s.app_settings.as_ref().unwrap().selected, 0);
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["wrap"], json!("none"));

    // Out-of-range clicks (and clicks with the overlay closed) no-op.
    assert!(s.app_settings_toggle(99).0.is_empty());
    let mut closed = session();
    assert!(closed.app_settings_toggle(0).0.is_empty());
}

#[test]
fn settings_changed_push_applies_wrap_live() {
    use aether_client::keymap::Action;
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::settings::SettingsChanged;

    let push = |wrap: &str| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: SettingsChanged::NAME.into(),
            params: json!({ "wrap": wrap }),
        })
    };

    // Another client turned wrap off (differs from the Soft default) → reflow live, plus a toast.
    let mut s = session();
    let fx = s.on_event(push("none"));
    assert!(fx
        .0
        .iter()
        .any(|e| matches!(e, Effect::ShellAction(ShellAction::ToggleWrap))));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::SaveContentAnchor)));
    assert!(fx
        .0
        .iter()
        .any(|e| matches!(e, Effect::Toast(_, ToastKind::Info))));

    // A push matching the current wrap doesn't reflow (still toasts).
    let mut s = session();
    let fx = s.on_event(push("soft"));
    assert!(!fx
        .0
        .iter()
        .any(|e| matches!(e, Effect::ShellAction(ShellAction::ToggleWrap))));
}

#[test]
fn startup_fetches_persisted_settings() {
    let mut s = session();
    let fx = s.startup();
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "settings/get");
}

#[test]
fn app_settings_loaded_applies_persisted_wrap_only_when_it_differs() {
    use aether_client::keymap::Action;
    use aether_client::update::Event;
    use aether_protocol::settings::AppSettings;
    use aether_protocol::viewport::WrapMode;

    // Persisted `none` differs from the `Soft` default → reflow to apply it.
    let mut s = session();
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        wrap: WrapMode::None,
        ligatures: true,
        ..AppSettings::default()
    })));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::SaveContentAnchor)));
    assert!(fx
        .0
        .iter()
        .any(|e| matches!(e, Effect::ShellAction(ShellAction::ToggleWrap))));

    // Persisted `soft` already matches the default → nothing to do.
    let mut s = session();
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        wrap: WrapMode::Soft,
        ligatures: true,
        ..AppSettings::default()
    })));
    assert!(fx.0.is_empty(), "matching wrap is a no-op");
}

#[test]
fn app_settings_apply_and_toggle_ligatures() {
    use aether_client::update::Event;
    use aether_protocol::settings::AppSettings;
    use aether_protocol::viewport::WrapMode;

    // Ligatures default on; a persisted `false` is adopted with no reflow effect (it's render-only).
    let mut s = session();
    assert!(s.ligatures);
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        wrap: WrapMode::Soft,
        ligatures: false,
        ..AppSettings::default()
    })));
    assert!(!s.ligatures, "persisted ligatures value is adopted");
    assert!(
        fx.0.is_empty(),
        "ligatures is render-only — no reflow/shell action"
    );

    // Toggling the Ligatures row flips the value and persists it via settings/set.
    s.open_app_settings(); // the overlay must be open for a toggle to register
    let rows = s.app_setting_rows();
    let idx = rows
        .iter()
        .position(|r| matches!(r.id, aether_client::session::AppSettingId::Ligatures))
        .expect("a Ligatures row");
    let fx = s.app_settings_toggle(idx);
    assert!(s.ligatures, "toggle flips it back on");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["ligatures"], json!(true));
}

// ---- project creation + settings (docs: project creation + project settings) -----------------

#[test]
fn project_create_row_appears_for_a_novel_name_in_the_projects_picker() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.project = "aether".into();
    let _ = s.open_picker(PickerKind::Projects, None, None, false);
    let p = s.picker.as_mut().unwrap();
    p.apply_update(PickerUpdateParams {
        kind: PickerKind::Projects,
        generation: p.generation,
        offset: 0,
        items: Some(vec![PickerItem::Project {
            name: "aether".into(),
            unsaved_buffers: 0,
            match_indices: vec![],
        }]),
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        grep_display_offset: None,
        grep_total_display_rows: None,
        center_on: None,
        explorer_peek_missing: false,
    });
    // An exact match offers no create row.
    p.query = "aether".into();
    assert_eq!(p.create_row_index(), None);
    // A novel name offers the create row, one past the single match.
    p.query = "scratchpad".into();
    assert_eq!(p.create_row_index(), Some(1));
    // Path separators disqualify it (the server forbids them).
    p.query = "a/b".into();
    assert_eq!(p.create_row_index(), None);
}

#[test]
fn accepting_the_projects_create_row_emits_project_create() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.project = "aether".into();
    let _ = s.open_picker(PickerKind::Projects, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.apply_update(PickerUpdateParams {
            kind: PickerKind::Projects,
            generation: p.generation,
            offset: 0,
            items: Some(vec![PickerItem::Project {
                name: "aether".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            }]),
            total_matches: 1,
            total_candidates: 1,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        });
        p.query = "fresh".into();
        assert_eq!(p.create_row_index(), Some(1));
    }
    // Click the create row → project/create with the trimmed name; the picker closes (a hide fires).
    let fx = s.on_event(Event::PickerClicked(1));
    let create = find_request(&fx, "project/create").expect("project/create fired");
    assert_eq!(create["name"], json!("fresh"));
    assert!(s.picker.is_none(), "the picker closes on create");
}

#[test]
fn project_created_with_no_roots_opens_a_scratch_and_settings() {
    use aether_client::update::Event;
    use aether_protocol::project::{ProjectActivateResult, ProjectInfo};

    let mut s = session();
    s.project = "old".into();
    // A fresh project comes back with no roots and no landing buffer.
    let fx = s.on_event(Event::ProjectCreated(Ok(ProjectActivateResult {
        project: ProjectInfo {
            name: "fresh".into(),
            paths: vec![],
        },
        last_buffer_id: None,
        opened: None,
        server_started_at: 0,
    })));
    assert_eq!(s.project, "fresh");
    // Rather than leave the previous project's buffer behind, a scratch is opened (a `buffer/open`
    // with no buffer_id/path) so the user lands in some editor in the new project.
    let (_, method, _) = the_request(&fx);
    assert_eq!(
        method, "buffer/open",
        "opens a fresh scratch in the new project"
    );
    // The settings overlay auto-opens, focused on the add-root input (index = roots.len() + 1 = 1).
    let ps = s.project_settings.as_ref().expect("settings opened");
    assert_eq!(ps.project_name, "fresh");
    assert!(ps.roots.is_empty());
    assert_eq!(ps.selected, ps.input_index());
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast(_, ToastKind::Success))),
        "a success toast names the new project"
    );
}

#[test]
fn opening_settings_populates_state_from_the_active_project() {
    let mut s = session();
    s.project = "aether".into();
    s.project_paths = vec!["/a".into(), "/b".into()];
    s.open_project_settings();
    let ps = s.project_settings.as_ref().unwrap();
    assert_eq!(ps.project_name, "aether");
    assert_eq!(ps.name.text, "aether");
    assert_eq!(ps.roots, vec!["/a".to_string(), "/b".to_string()]);
    // Focus lands on the project-name field (index 0).
    assert_eq!(ps.selected, 0);
    assert!(ps.on_name());
}

#[test]
fn settings_add_root_emits_request_and_its_result_updates_state() {
    use aether_client::update::Event;
    use aether_protocol::project::ProjectInfo;

    let mut s = session();
    s.project = "aether".into();
    s.project_paths = vec!["/a".into()];
    s.open_project_settings();
    // Open focuses the name field; move down to the add-root input (Alt-j past the single root).
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(s.project_settings.as_ref().unwrap().on_input());
    // The shell's input owns text entry and syncs the whole value; the core no longer key-edits.
    let _ = s.project_settings_set_add("/b".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let add = find_request(&fx, "project/add_root").expect("project/add_root fired");
    assert_eq!(add["project"], json!("aether"));
    assert_eq!(add["path"], json!("/b"));
    // The result updates the session roots + the overlay's roots and clears the input.
    let _ = s.on_event(Event::ProjectRootAdded(Ok(ProjectInfo {
        name: "aether".into(),
        paths: vec!["/a".into(), "/b".into()],
    })));
    assert_eq!(s.project_paths, vec!["/a".to_string(), "/b".to_string()]);
    let ps = s.project_settings.as_ref().unwrap();
    assert_eq!(ps.roots.len(), 2);
    assert!(
        ps.add.text.is_empty(),
        "the input clears after a successful add"
    );
}

#[test]
fn settings_rename_emits_request_and_its_result_updates_the_name() {
    use aether_client::update::Event;
    use aether_protocol::project::ProjectInfo;

    let mut s = session();
    s.project = "old".into();
    s.project_paths = vec!["/a".into()];
    s.open_project_settings();
    // Move up to the name field (Alt-k from the input row to the single root to the name).
    s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert!(s.project_settings.as_ref().unwrap().on_name());
    // The shell's input owns text entry and syncs the whole value; the core no longer key-edits.
    let _ = s.project_settings_set_name("oldx".into());
    // Enter commits the rename.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let rename = find_request(&fx, "project/rename").expect("project/rename fired");
    assert_eq!(rename["project"], json!("old"));
    assert_eq!(rename["new_name"], json!("oldx"));
    // The result reconciles the committed name in both the session and the overlay.
    let _ = s.on_event(Event::ProjectRenamed(Ok(ProjectInfo {
        name: "oldx".into(),
        paths: vec!["/a".into()],
    })));
    assert_eq!(s.project, "oldx");
    let ps = s.project_settings.as_ref().unwrap();
    assert_eq!(ps.project_name, "oldx");
    assert_eq!(ps.name.text, "oldx");
}

#[test]
fn settings_remove_root_needs_confirm_then_emits_request() {
    use aether_client::session::{ConfirmAction, Prompt};
    use aether_client::update::Event;
    use aether_protocol::project::{ProjectInfo, ProjectRemoveRootResult};

    let mut s = session();
    s.project = "aether".into();
    s.project_paths = vec!["/a".into(), "/b".into()];
    s.open_project_settings();
    // Open focuses the name field (index 0); Alt-j down to the first root row (index 1).
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert_eq!(s.project_settings.as_ref().unwrap().selected, 1);
    // Delete opens the shared confirm prompt for the highlighted root (no request yet).
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None, ROWS);
    assert!(
        find_request(&fx, "project/remove_root").is_none(),
        "Delete only raises the confirm prompt"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            action: ConfirmAction::RemoveProjectRoot { project, path },
            ..
        }) => {
            assert_eq!(project, "aether");
            assert_eq!(path, "/a");
        }
        other => panic!("expected a RemoveProjectRoot confirm prompt, got {other:?}"),
    }
    // The settings overlay stays open behind the prompt.
    assert!(s.project_settings.is_some());
    // Accepting the prompt fires the remove request for the staged root.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let remove = find_request(&fx, "project/remove_root").expect("project/remove_root fired");
    assert_eq!(remove["project"], json!("aether"));
    assert_eq!(remove["path"], json!("/a"));
    assert!(s.prompt.is_none(), "the prompt closes on accept");
    // The result refreshes the roots.
    let _ = s.on_event(Event::ProjectRootRemoved(Ok(ProjectRemoveRootResult {
        project: ProjectInfo {
            name: "aether".into(),
            paths: vec!["/b".into()],
        },
        closed_buffer_ids: vec![],
        next_buffer_id: None,
    })));
    assert_eq!(s.project_paths, vec!["/b".to_string()]);
    assert_eq!(
        s.project_settings.as_ref().unwrap().roots,
        vec!["/b".to_string()]
    );
}

#[test]
fn settings_remove_root_via_click_event() {
    use aether_client::session::{ConfirmAction, Prompt};
    use aether_client::update::Event;

    let mut s = session();
    s.project = "aether".into();
    s.project_paths = vec!["/a".into(), "/b".into()];
    s.open_project_settings();
    // A clicked delete button (0-based index) opens the same confirm prompt.
    let fx = s.on_event(Event::ProjectSettingsRemoveRoot(1));
    assert!(find_request(&fx, "project/remove_root").is_none());
    match &s.prompt {
        Some(Prompt::Confirm {
            action: ConfirmAction::RemoveProjectRoot { path, .. },
            ..
        }) => assert_eq!(path, "/b"),
        other => panic!("expected a RemoveProjectRoot confirm prompt, got {other:?}"),
    }
    // Out-of-range index is a no-op.
    let mut s2 = session();
    s2.project = "aether".into();
    s2.project_paths = vec!["/a".into()];
    s2.open_project_settings();
    let _ = s2.on_event(Event::ProjectSettingsRemoveRoot(9));
    assert!(s2.prompt.is_none());
}

#[test]
fn settings_set_name_and_add_sync_text() {
    let mut s = session();
    s.project = "aether".into();
    s.project_paths = vec!["/a".into()];
    s.open_project_settings();
    // The web set methods write the field text wholesale (native <input> parity).
    s.project_settings_set_name("renamed".into());
    s.project_settings_set_add("/new/root".into());
    let ps = s.project_settings.as_ref().unwrap();
    assert_eq!(ps.name.text, "renamed");
    assert_eq!(ps.add.text, "/new/root");
    // No-op outside the overlay.
    s.project_settings = None;
    let fx = s.project_settings_set_name("x".into());
    assert!(fx.0.is_empty());
}

#[test]
fn settings_esc_closes_the_overlay() {
    let mut s = session();
    s.project = "aether".into();
    s.open_project_settings();
    assert!(s.project_settings.is_some());
    s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.project_settings.is_none());
}

#[test]
fn document_symbols_opens_scoped_to_buffer_with_no_filters() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    // The symbols picker opens unfiltered (the full hierarchy, indented by depth — no top-level
    // collapse) and scoped to the active buffer so the server can resolve symbols + the cursor.
    let fx = s.open_picker(PickerKind::DocumentSymbols, None, None, false);
    let params = find_request(&fx, "picker/view").expect("symbols picker opens via picker/view");
    assert!(
        params.get("filters").is_none(),
        "no seeded filters: {params}"
    );
    assert!(params["buffer_id"].is_number());
}

#[test]
fn symbol_push_center_on_lands_the_highlight() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::picker::{
        PickerItem, PickerKind, PickerUpdate, PickerUpdateParams, SymbolKind,
    };
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::DocumentSymbols, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.generation = 0;
        p.offset = 0;
    }
    let sym = |line: u32, name: &str| PickerItem::Symbol {
        path: "/p/a.rs".into(),
        line,
        col: 0,
        name: name.into(),
        symbol_kind: SymbolKind::Function,
        detail: String::new(),
        depth: 0,
        context: false,
        match_indices: vec![],
    };
    // The async fill push tags the cursor-enclosing symbol via `center_on`; the client adopts it
    // as the highlight (here the second row).
    let push = Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(PickerUpdateParams {
            kind: PickerKind::DocumentSymbols,
            generation: 0,
            offset: 0,
            items: Some(vec![sym(0, "a"), sym(5, "b"), sym(9, "c")]),
            total_matches: 3,
            total_candidates: 3,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: Some(Box::new(sym(5, "b"))),
            explorer_peek_missing: false,
        })
        .unwrap(),
    });
    let _ = s.on_event(push);
    let p = s.picker.as_ref().unwrap();
    assert_eq!(
        p.selected, 1,
        "center_on lands the highlight on the enclosing symbol"
    );
    assert!(p.pending_center.is_none(), "center matched in-window");
}

#[test]
fn symbol_center_on_far_down_adopts_the_framed_window() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::picker::{
        PickerItem, PickerKind, PickerUpdate, PickerUpdateParams, SymbolKind,
    };
    let mut s = session();
    s.project_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::DocumentSymbols, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.generation = 0;
        p.offset = 0; // the picker opened at the top
    }
    let sym = |line: u32, name: &str| PickerItem::Symbol {
        path: "/p/a.rs".into(),
        line,
        col: 0,
        name: name.into(),
        symbol_kind: SymbolKind::Field,
        detail: String::new(),
        depth: 1,
        context: false,
        match_indices: vec![],
    };
    // A symbol deep in the file: the server frames the window around its rank (offset 60 here) and
    // tags the fill push with `center_on`. The client must adopt that offset — otherwise the
    // offset guard discards the push and the deep symbol never gets selected.
    let push = Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(PickerUpdateParams {
            kind: PickerKind::DocumentSymbols,
            generation: 0,
            offset: 60,
            items: Some(vec![
                sym(80, "a"),
                sym(81, "externally_modified"),
                sym(82, "c"),
            ]),
            total_matches: 63,
            total_candidates: 63,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: Some(Box::new(sym(81, "externally_modified"))),
            explorer_peek_missing: false,
        })
        .unwrap(),
    });
    let _ = s.on_event(push);
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.offset, 60, "the client adopts the server's framed offset");
    assert_eq!(
        p.selected, 61,
        "the deep symbol (offset 60 + window pos 1) is selected"
    );
    assert!(
        p.pending_center.is_none(),
        "center matched within the framed window"
    );
}

/// Closing the last buffer of an ephemeral "(project N)" context doesn't spawn a scratch — it
/// leaves the context. A session *launched* for the file (`ae /path`) quits, vim-like.
#[test]
fn ephemeral_last_buffer_close_when_launched_quits() {
    let mut s = session();
    s.project = "ephemeral/1".to_string();
    s.launched_with_file = true;

    let fx = s.close_buffer();
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(false),
        "no scratch successor in an ephemeral context"
    );

    // Server reports nothing left in the project.
    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a file-launched session quits when its only buffer closes"
    );
}

/// A session that *navigated into* an ephemeral context (picked it from the switcher, or a second
/// client that joined it) returns to the project chooser instead of quitting — quitting would be
/// surprising when the app was already in use. (Web takes this branch too: it never launches with
/// a file, can't quit a tab, and its chooser is mandatory.)
#[test]
fn ephemeral_last_buffer_close_when_navigated_opens_chooser() {
    let mut s = session();
    s.project = "ephemeral/1".to_string();
    s.launched_with_file = false;

    let fx = s.close_buffer();
    let (token, _, _) = the_request(&fx);

    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a navigated-into context must not quit the app on close"
    );
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::ToChooser)),
        "it returns to the project chooser (shell-side reset) instead"
    );
}

/// When another buffer remains in the ephemeral context (several files opened into one), closing
/// one attaches to the sibling rather than leaving.
#[test]
fn ephemeral_close_with_sibling_attaches_instead_of_leaving() {
    let mut s = session();
    s.project = "ephemeral/1".to_string();

    let fx = s.close_buffer();
    let (token, _, _) = the_request(&fx);

    let fx = s.on_rpc_result(token, Ok(json!({ "next_buffer_id": 5 })));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a remaining sibling means we stay, not quit"
    );
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/open");
    assert_eq!(
        params["buffer_id"],
        json!(5),
        "attach to the remaining sibling"
    );
}

/// A persisted project is unaffected: closing its last buffer still spawns a scratch successor
/// (`open_next`), and never quits.
#[test]
fn persisted_project_close_keeps_open_next_scratch() {
    let mut s = session();
    s.project = "my-project".to_string();

    let fx = s.close_buffer();
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(true),
        "persisted projects keep the close-then-scratch behaviour"
    );
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Exit)));
}

/// `Space Alt-w` open-from-path: typing syncs into the core, Enter submits via `project/open_path`,
/// and the result is adopted like a project switch (project + buffer).
#[test]
fn open_path_prompt_submits_via_open_path_rpc() {
    use aether_client::session::{Prompt, TextField};
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::project::{ProjectActivateResult, ProjectInfo};

    let mut s = session();
    s.project = "proj".into();
    // Opening the overlay (what `A::OpenPath` does).
    s.prompt = Some(Prompt::OpenPath(TextField::new(String::new())));

    // The shell syncs typed text into the core.
    let _ = s.open_path_set_input("/etc/hosts".into());

    // Enter submits.
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "project/open_path");
    assert_eq!(params["path"], json!("/etc/hosts"));
    assert!(s.prompt.is_none(), "the overlay closes on submit");

    // The result lands like a switch: adopt the (resolved) project + opened buffer.
    let opened = BufferOpenResult {
        buffer_id: 9,
        language: None,
        line_count: 1,
        byte_count: 0,
        revision: 0,
        saved_revision: 0,
        path: Some("/etc/hosts".into()),
        scratch_number: None,
        cursor: Default::default(),
        scroll: None,
        lsp_server: None,
        transient: false,
        search_summary: None,
    };
    let result = serde_json::to_value(ProjectActivateResult {
        project: ProjectInfo {
            name: "proj".into(),
            paths: vec![],
        },
        last_buffer_id: None,
        opened: Some(opened),
        server_started_at: 0,
    })
    .unwrap();
    let fx = s.on_rpc_result(token, Ok(result));
    assert!(!has_error_toast(&fx));
    assert_eq!(s.buffer.buffer_id, 9, "adopted the opened buffer");
}

/// Esc cancels the open-from-path overlay without opening anything.
#[test]
fn open_path_prompt_esc_cancels() {
    use aether_client::session::{Prompt, TextField};
    let mut s = session();
    s.project = "proj".into();
    s.prompt = Some(Prompt::OpenPath(TextField::new("/some/path".into())));
    let fx = s.on_prompt_key(KeyCode::Esc, Mods::NONE, None);
    assert!(s.prompt.is_none(), "Esc closes the overlay");
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "cancel issues no request"
    );
}

/// Submitting an empty path is a no-op that keeps the overlay open (nothing to open yet).
#[test]
fn open_path_empty_submit_keeps_overlay_open() {
    use aether_client::session::{Prompt, TextField};
    let mut s = session();
    s.project = "proj".into();
    s.prompt = Some(Prompt::OpenPath(TextField::new("   ".into()))); // whitespace only
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        matches!(s.prompt, Some(Prompt::OpenPath(_))),
        "an empty submit leaves the overlay open"
    );
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));
}
