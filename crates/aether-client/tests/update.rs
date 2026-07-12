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

/// The token of the (single) `buffer/save` request in `fx`.
fn save_token(fx: &Effects) -> u64 {
    fx.0.iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "buffer/save" => Some(*token),
            _ => None,
        })
        .expect("a buffer/save request was emitted")
}

fn quits(fx: &Effects) -> bool {
    fx.0.iter().any(|e| matches!(e, Effect::Exit))
}

fn has_error_toast(fx: &Effects) -> bool {
    fx.0.iter().any(|e| {
        matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Error,
                ..
            }
        )
    })
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
fn shift_arrow_in_insert_mode_does_not_extend_selection() {
    // Insert mode never holds a selection, so Shift+Arrow must not extend one (unlike Normal mode,
    // where Shift extends — see `shift_extends_symbol_navigation`). It just moves the caret.
    let mut s = session();
    key(&mut s, 'i');
    assert_eq!(s.mode, aether_client::session::Mode::Insert);

    let fx = s.on_key(KeyCode::Right, Mods::SHIFT, None, ROWS);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["extend_selection"], json!(false));
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
    s.workspace_paths = vec!["/p".into()];

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
    s.workspace_paths = vec!["/p".into()];
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
fn goto_definition_outside_roots_opens_an_external_buffer() {
    use aether_client::update::Event;
    use aether_protocol::lsp::LspGotoDefinitionResult;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];

    // A definition that resolves into a dependency's source — outside every workspace root — used to
    // be refused with an "outside the workspace's roots" toast. It now opens as an *external* guest
    // buffer via `absolute_path`, still jumping to the identifier and recording nav history.
    let dep: LspGotoDefinitionResult = serde_json::from_value(json!({
        "location": {
            "path": "/home/u/.cargo/registry/src/dep-1.0/src/lib.rs",
            "position": { "line": 42, "col": 7 },
            "end": { "line": 42, "col": 12 },
        },
        "readiness": "ready",
    }))
    .unwrap();
    let fx = s.on_event(Event::Definition(Ok(dep)));
    assert!(
        !has_error_toast(&fx),
        "an external definition opens rather than erroring"
    );
    let params = find_request(&fx, "buffer/open").expect("goto-def opens the external buffer");
    assert_eq!(
        params["absolute_path"],
        json!("/home/u/.cargo/registry/src/dep-1.0/src/lib.rs"),
        "outside-root paths route through absolute_path (external buffer)"
    );
    assert!(
        params["path_index"].is_null() && params["relative_path"].is_null(),
        "the root-relative fields are unset for an external open"
    );
    // Still a transient preview, still jumps to the identifier, still records the jump origin.
    assert_eq!(params["transient"], json!(true));
    assert_eq!(params["jump_to"], json!({ "line": 42, "col": 12 }));
    assert_eq!(params["jump_to_anchor"], json!({ "line": 42, "col": 7 }));
    assert!(
        params["record_nav_from"].is_u64(),
        "the jump origin is recorded so Alt-Left returns"
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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

/// `Space Alt-q` saves the current buffer in place, then quits — but only after the save result
/// lands successfully. The quit is deferred, not fired alongside the save request.
#[test]
fn space_alt_q_saves_then_quits_on_success() {
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None, ROWS);
    // Saves in place (overwrite:false), and does NOT quit yet.
    let params = find_request(&fx, "buffer/save").expect("Space Alt-q saves first");
    assert_eq!(params["overwrite"], json!(false));
    assert!(!quits(&fx), "quit is deferred until the save succeeds");
    let token = save_token(&fx);

    // Save lands → now it quits.
    let fx = s.on_rpc_result(token, Ok(json!({ "saved_at_unix_ms": 0, "revision": 3 })));
    assert!(quits(&fx), "a successful save quits");
}

/// A failed save must not quit — `Space Alt-q` is save-*and*-quit, not quit-regardless.
#[test]
fn space_alt_q_does_not_quit_when_the_save_fails() {
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None, ROWS);
    let token = save_token(&fx);
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "buffer/save",
            code: 0,
            message: "disk full".into(),
        }),
    );
    assert!(!quits(&fx), "a failed save must not quit");
    assert!(has_error_toast(&fx), "the failure is surfaced");
}

/// The quit intent survives the overwrite/external-change confirm detour: if the save is refused
/// pending confirmation, accepting retries and — on success — still quits.
#[test]
fn space_alt_q_survives_the_external_modify_confirm() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_client::update::Event;
    use aether_protocol::error::ErrorCode;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None, ROWS);
    let token = save_token(&fx);

    // The file changed on disk → the server refuses; a confirm is raised, still no quit.
    let _ = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "buffer/save",
            code: ErrorCode::EXTERNALLY_MODIFIED.code(),
            message: "changed".into(),
        }),
    );
    assert!(
        matches!(
            &s.prompt,
            Some(Prompt::Confirm {
                kind: ConfirmKind::OverwriteModified,
                ..
            })
        ),
        "external-modify confirm, got {:?}",
        s.prompt
    );

    // Accept → retry carries overwrite:true; the quit intent is threaded through, so still no
    // quit until the retry lands.
    let fx = s.on_event(Event::PromptAccept);
    let params = find_request(&fx, "buffer/save").expect("the confirmed save retries");
    assert_eq!(params["overwrite"], json!(true));
    assert!(!quits(&fx), "no quit until the retry succeeds");
    let token = save_token(&fx);

    // Retry succeeds → now it quits.
    let fx = s.on_rpc_result(token, Ok(json!({ "saved_at_unix_ms": 0, "revision": 4 })));
    assert!(quits(&fx), "save-and-quit survives the confirm detour");
}

/// Declining the overwrite confirm re-opens the save-as prompt pre-filled, so a tweak and re-save
/// is one gesture (and re-fetches the directory listing for the ghost).
#[test]
fn declining_save_as_overwrite_reopens_the_prompt_prefilled() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;
    use aether_protocol::error::ErrorCode;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
/// client) is adopted: this client follows the rename, re-deriving its workspace-relative label. An
/// unchanged path (in-place save / reload) leaves the label alone.
#[test]
fn buffer_state_push_follows_a_save_as_rename() {
    use aether_client::update::Event;
    use aether_protocol::buffer::{BufferState, BufferStateParams};
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
fn workspace_renamed_push_adopts_the_new_name() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::workspace::{WorkspaceRenamed, WorkspaceRenamedParams};
    let push = |old: &str, new: &str| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: WorkspaceRenamed::NAME.into(),
            params: serde_json::to_value(WorkspaceRenamedParams {
                old_name: old.into(),
                new_name: new.into(),
            })
            .unwrap(),
        })
    };
    let mut s = session();
    s.workspace = "aether".into();
    // A rename of our active workspace is adopted locally (drives display + reconnect baseline).
    let _ = s.on_event(push("aether", "aether-next"));
    assert_eq!(s.workspace, "aether-next");
    // A push that doesn't match our workspace (stale / not ours) is ignored.
    let _ = s.on_event(push("something-else", "whatever"));
    assert_eq!(s.workspace, "aether-next");
}

#[test]
fn streaming_grep_view_snapshot_does_not_wipe_pushed_rows() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams, PickerViewResult};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
        groups: Vec::new(),
        display_offset: Some(0),
        total_display_rows: Some(matches + 1),
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
    s.workspace_paths = vec!["/p".into()];
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
        groups: Vec::new(),
        display_offset: Some(0),
        total_display_rows: Some(matches),
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
    s.workspace_paths = vec!["/p".into()];
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
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
fn files_picker_alt_dot_hides_hidden_with_explorer_polarity() {
    use aether_client::chips::ChipValue;
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Files, None, None, false);
    // Files shows hidden files by default; Alt-. *hides* them — the Explorer's inverted polarity,
    // not Grep's `+hidden`. So the chip records `hide: true` and wires to `hide_hidden`.
    let fx = s.on_key(KeyCode::Char('.'), Mods::ALT, None, ROWS);
    assert!(
        s.picker
            .as_ref()
            .unwrap()
            .chips
            .iter()
            .any(|c| matches!(c, ChipValue::Hidden { hide: true })),
        "Alt-. adds a hide-polarity hidden chip on Files"
    );
    let params = find_request(&fx, "picker/query").expect("filter change re-queries");
    assert_eq!(params["filters"]["hide_hidden"], true);
    assert!(
        params["filters"].get("include_hidden").is_none(),
        "Files never sends include_hidden: {}",
        params["filters"]
    );
    // Alt-. again clears the chip.
    let _ = s.on_key(KeyCode::Char('.'), Mods::ALT, None, ROWS);
    assert!(
        !s.picker
            .as_ref()
            .unwrap()
            .chips
            .iter()
            .any(|c| matches!(c, ChipValue::Hidden { .. })),
        "second Alt-. removes the chip"
    );
}

#[test]
fn lsp_picker_centers_on_the_current_buffers_server() {
    use aether_protocol::lsp::LspServerRef;
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
fn buffers_picker_centers_on_the_active_buffer() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.buffer.buffer_id = 7;
    let fx = s.open_picker(PickerKind::Buffers, None, None, false);
    let params = find_request(&fx, "picker/view").expect("buffers picker opens via picker/view");
    // The view is anchored on the active buffer (matched by buffer_id), so it opens selected.
    assert_eq!(params["center_on"]["kind"], "buffer");
    assert_eq!(params["center_on"]["buffer_id"], 7);
}

#[test]
fn workspaces_picker_centers_on_the_active_workspace() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace = "aether".into();
    let fx = s.open_picker(PickerKind::Workspaces, None, None, false);
    let params = find_request(&fx, "picker/view").expect("workspaces picker opens via picker/view");
    // The view is anchored on the active workspace (matched by name), so it opens selected.
    assert_eq!(params["center_on"]["kind"], "workspace");
    assert_eq!(params["center_on"]["name"], "aether");
}

#[test]
fn space_slash_opens_the_keybindings_picker_with_its_rows() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, '/');
    let params = find_request(&fx, "picker/view").expect("Space / opens via picker/view");
    assert_eq!(params["kind"], "keybindings");
    assert_eq!(params["reset"], true);
    // The rows ride the open: the keymap tables live client-side, the server only matches.
    let rows = params["keybindings"].as_array().expect("rows shipped");
    assert!(
        rows.len() > 50,
        "the whole keymap ships ({} rows)",
        rows.len()
    );
    assert!(rows.iter().any(|r| r["keys"] == "Space /"
        && r["desc"] == "Show keyboard shortcuts"
        && r["mode"] == "Application"));
    assert!(
        rows.iter().any(|r| r["keys"] == "Space Alt-q"
            && r["desc"] == "Save and quit"
            && r["mode"] == "Application"),
        "the new save-and-quit binding shows in help"
    );
    assert_eq!(
        s.picker.as_ref().map(|p| p.kind),
        Some(PickerKind::Keybindings)
    );
}

#[test]
fn alt_l_and_alt_h_jump_keybinding_groups_via_section_jump() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    let _ = s.open_picker(PickerKind::Keybindings, None, None, false);
    let p = s.picker.as_mut().unwrap();
    p.items = (0..6)
        .map(|n| PickerItem::Keybinding {
            group: if n < 3 { "Motion" } else { "Edit" }.into(),
            desc: format!("binding {n}"),
            mode: "Normal".into(),
            keys: "x".into(),
            match_indices: vec![],
        })
        .collect();
    p.total_matches = 6;
    p.selected = 4;
    // Alt-l / Alt-h jump by group in every header-grouped kind — the same server-side grouping
    // that produces the section headers.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/section_jump").expect("Alt-l jumps sections");
    assert_eq!(params["kind"], "keybindings");
    assert_eq!(params["from_index"], 4);
    assert_eq!(params["direction"], "forward");
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/section_jump").expect("Alt-h jumps back");
    assert_eq!(params["direction"], "backward");
}

#[test]
fn enter_on_a_keybinding_row_is_a_noop() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    let _ = s.open_picker(PickerKind::Keybindings, None, None, false);
    let p = s.picker.as_mut().unwrap();
    p.items = vec![PickerItem::Keybinding {
        group: "App".into(),
        desc: "Show keyboard shortcuts".into(),
        mode: "Application".into(),
        keys: "Space /".into(),
        match_indices: vec![],
    }];
    p.total_matches = 1;
    // Informational rows: Enter does nothing — the panel stays open, no hide, no `picker/select`.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        s.picker.is_some(),
        "Enter leaves the keybindings picker open"
    );
    assert!(
        find_request(&fx, "picker/hide").is_none(),
        "Enter doesn't dismiss the picker"
    );
    assert!(
        find_request(&fx, "picker/select").is_none(),
        "no select round-trip for an informational row"
    );
}

#[test]
fn closing_the_lsp_dialog_returns_to_the_picker() {
    use aether_client::session::Prompt;
    use aether_protocol::lsp::LspStatus;
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
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

/// The `(message, group)` of the first toast in `fx`, if any.
fn first_toast(fx: &Effects) -> Option<(String, Option<String>)> {
    fx.0.iter().find_map(|e| match e {
        Effect::Toast { message, group, .. } => Some((message.clone(), group.clone())),
        _ => None,
    })
}

#[test]
fn lsp_restart_toasts_are_grouped_per_server_and_resolve_to_ready() {
    use aether_client::session::{lsp_toast_group, Prompt};
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::lsp::{LspServerStatus, LspStatus, LspStatusChanged};

    let status = |st: LspStatus| {
        Box::new(LspServerStatus {
            name: "rust-analyzer".into(),
            language: "rust".into(),
            workspace_root: "/p".into(),
            status: st,
            progress: vec![],
        })
    };
    let push = |st: LspStatus| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: LspStatusChanged::NAME.into(),
            params: serde_json::to_value(&*status(st)).unwrap(),
        })
    };
    let group = lsp_toast_group("rust", "/p");

    let mut s = session();

    // A `status_changed` busy→idle blip with no restart pending must NOT toast.
    let fx = s.on_event(push(LspStatus::Ready));
    assert!(
        first_toast(&fx).is_none(),
        "no toast without a pending restart"
    );

    // Ctrl-r in the LSP info dialog emits a grouped "Restarting" toast keyed to this server.
    s.prompt = Some(Prompt::LspInfo(status(LspStatus::Ready)));
    let fx = s.on_key(KeyCode::Char('r'), Mods::CTRL, None, ROWS);
    assert_eq!(
        first_toast(&fx),
        Some(("Restarting rust-analyzer".into(), Some(group.clone()))),
        "restart shows a grouped Restarting toast"
    );

    // The server reaching Ready replaces it in place — same group key, "restarted" message.
    // "restarted" not "ready" because the server's handshake is done but it may still be indexing.
    let fx = s.on_event(push(LspStatus::Ready));
    assert_eq!(
        first_toast(&fx),
        Some(("rust-analyzer restarted".into(), Some(group.clone()))),
        "the ready push resolves the pending restart with a same-group toast"
    );

    // The pending restart is consumed — a later idle blip is silent again.
    let fx = s.on_event(push(LspStatus::Ready));
    assert!(
        first_toast(&fx).is_none(),
        "restart resolved; no repeat toast"
    );
}

#[test]
fn diff_toggle_toast_is_grouped() {
    use aether_client::update::Event;
    use aether_protocol::viewport::{ViewportWindowResult, Window};
    // A diff toggle result carries a window; the toast is grouped "diff" so repeated toggling
    // updates one toast instead of stacking on/off pairs.
    let mut s = session();
    let window = Window {
        first_logical_line: 0,
        last_logical_line_exclusive: 0,
        line_count: 0,
        max_scroll_logical_line: 0,
        total_visual_rows: 0,
        first_visual_row: 0,
        max_line_width: 0,
        git_status: None,
        lines: vec![],
    };
    let fx = s.on_event(Event::DiffViewSet {
        enabled: true,
        result: Ok(ViewportWindowResult { window }),
    });
    assert_eq!(
        first_toast(&fx),
        Some(("Diff on".into(), Some("diff".into())))
    );
}

#[test]
fn repeat_prone_toasts_carry_a_group_so_they_coalesce_on_every_shell() {
    use aether_client::update::Event;
    // Messages a user can re-trigger in quick succession — an invalid regex re-reported on every
    // keystroke, stepping past the last grep hit — carry a stable group. Every shell replaces one
    // toast in place by group, so these no longer stack. (The iced shell used to dedup ungrouped
    // repeats locally; grouping in the core makes that behaviour uniform and shell-agnostic.)
    let mut s = session();

    // Invalid regex mid-type: keyed so successive bad keystrokes refresh one toast.
    let fx = s.on_event(Event::SearchApplied(Err("trailing backslash".into())));
    assert_eq!(
        first_toast(&fx),
        Some(("Invalid regex".into(), Some("search-error".into()))),
    );

    // Stepping past the last grep hit: keyed so mashing next-hit coalesces.
    let fx = s.on_event(Event::SwitchedPrimed(Ok(None)));
    assert_eq!(
        first_toast(&fx),
        Some(("No more grep hits".into(), Some("grep-nav".into()))),
    );
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
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Info,
                ..
            }
        )),
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
fn edit_without_cursor_motion_still_rerequests_symbol_highlights() {
    use aether_protocol::lsp::LspServerRef;
    let mut s = session();
    s.buffer.lsp_server = Some(LspServerRef {
        language: "rust".into(),
        workspace_root: "/p".into(),
    });

    // A comment toggle with the caret in the indent edits the buffer but leaves the cursor
    // where it was. The server drops the (now stale) symbol-highlight set on every mutation,
    // so the client must re-request it on the revision bump even though nothing moved.
    let fx = ctrl(&mut s, 'y');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "input/toggle_comment");
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "revision": 3,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );
    let params =
        find_request(&fx, "lsp/document_highlight").expect("edit re-requests symbol highlights");
    assert_eq!(params["active"], true);
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
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
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

/// How many `picker/view` requests `fx` carries.
fn count_picker_views(fx: &Effects) -> usize {
    fx.0.iter()
        .filter(|e| matches!(e, Effect::Request { method, .. } if *method == "picker/view"))
        .count()
}

/// Feed a `picker/view` reply carrying a flat Files window of `n` items starting at `offset`,
/// out of `total` matches (generation 0, matching a freshly-opened picker).
fn feed_files_window(s: &mut Session, initial: bool, offset: u32, n: u32, total: u32) -> Effects {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams, PickerViewResult};
    let items = (0..n)
        .map(|i| PickerItem::File {
            path_index: offset + i,
            relative_path: format!("f{}.rs", offset + i),
            match_indices: vec![],
            git_status: None,
        })
        .collect();
    let update = PickerUpdateParams {
        kind: PickerKind::Files,
        generation: 0,
        offset,
        items: Some(items),
        total_matches: total,
        total_candidates: total,
        ticking: false,
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let r = PickerViewResult {
        query: String::new(),
        generation: 0,
        total_candidates: total,
        effective_offset: offset,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: Default::default(),
        update: Some(update),
    };
    s.on_event(Event::PickerViewed {
        initial,
        result: Ok(r),
    })
}

/// Single-flight: crossing the fetched window fires exactly one refetch and marks it in flight;
/// further moves while it's pending are coalesced (no new requests) — the selection still advances
/// locally. This is the fast-scroll pile-up cure.
#[test]
fn fast_picker_scroll_coalesces_refetches_into_one_in_flight() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.open_picker(PickerKind::Files, None, None, false);
    feed_files_window(&mut s, true, 0, 90, 500); // window [0, 90) of 500; FETCH_LIMIT = 90

    // Cross the window edge: one refetch, slot armed.
    let fx = s.picker_wheel(90); // selected 0 -> 90, leaves [0, 90)
    assert_eq!(
        count_picker_views(&fx),
        1,
        "boundary crossing fires one refetch"
    );
    assert!(s.picker.as_ref().unwrap().refetch_in_flight);
    let selected = s.picker.as_ref().unwrap().selected;

    // Two more ticks while the fetch is in flight — coalesced, no traffic, selection advances.
    let fx2 = s.picker_wheel(1);
    let fx3 = s.picker_wheel(1);
    assert_eq!(count_picker_views(&fx2), 0, "coalesced — no second refetch");
    assert_eq!(count_picker_views(&fx3), 0, "coalesced — no third refetch");
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.selected, selected + 2, "selection kept moving locally");
    assert!(p.refetch_in_flight, "still one fetch in flight");
}

/// Trailing chase: when the in-flight reply lands and coalesced moves ran the selection past the
/// window it delivered, exactly one more refetch fires, recomputed from the current selection.
#[test]
fn refetch_reply_chases_a_selection_that_raced_past_the_window() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.open_picker(PickerKind::Files, None, None, false);
    feed_files_window(&mut s, true, 0, 90, 500);

    s.picker_wheel(90); // refetch @ offset 45 fires; selected = 90
    s.picker_wheel(60); // coalesced; selected races to 150 (no request)
    assert_eq!(s.picker.as_ref().unwrap().selected, 150);

    // The in-flight reply (window [45, 135)) lands; 150 is past it → one trailing refetch at
    // 150 - 45 = 105.
    let fx = feed_files_window(&mut s, false, 45, 90, 500);
    assert_eq!(
        count_picker_views(&fx),
        1,
        "trailing chase fires one refetch"
    );
    assert_eq!(find_request(&fx, "picker/view").unwrap()["offset"], 105);
    assert!(
        s.picker.as_ref().unwrap().refetch_in_flight,
        "chase re-arms the slot"
    );
}

/// The chase stops as soon as a delivered window contains the selection: no extra refetch, slot
/// freed.
#[test]
fn refetch_reply_stops_when_it_catches_the_selection() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.open_picker(PickerKind::Files, None, None, false);
    feed_files_window(&mut s, true, 0, 90, 500);

    s.picker_wheel(90); // refetch @ 45; selected = 90
    let fx = feed_files_window(&mut s, false, 45, 90, 500); // window [45, 135) contains 90
    assert_eq!(
        count_picker_views(&fx),
        0,
        "caught up — no trailing refetch"
    );
    let p = s.picker.as_ref().unwrap();
    assert!(!p.refetch_in_flight, "slot freed");
    assert_eq!(p.items.len(), 90);
}

/// A query change abandons the window cycle, so it must free the single-flight slot — otherwise a
/// late reply from the old cycle would wedge it and coalesce every later move forever.
#[test]
fn query_change_frees_the_refetch_slot() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.open_picker(PickerKind::Files, None, None, false);
    feed_files_window(&mut s, true, 0, 90, 500);

    s.picker_wheel(90); // refetch in flight
    assert!(s.picker.as_ref().unwrap().refetch_in_flight);
    s.picker_set_query("abc".into());
    assert!(
        !s.picker.as_ref().unwrap().refetch_in_flight,
        "query change frees the slot"
    );
}

/// Free pixel scroll (iced / web scrollbar) refetches at the *scroll position* without moving the
/// selection. Its reply must NOT chase the selection back into view — that would yank the window
/// off the scroll position and, repeated against the scroll handler, oscillate the scrollbar and
/// blank the list (the native-client regression). The selection-driven chase only applies to
/// keyboard nav.
#[test]
fn free_scroll_refetch_does_not_chase_the_selection() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.open_picker(PickerKind::Files, None, None, false);
    feed_files_window(&mut s, true, 0, 90, 500); // window [0, 90), selection at 0

    // The scrollbar drags the view far from the selection: a free-scroll refetch (chase = false).
    let fx = s.picker_refetch(200, false);
    assert_eq!(count_picker_views(&fx), 1, "the scroll refetch itself");
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        0,
        "free scroll leaves the selection put"
    );

    // Window [200, 290) lands; the selection (0) is outside it — but this was free scroll, so it
    // must stay where it was scrolled, not chase back to the selection.
    let fx2 = feed_files_window(&mut s, false, 200, 90, 500);
    assert_eq!(
        count_picker_views(&fx2),
        0,
        "free scroll must not chase the selection back (no oscillation)"
    );
    let p = s.picker.as_ref().unwrap();
    assert!(!p.refetch_in_flight, "slot freed");
    assert_eq!(p.offset, 200, "window stayed where it was scrolled");
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

#[test]
fn ctrl_alt_x_cuts_the_selection_and_enters_insert() {
    use aether_client::session::Mode;

    let mut s = session();
    let ctrl_alt = Mods {
        ctrl: true,
        alt: true,
        shift: false,
    };
    let fx = s.on_key(KeyCode::Char('x'), ctrl_alt, None, ROWS);

    // Cuts via the same RPC as a plain Ctrl-x...
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/cut");
    assert_eq!(params["scope"], json!("selection"));

    // ...but unlike Ctrl-x (which stays in Normal) it leaves us in Insert at the gap.
    assert_eq!(s.mode, Mode::Insert);
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
fn workspaces_delete_confirms_then_deletes_and_guards_active() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    s.workspace = "current".into();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![
            PickerItem::Workspace {
                name: "current".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            },
            PickerItem::Workspace {
                name: "other".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            },
        ];
        p.selected = 0; // the active workspace
        p.offset = 0;
        p.total_matches = 2;
    }
    // Ctrl-d on the *active* workspace refuses client-side — no confirm, no request.
    let fx = s.picker_stage_delete();
    assert!(s.prompt.is_none(), "active workspace can't be staged");
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Error,
                ..
            }
        )),
        "refusing the active workspace surfaces an error toast"
    );

    // Move to a non-active workspace: Ctrl-d stages a confirm, sends nothing yet.
    s.picker.as_mut().unwrap().selected = 1;
    let fx = s.picker_stage_delete();
    assert!(fx.0.is_empty(), "delete stages a confirm, sends nothing");
    match &s.prompt {
        Some(Prompt::Confirm { kind, .. }) => match kind {
            ConfirmKind::DeleteWorkspace { name } => assert_eq!(name, "other"),
            other => panic!("expected a delete-workspace confirm, got {other:?}"),
        },
        other => panic!("expected a confirm prompt, got {other:?}"),
    }
    // `y` accepts → `workspace/delete { name }`.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let del = find_request(&fx, "workspace/delete").expect("workspace/delete fired");
    assert_eq!(del["name"], json!("other"));

    // A server "active in another window" refusal surfaces a clean, tailored toast — not the raw
    // `RpcError` Display (no "RPC … returned error -32005:" prefix).
    let token = fx
        .0
        .iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "workspace/delete" => Some(*token),
            _ => None,
        })
        .expect("workspace/delete token");
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "workspace/delete",
            code: aether_protocol::error::ErrorCode::ACTIVE_WORKSPACE_PREVENTS_DELETE.code(),
            message: "workspace other is active — switch to another workspace before deleting it"
                .into(),
        }),
    );
    let msg =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Toast {
                    message: m,
                    kind: ToastKind::Error,
                    ..
                } => Some(m.clone()),
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
fn buffers_picker_close_closes_in_place() {
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
            dormant: false,
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

    // A dirty buffer: closing it stages a discard confirm and sends nothing yet.
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

/// The Buffers-picker close chord is `Ctrl-d` (the delete-file gesture in the other pickers, free
/// here because the guards are keyed by picker kind). It is deliberately NOT `Ctrl-x`: every GUI
/// shell's focused query input claims Ctrl-x as its native Cut and swallows it before the core sees
/// it, so Ctrl-x would only ever work in the TUI. Closing the *active* buffer switches the editor to
/// a successor but keeps the picker open — the user is still working the list.
#[test]
fn buffers_picker_ctrl_d_closes_active_buffer_and_keeps_picker_open() {
    use aether_client::update::Event;
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::picker::{BufferDirtyState, PickerItem, PickerKind};

    fn buf(buffer_id: u64, display: &str) -> PickerItem {
        PickerItem::Buffer {
            buffer_id,
            display: display.into(),
            status: BufferDirtyState::Clean,
            path_index: None,
            relative_path: None,
            match_indices: vec![],
            transient: false,
            dormant: false,
        }
    }

    let mut s = session();
    // The active editor buffer is id 0 (placeholder default).
    let _ = s.open_picker(PickerKind::Buffers, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![buf(0, "active.rs"), buf(7, "other.rs")];
        p.offset = 0;
        p.total_matches = 2;
        p.selected = 0; // the active buffer
    }

    // Ctrl-x is deliberately NOT the close chord — the GUI shells' query inputs eat it as Cut, so it
    // must be a no-op in the core rather than a chord that only fires in the TUI.
    let fx = ctrl(&mut s, 'x');
    assert!(
        find_request(&fx, "buffer/close").is_none(),
        "Ctrl-x must not close a buffer in the Buffers picker"
    );
    assert!(
        s.picker.is_some(),
        "an unhandled chord leaves the picker open"
    );

    // Ctrl-d closes the highlighted (active) buffer, attaching its MRU successor via open_next.
    let fx = ctrl(&mut s, 'd');
    let close = find_request(&fx, "buffer/close").expect("Ctrl-d fires buffer/close");
    assert_eq!(close["buffer_id"], json!(0));
    assert_eq!(close["open_next"], json!(true));

    // When the successor switch resolves, the editor rebinds to it *and the picker stays open* — a
    // switch no longer tears the picker down (see `adopt_switch`); the pick path owns that.
    let successor = BufferOpenResult {
        buffer_id: 7,
        language: None,
        line_count: 1,
        byte_count: 0,
        revision: 0,
        saved_revision: 0,
        path: Some("/proj/other.rs".into()),
        scratch_number: None,
        cursor: Default::default(),
        scroll: None,
        lsp_server: None,
        transient: false,
        search_summary: None,
    };
    let _ = s.on_event(Event::Switched(Ok(successor)));
    assert_eq!(
        s.buffer.buffer_id, 7,
        "editor rebinds to the successor buffer"
    );
    assert!(
        s.picker.is_some(),
        "closing the active buffer from the picker keeps the picker open"
    );
}

#[test]
fn explorer_create_makes_a_file_with_create_if_missing() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
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
    s.workspace_paths = vec!["/proj".into()];
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
    s.workspace_paths = vec!["/proj".into()];
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
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
        Effect::Toast {
            message: m,
            kind: ToastKind::Info,
            ..
        } => Some(m.clone()),
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
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Info,
                ..
            }
        )),
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
fn space_p_copies_relative_and_absolute_paths() {
    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
    s.buffer.path = Some("/proj/src/main.rs".into());

    // Space p → workspace-relative path.
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()), ROWS);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("src/main.rs"));

    // Space Alt-p → absolute path.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::ALT, None, ROWS);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("/proj/src/main.rs"));
}

#[test]
fn space_p_multi_root_copies_bare_relative_path() {
    let mut s = session();
    s.workspace_paths = vec!["/proj/alpha".into(), "/proj/beta".into()];
    s.buffer.path = Some("/proj/beta/src/main.rs".into());

    // Unlike the status-bar label, the copied path carries no `root:` prefix.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()), ROWS);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("src/main.rs"));
}

#[test]
fn copy_path_warns_for_scratch_buffer() {
    let mut s = session();
    s.buffer.path = None; // a scratch buffer
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()), ROWS);
    assert!(
        written_clipboard(&fx).is_none(),
        "no path — nothing is copied"
    );
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Warning,
                ..
            }
        )),
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
    // The workspace-settings overlay (Space ,) is a distinct chord.
    assert!(s.workspace_settings.is_none());
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
    assert!(fx.0.iter().any(|e| matches!(
        e,
        Effect::Toast {
            kind: ToastKind::Info,
            ..
        }
    )));

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

// ---- workspace creation + settings (docs: workspace creation + workspace settings) -----------------

#[test]
fn workspace_create_row_appears_for_a_novel_name_in_the_workspaces_picker() {
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.workspace = "aether".into();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false);
    let p = s.picker.as_mut().unwrap();
    p.apply_update(PickerUpdateParams {
        kind: PickerKind::Workspaces,
        generation: p.generation,
        offset: 0,
        items: Some(vec![PickerItem::Workspace {
            name: "aether".into(),
            unsaved_buffers: 0,
            match_indices: vec![],
        }]),
        total_matches: 1,
        total_candidates: 1,
        ticking: false,
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
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
fn accepting_the_workspaces_create_row_emits_workspace_create() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.workspace = "aether".into();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false);
    {
        let p = s.picker.as_mut().unwrap();
        p.apply_update(PickerUpdateParams {
            kind: PickerKind::Workspaces,
            generation: p.generation,
            offset: 0,
            items: Some(vec![PickerItem::Workspace {
                name: "aether".into(),
                unsaved_buffers: 0,
                match_indices: vec![],
            }]),
            total_matches: 1,
            total_candidates: 1,
            ticking: false,
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        });
        p.query = "fresh".into();
        assert_eq!(p.create_row_index(), Some(1));
    }
    // Click the create row → workspace/create with the trimmed name; the picker closes (a hide fires).
    let fx = s.on_event(Event::PickerClicked(1));
    let create = find_request(&fx, "workspace/create").expect("workspace/create fired");
    assert_eq!(create["name"], json!("fresh"));
    assert!(s.picker.is_none(), "the picker closes on create");
}

#[test]
fn workspace_created_with_no_roots_opens_a_scratch_and_settings() {
    use aether_client::update::Event;
    use aether_protocol::workspace::{WorkspaceActivateResult, WorkspaceInfo};

    let mut s = session();
    s.workspace = "old".into();
    // A fresh workspace comes back with no roots and no landing buffer.
    let fx = s.on_event(Event::WorkspaceCreated(Ok(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            name: "fresh".into(),
            paths: vec![],
        },
        last_buffer_id: None,
        opened: None,
        server_started_at: 0,
    })));
    assert_eq!(s.workspace, "fresh");
    // Rather than leave the previous workspace's buffer behind, a scratch is opened (a `buffer/open`
    // with no buffer_id/path) so the user lands in some editor in the new workspace.
    let (_, method, _) = the_request(&fx);
    assert_eq!(
        method, "buffer/open",
        "opens a fresh scratch in the new workspace"
    );
    // The settings overlay auto-opens, focused on the add-root input (index = roots.len() + 1 = 1).
    let ps = s.workspace_settings.as_ref().expect("settings opened");
    assert_eq!(ps.workspace_name, "fresh");
    assert!(ps.roots.is_empty());
    assert_eq!(ps.selected, ps.input_index());
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Success,
                ..
            }
        )),
        "a success toast names the new workspace"
    );
}

#[test]
fn opening_settings_populates_state_from_the_active_workspace() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    s.open_workspace_settings();
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.workspace_name, "aether");
    assert_eq!(ps.name.text, "aether");
    assert_eq!(ps.roots, vec!["/a".to_string(), "/b".to_string()]);
    // Focus lands on the workspace-name field (index 0).
    assert_eq!(ps.selected, 0);
    assert!(ps.on_name());
}

#[test]
fn settings_add_root_emits_request_and_its_result_updates_state() {
    use aether_client::update::Event;
    use aether_protocol::workspace::WorkspaceInfo;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // Open focuses the name field; move down to the add-root input (Alt-j past the single root).
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(s.workspace_settings.as_ref().unwrap().on_input());
    // The shell's input owns text entry and syncs the whole value; the core no longer key-edits.
    let _ = s.workspace_settings_set_add("/b".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let add = find_request(&fx, "workspace/add_root").expect("workspace/add_root fired");
    assert_eq!(add["workspace"], json!("aether"));
    assert_eq!(add["path"], json!("/b"));
    // The result updates the session roots + the overlay's roots and clears the input.
    let _ = s.on_event(Event::WorkspaceRootAdded(Ok(WorkspaceInfo {
        name: "aether".into(),
        paths: vec!["/a".into(), "/b".into()],
    })));
    assert_eq!(s.workspace_paths, vec!["/a".to_string(), "/b".to_string()]);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.roots.len(), 2);
    assert!(
        ps.add.text.is_empty(),
        "the input clears after a successful add"
    );
}

#[test]
fn settings_rename_emits_request_and_its_result_updates_the_name() {
    use aether_client::update::Event;
    use aether_protocol::workspace::WorkspaceInfo;

    let mut s = session();
    s.workspace = "old".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // Move up to the name field (Alt-k from the input row to the single root to the name).
    s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert!(s.workspace_settings.as_ref().unwrap().on_name());
    // The shell's input owns text entry and syncs the whole value; the core no longer key-edits.
    let _ = s.workspace_settings_set_name("oldx".into());
    // Enter commits the rename.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let rename = find_request(&fx, "workspace/rename").expect("workspace/rename fired");
    assert_eq!(rename["workspace"], json!("old"));
    assert_eq!(rename["new_name"], json!("oldx"));
    // The result reconciles the committed name in both the session and the overlay.
    let _ = s.on_event(Event::WorkspaceRenamed(Ok(WorkspaceInfo {
        name: "oldx".into(),
        paths: vec!["/a".into()],
    })));
    assert_eq!(s.workspace, "oldx");
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.workspace_name, "oldx");
    assert_eq!(ps.name.text, "oldx");
}

#[test]
fn settings_remove_root_needs_confirm_then_emits_request() {
    use aether_client::session::{ConfirmAction, Prompt};
    use aether_client::update::Event;
    use aether_protocol::workspace::{WorkspaceInfo, WorkspaceRemoveRootResult};

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    s.open_workspace_settings();
    // Open focuses the name field (index 0); Alt-j down to the first root row (index 1).
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert_eq!(s.workspace_settings.as_ref().unwrap().selected, 1);
    // Delete opens the shared confirm prompt for the highlighted root (no request yet).
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None, ROWS);
    assert!(
        find_request(&fx, "workspace/remove_root").is_none(),
        "Delete only raises the confirm prompt"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            action: ConfirmAction::RemoveWorkspaceRoot { workspace, path },
            ..
        }) => {
            assert_eq!(workspace, "aether");
            assert_eq!(path, "/a");
        }
        other => panic!("expected a RemoveWorkspaceRoot confirm prompt, got {other:?}"),
    }
    // The settings overlay stays open behind the prompt.
    assert!(s.workspace_settings.is_some());
    // Accepting the prompt fires the remove request for the staged root.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()), ROWS);
    let remove = find_request(&fx, "workspace/remove_root").expect("workspace/remove_root fired");
    assert_eq!(remove["workspace"], json!("aether"));
    assert_eq!(remove["path"], json!("/a"));
    assert!(s.prompt.is_none(), "the prompt closes on accept");
    // The result refreshes the roots.
    let _ = s.on_event(Event::WorkspaceRootRemoved(Ok(WorkspaceRemoveRootResult {
        workspace: WorkspaceInfo {
            name: "aether".into(),
            paths: vec!["/b".into()],
        },
        closed_buffer_ids: vec![],
        next_buffer_id: None,
    })));
    assert_eq!(s.workspace_paths, vec!["/b".to_string()]);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().roots,
        vec!["/b".to_string()]
    );
}

#[test]
fn settings_remove_root_via_click_event() {
    use aether_client::session::{ConfirmAction, Prompt};
    use aether_client::update::Event;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    s.open_workspace_settings();
    // A clicked delete button (0-based index) opens the same confirm prompt.
    let fx = s.on_event(Event::WorkspaceSettingsRemoveRoot(1));
    assert!(find_request(&fx, "workspace/remove_root").is_none());
    match &s.prompt {
        Some(Prompt::Confirm {
            action: ConfirmAction::RemoveWorkspaceRoot { path, .. },
            ..
        }) => assert_eq!(path, "/b"),
        other => panic!("expected a RemoveWorkspaceRoot confirm prompt, got {other:?}"),
    }
    // Out-of-range index is a no-op.
    let mut s2 = session();
    s2.workspace = "aether".into();
    s2.workspace_paths = vec!["/a".into()];
    s2.open_workspace_settings();
    let _ = s2.on_event(Event::WorkspaceSettingsRemoveRoot(9));
    assert!(s2.prompt.is_none());
}

#[test]
fn settings_set_name_and_add_sync_text() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // The web set methods write the field text wholesale (native <input> parity).
    s.workspace_settings_set_name("renamed".into());
    s.workspace_settings_set_add("/new/root".into());
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.name.text, "renamed");
    assert_eq!(ps.add.text, "/new/root");
    // No-op outside the overlay.
    s.workspace_settings = None;
    let fx = s.workspace_settings_set_name("x".into());
    assert!(fx.0.is_empty());
}

#[test]
fn settings_esc_closes_the_overlay() {
    let mut s = session();
    s.workspace = "aether".into();
    s.open_workspace_settings();
    assert!(s.workspace_settings.is_some());
    s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.workspace_settings.is_none());
}

#[test]
fn document_symbols_opens_scoped_to_buffer_with_no_filters() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
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
    s.workspace_paths = vec!["/p".into()];
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
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
    s.workspace_paths = vec!["/p".into()];
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
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

/// Closing the last buffer of an ephemeral "(workspace N)" context doesn't spawn a scratch — it
/// leaves the context. A session *launched* for the file (`ae /path`) quits, vim-like.
#[test]
fn ephemeral_last_buffer_close_when_launched_quits() {
    let mut s = session();
    s.workspace = "ephemeral/1".to_string();
    s.launched_with_file = true;

    let fx = s.close_buffer();
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(false),
        "no scratch successor in an ephemeral context"
    );

    // Server reports nothing left in the workspace.
    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a file-launched session quits when its only buffer closes"
    );
}

/// A session that *navigated into* an ephemeral context (picked it from the switcher, or a second
/// client that joined it) returns to the workspace chooser instead of quitting — quitting would be
/// surprising when the app was already in use. (Web takes this branch too: it never launches with
/// a file, can't quit a tab, and its chooser is mandatory.)
#[test]
fn ephemeral_last_buffer_close_when_navigated_opens_chooser() {
    let mut s = session();
    s.workspace = "ephemeral/1".to_string();
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
        "it returns to the workspace chooser (shell-side reset) instead"
    );
}

/// When another buffer remains in the ephemeral context (several files opened into one), closing
/// one attaches to the sibling rather than leaving.
#[test]
fn ephemeral_close_with_sibling_attaches_instead_of_leaving() {
    let mut s = session();
    s.workspace = "ephemeral/1".to_string();

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

/// A persisted workspace is unaffected: closing its last buffer still spawns a scratch successor
/// (`open_next`), and never quits.
#[test]
fn persisted_workspace_close_keeps_open_next_scratch() {
    let mut s = session();
    s.workspace = "my-workspace".to_string();

    let fx = s.close_buffer();
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(true),
        "persisted workspaces keep the close-then-scratch behaviour"
    );
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Exit)));
}

/// `Space Alt-w` open-from-path: typing syncs into the core, Enter submits via `workspace/open_path`,
/// and the result is adopted like a workspace switch (workspace + buffer).
#[test]
fn open_path_prompt_submits_via_open_path_rpc() {
    use aether_client::session::{Prompt, TextField};
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::workspace::{WorkspaceActivateResult, WorkspaceInfo};

    let mut s = session();
    s.workspace = "proj".into();
    // Opening the overlay (what `A::OpenPath` does).
    s.prompt = Some(Prompt::OpenPath(TextField::new(String::new())));

    // The shell syncs typed text into the core.
    let _ = s.open_path_set_input("/etc/hosts".into());

    // Enter submits.
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "workspace/open_path");
    assert_eq!(params["path"], json!("/etc/hosts"));
    assert!(s.prompt.is_none(), "the overlay closes on submit");

    // The result lands like a switch: adopt the (resolved) workspace + opened buffer.
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
    let result = serde_json::to_value(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
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
    s.workspace = "proj".into();
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
    s.workspace = "proj".into();
    s.prompt = Some(Prompt::OpenPath(TextField::new("   ".into()))); // whitespace only
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        matches!(s.prompt, Some(Prompt::OpenPath(_))),
        "an empty submit leaves the overlay open"
    );
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));
}

// ---- sneak (s / S word-jump) --------------------------------------------------------------------

/// A session with a viewport, so `sneak/update` has an id to scope to.
fn session_with_viewport() -> Session {
    let mut s = session();
    s.viewport_id = Some(7);
    s
}

#[test]
fn sneak_arms_then_first_char_requests_update() {
    let mut s = session_with_viewport();
    // `s` arms the session but issues no traffic yet.
    let fx = key(&mut s, 's');
    assert!(s.sneak.is_some(), "sneak armed");
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));

    // First char queries the server.
    let fx = key(&mut s, 'f');
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/update");
    assert_eq!(params["query"], json!("f"));
    assert_eq!(params["viewport_id"], json!(7));

    // The label set (digits) comes back and is adopted for keystroke classification.
    let fx = s.on_rpc_result(token, Ok(json!({"labels": ["a", "b"], "match_count": 2})));
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));
    assert_eq!(s.sneak.as_ref().unwrap().labels, vec!['a', 'b']);
}

#[test]
fn sneak_label_key_selects_and_refine_narrows() {
    let mut s = session_with_viewport();
    let _ = key(&mut s, 's');
    let fx = key(&mut s, 'f');
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(token, Ok(json!({"labels": ["a", "b"], "match_count": 2})));

    // A non-label char (a letter) refines the query, it doesn't jump.
    let fx = key(&mut s, 'o');
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/update");
    assert_eq!(params["query"], json!("fo"), "refined query");
    let _ = s.on_rpc_result(token, Ok(json!({"labels": ["a"], "match_count": 1})));

    // A label key jumps: a sneak/select with the label, and the session ends locally.
    let fx = key(&mut s, 'a');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/select");
    assert_eq!(params["label"], json!("a"));
    assert_eq!(
        params.get("extend"),
        None,
        "plain `s` doesn't extend (omitted)"
    );
    assert!(s.sneak.is_none(), "session ended on label press");
}

#[test]
fn sneak_shift_select_extends() {
    let mut s = session_with_viewport();
    // `S` (Shift) arms the extend variant.
    let _ = s.on_key(KeyCode::Char('s'), Mods::SHIFT, Some("S".into()), ROWS);
    assert!(s.sneak.as_ref().unwrap().extend);
    let fx = s.on_key(KeyCode::Char('g'), Mods::SHIFT, Some("G".into()), ROWS);
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(token, Ok(json!({"labels": ["a"], "match_count": 1})));

    let fx = key(&mut s, 'a');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/select");
    assert_eq!(
        params["extend"],
        json!(true),
        "S jump extends the selection"
    );
}

#[test]
fn sneak_alt_s_targets_big_words() {
    let mut s = session_with_viewport();
    // Alt-s arms the big-word variant.
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, Some("s".into()), ROWS);
    assert!(s.sneak.as_ref().unwrap().big);
    let fx = key(&mut s, 'f');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/update");
    assert_eq!(params["big"], json!(true), "big-word query");
}

#[test]
fn sneak_backspace_unwinds_and_esc_cancels() {
    let mut s = session_with_viewport();
    let _ = key(&mut s, 's');
    let fx = key(&mut s, 'f');
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(token, Ok(json!({"labels": ["a"], "match_count": 1})));

    // Backspace shortens the query (here back to empty) and re-queries.
    let fx = s.on_key(KeyCode::Backspace, Mods::NONE, None, ROWS);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/update");
    assert_eq!(params["query"], json!(""));
    assert!(s.sneak.is_some(), "still armed after backspace");

    // Esc cancels: a sneak/cancel and the session ends.
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "sneak/cancel");
    assert!(s.sneak.is_none(), "session ended on Esc");
}

#[test]
fn space_alt_x_asks_the_shell_to_open_a_new_window() {
    let mut s = session();
    // `Space Alt-x` — a leader chord distinct from `Space x` (close buffer).
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None, ROWS);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::NewWindow(_)))),
        "Space Alt-x should emit ShellAction::NewWindow"
    );
    // It's a pure shell hand-off — no server traffic, and crucially not a buffer/close (that's
    // `Space x`, the un-Alted chord).
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "opening a window issues no RPC"
    );
}
