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

fn ctrl_alt(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::CTRL_ALT, None, ROWS)
}

/// No `Effect::Request` in `fx` — the input was swallowed (hint/toast effects may still ride).
fn no_request(fx: &Effects) -> bool {
    !fx.0.iter().any(|e| matches!(e, Effect::Request { .. }))
}

/// Every `Effect::Request` in `fx`, in emission (= wire) order — for composite actions that
/// queue several.
fn all_requests(fx: &Effects) -> Vec<(&'static str, serde_json::Value)> {
    fx.0.iter()
        .filter_map(|e| match e {
            Effect::Request { method, params, .. } => Some((*method, params.clone())),
            _ => None,
        })
        .collect()
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
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    let mut s = session();
    // The save-as prompt's text is owned by each shell's input; the core only stores the value
    // and handles command keys. A typed char reaching the core must NOT edit the value.
    s.prompt = Some(Prompt::SaveAs(Box::new(PathEditor::new(
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

/// A `viewport/lines_changed` push carrying a cursor adopts it — the server moved the cursor
/// with no request in flight (e.g. the clamp a watcher reload applies when the file shrank
/// under it). A push without one leaves the client's cursor alone.
#[test]
fn lines_changed_push_adopts_the_server_cursor() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;
    use aether_protocol::LogicalPosition;

    let mut s = session();
    s.viewport_id = Some(7);

    let push = |cursor: serde_json::Value| {
        let mut params = json!({
            "viewport_id": 7,
            "revision": 9,
            "range": {"start_logical_line": 0, "end_logical_line_exclusive": 6},
            "replacement_lines": [],
            "line_count": 6,
            "max_scroll_logical_line": 0,
            "total_visual_rows": 6,
            "first_visual_row": 0,
            "max_line_width": 0,
        });
        if !cursor.is_null() {
            params["cursor"] = cursor;
        }
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: ViewportLinesChanged::NAME.into(),
            params,
        })
    };

    let _ = s.on_event(push(
        json!({"position": {"line": 5, "col": 2}, "anchor": {"line": 5, "col": 2}}),
    ));
    assert_eq!(
        s.buffer.cursor.position,
        LogicalPosition { line: 5, col: 2 },
        "the pushed cursor is adopted"
    );

    // No cursor on the push (nothing stored server-side): local state is kept.
    let _ = s.on_event(push(serde_json::Value::Null));
    assert_eq!(
        s.buffer.cursor.position,
        LogicalPosition { line: 5, col: 2 },
        "a cursor-less push leaves the cursor alone"
    );
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
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
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
        expanded_run: None,
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
        path_filterable: false,
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
fn view_response_does_not_regress_a_query_typed_before_it() {
    // Request pipelining: the user types into a fresh picker before its `picker/view` response
    // has arrived. Typing claims the generation (the server adopts `picker/query`'s number), so
    // the response's carried snapshot — the slot's pre-reopen generation and its resumed (empty)
    // query — must not regress either: adopting them would clobber the typed query and orphan
    // the query's own push. (Pushes can't race the response itself — shells deliver server
    // messages in wire order, docs/client-core.md — so pipelining is the one case this gates.)
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::picker::{
        PickerItem, PickerKind, PickerUpdate, PickerUpdateParams, PickerViewResult, SymbolKind,
    };
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::DocumentSymbols, None, None, false, None);
    // Type "f" while the view response is still in flight: generation 0 → 1, claimed.
    let _ = key(&mut s, 'f');
    {
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.query, "f");
        assert_eq!(p.generation, 1);
    }
    // The view response lands late, carrying the slot's carried generation (4) and the resumed
    // empty query. Neither may overwrite what typing established.
    let view = PickerViewResult {
        query: String::new(),
        generation: 4,
        total_candidates: 0,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: Default::default(),
        path_filterable: false,
        update: None,
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(view),
    });
    {
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.query, "f", "typed query survives the late response");
        assert_eq!(p.generation, 1, "claimed generation survives the late response");
    }
    // The query's own push (the server adopted generation 1) applies and settles the picker.
    let sym = |line: u32, name: &str| PickerItem::Symbol {
        path: "/p/a.rs".into(),
        display_path: String::new(),
        line,
        col: 0,
        name: name.into(),
        symbol_kind: SymbolKind::Function,
        detail: String::new(),
        depth: 0,
        context: false,
        match_indices: vec![],
    };
    let push = PickerUpdateParams {
        kind: PickerKind::DocumentSymbols,
        generation: 1,
        offset: 0,
        items: Some(vec![sym(0, "foo"), sym(5, "fizz")]),
        total_matches: 2,
        total_candidates: 2,
        ticking: false,
        groups: Vec::new(),
        display_offset: None,
        total_display_rows: None,
        expanded_run: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let _ = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(&push).unwrap(),
    }));
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.items.len(), 2, "the query's push applies under the claimed generation");
    assert!(!p.ticking);
}

#[test]
fn grep_count_only_ticks_keep_the_window_then_the_first_batch_replaces_it() {
    // The grep streaming sequence at the core: the previous query's hits stay put through the
    // initial count-only tick (`items: None`) and the throttled count ticks while the new search
    // runs, then the first real batch replaces them — so the list never blanks mid-type.
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
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
        expanded_run: None,
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
        expanded_run: None,
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
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
fn jumplist_path_chips_gate_on_the_path_filterable_echo() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerKind, PickerViewResult};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Jumplist, None, None, false, None);

    // Before the view result lands (and whenever the capture isn't worth scoping — one file,
    // or nothing in-root) the dir/glob chords are clean no-ops.
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    assert!(
        s.picker.as_ref().unwrap().chip_editor.is_none(),
        "Alt-g must not open the glob editor without the path_filterable echo"
    );
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None, ROWS);
    assert!(s.picker.as_ref().unwrap().chip_editor.is_none());

    // The server says this capture spans in-root files → the path chips apply.
    let view = PickerViewResult {
        query: String::new(),
        generation: 0,
        total_candidates: 3,
        effective_offset: 0,
        effective_center_on: None,
        directory_path: None,
        directory_parent: None,
        filters: Default::default(),
        path_filterable: true,
        update: None,
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(view),
    });
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    assert!(
        s.picker.as_ref().unwrap().chip_editor.is_some(),
        "Alt-g opens the glob editor once the echo lands"
    );
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);

    // The pattern chips never apply to the Jumplist — its query is a fuzzy match over the
    // captured row text, not a content regex — regardless of the flag.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None, ROWS);
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "Alt-w stays a no-op on the Jumplist picker"
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
    let fx = s.open_picker(PickerKind::LspServers, None, None, false, None);
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
    let fx = s.open_picker(PickerKind::Buffers, None, None, false, None);
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
    let fx = s.open_picker(PickerKind::Workspaces, None, None, false, None);
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
    assert_eq!(params["reset"], "all");
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
    let _ = s.open_picker(PickerKind::Keybindings, None, None, false, None);
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

/// A collapsible picker window (docs/picker-groups.md): a.rs collapsed with 2 hidden hits,
/// b.rs expanded (selected group) with its 2 hits inline. Row space: [0]=a.rs hdr,
/// [1]=b.rs hdr, [2..3]=hits.
fn grep_with_groups(s: &mut Session) {
    use aether_protocol::picker::{ExpandedRun, GroupHeader, GroupSpan, PickerItem, PickerKind};
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    let p = s.picker.as_mut().unwrap();
    let group = |path: &str, count: u32, expanded: bool| PickerItem::Group {
        header: GroupHeader::File {
            path_index: 0,
            relative_path: path.into(),
        },
        count,
        expanded,
    };
    let hit = |line: u32| PickerItem::GrepHit {
        path_index: 0,
        relative_path: "b.rs".into(),
        line,
        col: 0,
        preview: "x".into(),
        match_indices: vec![],
    };
    p.items = vec![
        group("a.rs", 2, false),
        group("b.rs", 2, true),
        hit(1),
        hit(2),
    ];
    p.groups = ["a.rs", "b.rs"]
        .iter()
        .enumerate()
        .map(|(i, path)| GroupSpan {
            start: i as u32,
            header: GroupHeader::File {
                path_index: 0,
                relative_path: (*path).into(),
            },
            count: Some(2),
            expanded: Some(i == 1),
        })
        .collect();
    p.total_matches = 4;
    p.total_display_rows = 4;
    p.expanded_run = Some(ExpandedRun {
        header_row: 1,
        len: 2,
    });
}

#[test]
fn alt_l_descends_into_the_selected_group() {
    let mut s = session();
    grep_with_groups(&mut s);
    // On the selected (expanded) group's header: Alt-l descends onto the run's first item —
    // a local move, no round-trip (docs/picker-groups.md §9).
    s.picker.as_mut().unwrap().selected = 1;
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert!(find_request(&fx, "picker/section_jump").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    // On an item row: as deep as it goes — nothing fires.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx));
    // On a header that is NOT the open group (transient, post-re-rank): Alt-l re-selects
    // that group first — each press makes progress.
    s.picker.as_mut().unwrap().selected = 0;
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("Alt-l re-selects the group");
    assert_eq!(params["kind"], "grep");
    assert_eq!(params["header"]["relative_path"], "a.rs");
    assert!(params.get("step").is_none(), "header-addressed, not a step");
}

#[test]
fn alt_h_ascends_to_the_header_and_never_touches_the_query() {
    let mut s = session();
    grep_with_groups(&mut s);
    // On an item row: ascend onto the run's header — a local move, nothing collapses
    // (moving the group selection is what moves the expansion, docs/picker-groups.md §9).
    {
        let p = s.picker.as_mut().unwrap();
        p.selected = 3;
        p.level = aether_client::picker::PickerLevel::Item;
    }
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx), "ascend is local — no set_group");
    // A plain row reveal — run framing is the group *select* gesture's, not ascend's.
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::RevealPickerSelection(aether_client::picker::Reveal::Minimal)
        )),
        "ascend reveals minimally"
    );
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        1,
        "lands on the header"
    );
    // On a header: as shallow as it goes — a no-op. Alt-h never wipes the query
    // (that's Alt-Backspace's).
    let p = s.picker.as_mut().unwrap();
    p.selected = 0;
    p.query = "needle".into();
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx));
    assert_eq!(
        s.picker.as_ref().unwrap().query,
        "needle",
        "the query survives Alt-h"
    );
    // Alt-Backspace is the unwind: clear the query — and never a group gesture.
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert!(
        find_request(&fx, "picker/query").is_some(),
        "unwind stage: clear the query"
    );
    assert_eq!(s.picker.as_ref().unwrap().query, "");
}

#[test]
fn alt_jk_step_groups_at_group_level_and_walk_the_run_at_item_level() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    let mut s = session();
    grep_with_groups(&mut s);
    // Group level (selection on a header): Alt-j/k are a server-resolved group *step* —
    // the neighbour may sit past the fetched window (docs/picker-groups.md §9).
    s.picker.as_mut().unwrap().selected = 1;
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("group-level Alt-j steps");
    assert_eq!(params["step"], "forward");
    assert!(params.get("header").is_none(), "step-addressed, no header");
    // Resolve the gesture (a stop releases the single-flight guard at reply time — no
    // reshaping push follows a stop) so the next key isn't swallowed.
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::Header));
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("group-level Alt-k steps");
    assert_eq!(params["step"], "backward");
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::Header));
    // Item level — entered by the *descend gesture* (Alt-l), which is what flips the stored
    // level bit; poking `selected` into the run alone must not (that's the held-key guard,
    // see `PickerLevel`). Local moves clamp to the run.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 3);
    // At the run's last row Alt-j *spills* into the next group — an RPC, not a local
    // walk-out; the selection waits for the reply. (Landings are exercised in
    // item_level_spills_across_group_edges.)
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("edge spill steps the group");
    assert_eq!(params["step"], "forward");
    assert_eq!(s.picker.as_ref().unwrap().selected, 3);
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::RunStart)); // the very end: a stop
                                                                           // Back inside, Alt-k walks locally…
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    // …and at the run's first row it spills backward.
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("upward spill steps back");
    assert_eq!(params["step"], "backward");
}

/// The held-`Alt-j` race (see `PickerLevel` / `group_gesture_in_flight`): a group step's
/// outcome arrives as two order-independent messages — the reply moves `selected`, the
/// reshaping push moves `expanded_run`. A repeat firing between them used to derive "item
/// level" from the *new* selection row against the *stale* run interval and walk into the
/// run. Now repeats during a gesture are swallowed (single-flight, released by the push's
/// adoption), and the stored level bit backstops the routing either way.
#[test]
fn held_group_step_keeps_stepping_through_the_reply_push_gap() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    use aether_protocol::picker::ExpandedRun;
    let mut s = session();
    grep_with_groups(&mut s);
    s.picker.as_mut().unwrap().selected = 1; // b.rs's header — group level
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_some());
    // A repeat while the gesture is mid-reshape is swallowed, not misrouted.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx), "repeat during the gesture is swallowed");
    // The reply lands the next group's header row *in the incoming row space* (row 2), while
    // the stale local `expanded_run` ({header_row: 1, len: 2}) still claims rows 2..=3 as
    // b.rs's items — the misclassifying pair (the reshaping push hasn't been adopted yet).
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(ExpandedRun {
            header_row: 2,
            len: 4,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    // A repeat in the reply→push gap: still swallowed — and crucially NOT a local walk into
    // the stale interval.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().selected, 2, "no local walk");
    // The reshaping push adopts (fresh run + guard release): stepping resumes.
    {
        let p = s.picker.as_mut().unwrap();
        p.expanded_run = Some(ExpandedRun {
            header_row: 2,
            len: 4,
        });
        p.group_gesture_in_flight = false;
    }
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("stepping resumes after adoption");
    assert_eq!(params["step"], "forward");
}

/// Item-level `Alt-j`/`Alt-k` spill over the run's edges (docs/picker-groups.md §9): down
/// off the last item enters the next group at its *first* item, up off the first enters the
/// previous at its *last* — both staying at item level, revealed minimally (a continuous
/// scan, not a run framing).
#[test]
fn item_level_spills_across_group_edges() {
    use aether_client::picker::{GroupLanding, Reveal};
    use aether_client::update::Event;
    use aether_protocol::picker::ExpandedRun;
    let mut s = session();
    grep_with_groups(&mut s);
    // Enter b.rs's run (header row 1, items 2..=3) and walk to its last item.
    s.picker.as_mut().unwrap().selected = 1;
    let _ = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS); // descend → 2
    let _ = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS); // → 3 (last)
                                                                 // Down off the last item: the same step RPC as group navigation — the landing intent
                                                                 // stays client-side.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "picker/set_group").expect("edge spill steps the group");
    assert_eq!(params["step"], "forward");
    // The reply carries the newly selected run's geometry; a RunStart landing enters at its
    // first item — item level, minimal reveal.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(ExpandedRun {
            header_row: 4,
            len: 5,
        })),
        GroupLanding::RunStart,
    ));
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        5,
        "first item of the entered run"
    );
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::RevealPickerSelection(Reveal::Minimal))),
        "spills reveal minimally, not run-framed"
    );
    // A RunEnd landing (an upward spill) enters at the previous run's last item.
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(ExpandedRun {
            header_row: 0,
            len: 4,
        })),
        GroupLanding::RunEnd,
    ));
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        4,
        "last item of the entered run"
    );
    // Once the reshaping push adopts (run + guard release), local walking resumes at item
    // level from the landing row.
    {
        let p = s.picker.as_mut().unwrap();
        p.expanded_run = Some(ExpandedRun {
            header_row: 0,
            len: 4,
        });
        p.group_gesture_in_flight = false;
    }
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 3, "local move resumes");
}

#[test]
fn alt_h_is_unbound_in_flat_pickers() {
    use aether_protocol::picker::PickerKind;
    // Files: Alt-h used to clear the query, duplicating Alt-Backspace; now only Alt-Backspace
    // unwinds and Alt-h does nothing (and must not leak an `h` into the query).
    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    s.picker.as_mut().unwrap().query = "needle".into();
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().query, "needle");
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None, ROWS);
    assert!(
        find_request(&fx, "picker/query").is_some(),
        "Alt-Backspace still clears"
    );
    assert_eq!(s.picker.as_ref().unwrap().query, "");
}

#[test]
fn explorer_alt_h_ascends_regardless_of_the_query() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src/sub".into());
        p.directory_parent = Some("/proj/src".into());
        p.query = "ma".into();
    }
    // Alt-h is the structural mirror of Alt-l's descend: one press ascends the breadcrumb even
    // with a query typed (navigation starts a fresh listing) — clearing the query *first* and
    // staying put is Alt-Backspace's unwind, not Alt-h's.
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    let view = find_request(&fx, "picker/view").expect("ascends via picker/view");
    assert_eq!(view["directory_path"], json!("/proj/src"));
}

#[test]
fn enter_on_a_group_header_jumps_to_its_first_item() {
    let mut s = session();
    grep_with_groups(&mut s);
    // Enter on a header IS a jump (docs/picker-groups.md §9): the Group row rides
    // `picker/select` and the server resolves it to the group's first item — so
    // type-query-then-Enter takes the top hit without a mandatory descend. The picker
    // closes like any accept.
    s.picker.as_mut().unwrap().selected = 0;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(find_request(&fx, "picker/set_group").is_none());
    let params = find_request(&fx, "picker/select").expect("Enter selects the header");
    assert_eq!(params["item"]["kind"], "group");
    assert_eq!(params["item"]["header"]["relative_path"], "a.rs");
    assert!(find_request(&fx, "picker/hide").is_some());
    assert!(s.picker.is_none(), "accept closes the picker");
}

#[test]
fn clicking_a_group_header_selects_it_instead_of_jumping() {
    use aether_client::update::Event;
    let mut s = session();
    grep_with_groups(&mut s);
    // A header click is the disclosure gesture: select (and expand) the group — no select,
    // no close. The mouse path to a jump is clicking a visible item row.
    let fx = s.on_event(Event::PickerClicked(0));
    let params = find_request(&fx, "picker/set_group").expect("click selects the group");
    assert_eq!(params["header"]["relative_path"], "a.rs");
    assert!(find_request(&fx, "picker/select").is_none());
    assert!(s.picker.is_some(), "the picker stays open");
    // Clicking an item row accepts it, as ever.
    let fx = s.on_event(Event::PickerClicked(2));
    assert!(find_request(&fx, "picker/select").is_some());
}

#[test]
fn group_set_landing_seats_the_selection() {
    use aether_client::picker::{GroupLanding, Reveal};
    use aether_client::update::Event;
    use aether_protocol::picker::ExpandedRun;
    let mut s = session();
    grep_with_groups(&mut s);
    s.picker.as_mut().unwrap().selected = 3;
    // A Header landing seats the selection on the selected run's header row.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(ExpandedRun {
            header_row: 1,
            len: 2,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 1);
    assert!(no_request(&fx), "in-window: no refetch needed");
    // Group navigation frames the whole freshly-opened run, not just its header row —
    // immediately and re-armed for the reshaped push (docs/picker-groups.md §9).
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::RevealPickerSelection(Reveal::Run))),
        "group select reveals the run"
    );
    assert_eq!(
        s.picker.as_ref().unwrap().reveal_on_update,
        Some(Reveal::Run)
    );
    // A landing outside the fetched window chases with a refetch.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(ExpandedRun {
            header_row: 90,
            len: 3,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 90);
    assert!(
        find_request(&fx, "picker/view").is_some(),
        "out-of-window: refetch"
    );
    // A vanished group / a step off the ends adopts nothing.
    let before = s.picker.as_ref().unwrap().selected;
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::Header));
    assert_eq!(s.picker.as_ref().unwrap().selected, before);
}

#[test]
fn enter_on_a_keybinding_row_is_a_noop() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    let _ = s.open_picker(PickerKind::Keybindings, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::LspServers, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::LspServers, None, None, false, None);
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
        expanded_run: None,
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

/// `Space ?` fetches the snapshot rather than composing one client-side (the build, pid, port and
/// counts all describe the *server*), then opens the dialog when it lands.
#[test]
fn space_question_opens_the_app_info_dialog() {
    use aether_client::session::Prompt;
    use aether_client::update::Event;

    let mut s = session();
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    // A terminal reports `?` with SHIFT held; the binding uses `IgnoreShift` so both that and the
    // GUI/web's already-resolved character hit it.
    let fx = s.on_key(KeyCode::Char('?'), Mods::SHIFT, Some("?".into()), ROWS);
    assert!(
        find_request(&fx, "app/info").is_some(),
        "the dialog's content is fetched from the server"
    );
    assert!(s.prompt.is_none(), "nothing opens until the snapshot lands");

    let _ = s.on_event(Event::AppInfoLoaded(Ok(app_info())));
    assert!(matches!(s.prompt, Some(Prompt::AppInfo(_))), "dialog opens");
}

/// `Space ?` while disconnected opens the dialog anyway — composed from client-side facts (our
/// build + the connection state) — instead of silently dropping the RPC. Diagnostics matter most
/// exactly when the server is unreachable.
#[test]
fn space_question_opens_client_side_info_while_disconnected() {
    use aether_client::session::{ConnState, Prompt};

    let mut s = session();
    s.conn = ConnState::Reconnecting {
        attempt: 0,
        had_unsaved: false,
    };
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('?'), Mods::SHIFT, Some("?".into()), ROWS);
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "nothing to fetch while disconnected"
    );
    assert!(
        matches!(s.prompt, Some(Prompt::AppInfo(None))),
        "the client-side dialog opens immediately"
    );
}

/// `Ctrl-c` copies the whole snapshot and *stays open* (copying isn't dismissing); any other key
/// closes. It's the editor's own Copy chord — safe here because the dialog has no text input to
/// claim it first, unlike a picker's query field.
#[test]
fn app_info_ctrl_c_copies_and_keeps_the_dialog_open() {
    use aether_client::session::Prompt;

    let mut s = session();
    s.prompt = Some(Prompt::AppInfo(Some(Box::new(app_info()))));
    let fx = s.on_key(KeyCode::Char('c'), Mods::CTRL, None, ROWS);
    let copied = written_clipboard(&fx).expect("Ctrl-c copies");
    // The copied text is the rendered dialog, so a row can't exist in one and not the other.
    assert!(copied.contains("0.9.9") && copied.contains("dev") && copied.contains("Paths"));
    assert!(
        matches!(s.prompt, Some(Prompt::AppInfo(_))),
        "copying leaves the dialog up"
    );

    // A bare `c` is not the copy chord — it closes like any other key.
    let fx = s.on_key(KeyCode::Char('c'), Mods::NONE, Some("c".into()), ROWS);
    assert!(s.prompt.is_none(), "any other key closes");
    assert!(written_clipboard(&fx).is_none());

    s.prompt = Some(Prompt::AppInfo(Some(Box::new(app_info()))));
    let fx = s.on_key(KeyCode::Char('q'), Mods::NONE, Some("q".into()), ROWS);
    assert!(s.prompt.is_none(), "any other key closes");
    assert!(written_clipboard(&fx).is_none());
}

/// A failed fetch surfaces as an error toast instead of an empty dialog.
#[test]
fn app_info_failure_toasts_rather_than_opening() {
    use aether_client::update::Event;

    let mut s = session();
    let fx = s.on_event(Event::AppInfoLoaded(Err("server gone".into())));
    assert!(s.prompt.is_none());
    assert!(fx.0.iter().any(|e| matches!(
        e,
        aether_client::effect::Effect::Toast { message, .. } if message.contains("server gone")
    )));
}

fn app_info() -> aether_protocol::app::AppInfo {
    aether_protocol::app::AppInfo {
        version: "0.9.9".into(),
        commit: Some("abc1234".into()),
        commit_dirty: false,
        debug_build: false,
        appimage: None,
        profile: "dev".into(),
        port: Some(2385),
        pid: 42,
        started_at_unix_ms: 0,
        uptime_secs: 90,
        idle_timeout_secs: None,
        clients: 1,
        buffers_open: 2,
        buffers_unsaved: 0,
        workspaces_active: 1,
        paths: aether_protocol::app::AppPaths {
            config_dir: Some("/c".into()),
            ..Default::default()
        },
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

    // Stepping with nothing captured: keyed so mashing `]` coalesces.
    let fx = s.on_event(Event::JumplistStepped(
        Ok(aether_protocol::jumplist::JumplistStepResult::Empty),
        aether_protocol::cursor::Direction::Forward,
        aether_protocol::jumplist::JumplistStepScope::Full,
    ));
    assert_eq!(
        first_toast(&fx),
        Some((
            "Jumplist is empty — Ctrl-j in a picker captures results".into(),
            Some("jumplist".into())
        )),
    );

    // Stepping past the last entry: the boundary toast is keyed the same, so `]` at the end
    // coalesces too and names the end reached.
    let fx = s.on_event(Event::JumplistStepped(
        Ok(aether_protocol::jumplist::JumplistStepResult::AtEnd),
        aether_protocol::cursor::Direction::Forward,
        aether_protocol::jumplist::JumplistStepScope::Full,
    ));
    assert_eq!(
        first_toast(&fx),
        Some(("Last jumplist entry".into(), Some("jumplist".into()))),
    );

    // `Alt-]` (file-scoped) with no entries in the current file — a distinct keyed toast.
    let fx = s.on_event(Event::JumplistStepped(
        Ok(aether_protocol::jumplist::JumplistStepResult::NoneInFile),
        aether_protocol::cursor::Direction::Forward,
        aether_protocol::jumplist::JumplistStepScope::CurrentFile,
    ));
    assert_eq!(
        first_toast(&fx),
        Some((
            "No jumplist entries in this file — ] steps across files".into(),
            Some("jumplist".into())
        )),
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
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
    let fx = s.open_picker(PickerKind::GitChangesFile, None, None, false, None);
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
        "no seeded filters — grep-for-selection is a fresh workspace-wide open"
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

/// Each `/` opens at the defaults — options are part of the search you're running, not standing
/// configuration, matching how a picker open resets its chips. Esc still restores the committed
/// search *and* the options it ran under, because the snapshot is taken before the reset.
#[test]
fn search_prompt_opens_with_default_options() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::{CaseMode, MatchOptions};
    let mut s = session();

    // Commit a regex, case-sensitive search.
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("fn \\w+".into());
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None, ROWS);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS);
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(s.search.active);
    assert!(s.search.options.regex && s.search.options.case == CaseMode::Sensitive);

    // Re-opening the prompt starts clean: no leftover regex to silently change what the next
    // query matches, and no chips rendered above it.
    let _ = key(&mut s, '/');
    assert_eq!(s.search.options, MatchOptions::default());
    assert_eq!(s.search.query, "");
    assert!(s.search.option_chips().is_empty());

    // The next search runs literally, without inheriting anything.
    let fx = s.search_set_query("fn".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params.get("options"), None, "all-default options, skipped");

    // Esc puts the previous search back exactly as it was — query, active flag and options.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "fn \\w+");
    assert!(s.search.active);
    assert!(s.search.options.regex && s.search.options.case == CaseMode::Sensitive);
}

/// `Alt-/` starts a search too, so it starts one at the defaults — it must not inherit the options
/// of whatever search ran before it, and unlike the prompt it shows no chip row that would reveal
/// what got carried over.
#[test]
fn search_from_selection_runs_at_default_options() {
    use aether_client::keymap::Mods;
    let mut s = session();
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("foo".into());
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None, ROWS); // regex
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None, ROWS); // whole-word
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);

    let fx = s.search_from_selection();
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params["from_selection"], json!(true));
    assert_eq!(
        params.get("options"),
        None,
        "all-default: literal, smartcase, no whole-word inherited from the previous search"
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
fn ctrl_alt_g_unjoins_in_both_modes() {
    // Join's dual: `Ctrl-Alt-g` un-joins — the break lands at the cursor and the cursor stays
    // before it (`park_before`), so a following join re-joins the same pair. From the Global
    // table, so it works in Normal mode as well as Insert.
    let mut s = session();
    let fx = ctrl_alt(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/newline_and_indent");
    assert_eq!(params["park_before"], json!(true));

    let _ = key(&mut s, 'i');
    let fx = ctrl_alt(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/newline_and_indent");
    assert_eq!(params["park_before"], json!(true));
}

#[test]
fn enter_is_newline_and_indent_in_insert() {
    let mut s = session();
    let _ = key(&mut s, 'i');
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/newline_and_indent");
    // Enter advances onto the new line — no parking.
    assert!(params.get("park_before").is_none());
    assert_eq!(s.mode, aether_client::session::Mode::Insert);
}

#[test]
fn paste_text_routes_by_mode() {
    // Insert: plain insert at the caret, exactly like the Ctrl-v gesture.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let fx = s.paste_text("one\ntwo".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/text");
    assert_eq!(params["text"], json!("one\ntwo"));
    assert_eq!(params["select_pasted"], json!(false));
    assert!(params.get("at").is_none());

    // Normal: paste before the selection, selecting the pasted text.
    let mut s = session();
    let fx = s.paste_text("one\ntwo".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "input/text");
    assert_eq!(params["select_pasted"], json!(true));
    assert_eq!(params["at"], json!("start"));
}

#[test]
fn paste_text_normalizes_line_endings_and_strips_controls() {
    // Terminals disagree on pasted newlines (CR, CRLF, LF) — all land as `\n`; other control
    // chars are filtered as typed input would be, tabs survive.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let fx = s.paste_text("a\r\nb\rc\u{7}\td".into());
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["text"], json!("a\nb\nc\td"));

    // Nothing left after filtering → no edit at all.
    assert!(no_request(&s.paste_text("\u{7}\u{1b}".into())));
}

#[test]
fn paste_text_dropped_while_another_surface_owns_the_keyboard() {
    // Search mode: the query input is the shell's editor; the buffer must not see the paste.
    let mut s = session();
    let _ = key(&mut s, '/');
    assert!(no_request(&s.paste_text("query".into())));

    // An open picker likewise (its query is shell-owned too).
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let _ = key(&mut s, 'f');
    assert!(s.picker.is_some(), "Space f opens the Files picker");
    assert!(no_request(&s.paste_text("clip".into())));
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
fn jumplist_capture_swaps_the_picker_for_the_jumplist() {
    // Picker Ctrl-j sends `jumplist/capture` with the highlighted item (the source picker stays
    // open while it's in flight); the Ok(Some) response swaps it for the Jumplist picker framed
    // on the captured row (`center_on` its 0-based index) — the capture is visible, and Enter
    // there jumps via the ordinary select path.
    use aether_client::update::Event;
    use aether_protocol::jumplist::JumplistCaptureResult;
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    // Land one row so Ctrl-j has a highlighted item to send.
    {
        let p = s.picker.as_mut().unwrap();
        p.loaded = true;
        p.ticking = false; // search settled — capture refuses a partial (still-filling) list
        p.items = vec![PickerItem::GrepHit {
            path_index: 0,
            relative_path: "a.rs".into(),
            line: 3,
            col: 1,
            preview: "let x = 1;".into(),
            match_indices: vec![],
        }];
    }

    let fx = ctrl(&mut s, 'j');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "jumplist/capture");
    assert_eq!(params["kind"], "grep");
    assert_eq!(params["item"]["kind"], "grep_hit");
    assert_eq!(
        s.picker.as_ref().map(|p| p.kind),
        Some(PickerKind::Grep),
        "the source picker stays open while the capture is in flight"
    );

    let fx = s.on_event(Event::JumplistCaptured(
        Ok(Some(JumplistCaptureResult { total: 1, index: 0 })),
        PickerKind::Grep,
    ));
    assert_eq!(
        s.picker.as_ref().map(|p| p.kind),
        Some(PickerKind::Jumplist),
        "the capture lands as the Jumplist picker"
    );
    // A confirmation toast makes the swap read as an action (the pickers look alike); keyed on
    // "jumplist" so repeated captures coalesce, and singular for a one-entry list.
    assert_eq!(
        first_toast(&fx),
        Some((
            "Captured 1 result to the jumplist".into(),
            Some("jumplist".into())
        )),
    );
    // The swap re-views the Jumplist picker framed on the captured row.
    let view =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { method, params, .. } if *method == "picker/view" => {
                    Some(params.clone())
                }
                _ => None,
            })
            .expect("the swap opens the Jumplist picker");
    assert_eq!(view["kind"], "jumplist");
    assert_eq!(view["center_on"]["kind"], "jumplist_entry");
    assert_eq!(view["center_on"]["index"], 0);
}

#[test]
fn jumplist_step_adopts_the_opened_entry() {
    // A `jumplist/step` composite (`]`/`[`) returns the target entry already opened; the client
    // adopts it exactly like a picker selection — cross-buffer targets switch the session's
    // buffer, and the status counter rides the opened cursor's `jumplist_position` stamp rather
    // than any client-held state.
    use aether_client::update::Event;
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::cursor::{Direction, JumplistPosition};
    use aether_protocol::jumplist::{JumplistStepResult, JumplistStepTarget};
    use aether_protocol::LogicalPosition;

    let mut s = session();
    let cursor = aether_protocol::cursor::CursorState {
        position: LogicalPosition { line: 4, col: 9 },
        anchor: LogicalPosition { line: 4, col: 2 },
        jumplist_position: Some(JumplistPosition {
            current: 3,
            total: 17,
        }),
        ..Default::default()
    };
    let open = BufferOpenResult {
        buffer_id: 7,
        language: None,
        line_count: 20,
        byte_count: 100,
        revision: 0,
        saved_revision: 0,
        path: Some("/proj/b.rs".into()),
        scratch_number: None,
        cursor,
        scroll: None,
        lsp_server: None,
        transient: true,
    };
    let _ = s.on_event(Event::JumplistStepped(
        Ok(JumplistStepResult::Moved(Box::new(JumplistStepTarget {
            path: "/b.rs".into(),
            position: LogicalPosition { line: 4, col: 9 },
            anchor: Some(LogicalPosition { line: 4, col: 2 }),
            index: 3,
            total: 17,
            opened: Some(open),
        }))),
        Direction::Forward,
        aether_protocol::jumplist::JumplistStepScope::Full,
    ));

    assert_eq!(
        s.buffer.buffer_id, 7,
        "the step switched to the entry's buffer"
    );
    assert_eq!(
        s.buffer.cursor.jumplist_position,
        Some(JumplistPosition {
            current: 3,
            total: 17
        }),
        "the status counter rides the opened cursor's stamp"
    );
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
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
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
        expanded_run: None,
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
        path_filterable: false,
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
        expanded_run: None,
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
        path_filterable: false,
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
    s.open_picker(PickerKind::Files, None, None, false, None);
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
    s.open_picker(PickerKind::Files, None, None, false, None);
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
    s.open_picker(PickerKind::Files, None, None, false, None);
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
    s.open_picker(PickerKind::Files, None, None, false, None);
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
    s.open_picker(PickerKind::Files, None, None, false, None);
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

/// Accepting a row resolves it **before** closing the picker. `picker/hide` releases the picker's
/// state server-side, and requests go out in enqueue order, so a `picker/select` behind the close
/// would find no candidate set and come back `invalid params` instead of jumping. Ordering only,
/// but it's the whole contract — kind-independent, checked here on the changes picker.
#[test]
fn accepting_a_row_selects_before_it_closes() {
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::GitChanges, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![PickerItem::GitChange {
            path_index: 0,
            relative_path: "src/main.rs".into(),
            hunk_index: 0,
            line: 12,
            stage: Default::default(),
            added: 1,
            removed: 0,
            preview: "let x = 1;".into(),
            match_indices: Vec::new(),
        }];
        p.selected = 0;
    }

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let methods: Vec<&str> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, .. } => Some(*method),
                _ => None,
            })
            .collect();
    let select = methods.iter().position(|m| *m == "picker/select");
    let hide = methods.iter().position(|m| *m == "picker/hide");
    assert!(
        select.is_some() && hide.is_some(),
        "accept both selects and closes, got {methods:?}"
    );
    assert!(
        select < hide,
        "select must reach the server while the picker still has candidates, got {methods:?}"
    );
}

/// Trashing a file from the Files picker re-lists it *without* throwing away what you'd typed.
/// Files' candidates come from the workspace index, so the list has to be re-bound server-side —
/// but via a `Keep` re-view, not a fresh open, which would also wipe the query and chips as a side
/// effect of a delete. The Explorer branch of the same handler keeps its query for the same reason.
#[test]
fn trashing_from_the_files_picker_relists_without_clearing_the_query() {
    use aether_client::update::Event;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    let _ = s.picker_set_query("main".into());
    {
        let p = s.picker.as_mut().unwrap();
        p.selected = 7;
    }

    let fx = s.on_event(Event::PathDeleted {
        noun: "file",
        result: Ok(serde_json::from_value(json!({})).unwrap()),
    });

    let view = find_request(&fx, "picker/view").expect("re-lists via picker/view");
    assert_eq!(
        view["reset"],
        json!("keep"),
        "a re-view re-binds the index without wiping the query"
    );
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.query, "main", "the query you typed survives the delete");
    assert_eq!(
        p.selected, 0,
        "but the highlight resets — its row just went"
    );
}

#[test]
fn every_picker_open_resets_the_scroll() {
    // No picker resumes any more, so every open starts the list at the top. The kinds that want to
    // land elsewhere (the changes pickers, the jumplist) centre via the `effective_center_on` echo,
    // which arrives with the response and reveals *after* this — the order Buffers and the Explorer
    // have always opened in.
    use aether_protocol::picker::PickerKind;

    for kind in [
        PickerKind::Files,
        PickerKind::Grep,
        PickerKind::GitChanges,
        PickerKind::GitChangesFile,
        PickerKind::Buffers,
    ] {
        let mut s = session();
        let fx = s.open_picker(kind, None, None, false, None);
        assert!(
            fx.0.iter().any(|e| matches!(e, Effect::PickerScrollReset)),
            "{kind:?} opens fresh, so its scroll resets to the top"
        );
    }
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
fn explorer_alt_l_applies_common_prefix_completion() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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
    // Alt-l — the accept/advance gesture everywhere — extends the query by the shared remainder
    // (`her-`), then re-queries. Tab no longer completes anywhere in the app; it traverses fields.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "aether-");
    let requery = find_request(&fx, "picker/query").expect("alt-l re-queries");
    assert_eq!(requery["query"], json!("aether-"));

    // With no ghost left to adopt, the same chord falls through to descending into the selection —
    // which re-lists the new directory from an empty query.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    let requery = find_request(&fx, "picker/query").expect("descending re-lists");
    assert_eq!(
        requery["query"],
        json!(""),
        "nothing left to complete — Alt-l descended instead of extending the query",
    );
    assert_eq!(s.picker.as_ref().unwrap().query, "");
}

#[test]
fn explorer_alt_backspace_unwinds_breadcrumb_before_chips() {
    use aether_client::chips::ChipValue;
    use aether_client::keymap::Mods;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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

/// The changes pickers open fresh like everything else — their query and chips don't outlive an
/// open — and land on the cursor's hunk instead of a saved highlight. `Space Alt-c` also re-points
/// at the active buffer on every open, so it carries `buffer_id` too.
#[test]
fn changes_pickers_open_fresh_and_centre_on_the_cursor() {
    use aether_protocol::picker::PickerKind;
    for (kind, wire) in [
        (PickerKind::GitChanges, "git_changes"),
        (PickerKind::GitChangesFile, "git_changes_file"),
    ] {
        let mut s = session();
        let fx = s.open_picker(kind, None, None, false, None);
        let view = find_request(&fx, "picker/view").expect("opens via picker/view");
        assert_eq!(view["kind"], json!(wire));
        assert_eq!(
            view["reset"],
            json!("all"),
            "{kind:?} starts over — no resumed query or chips"
        );
        assert_eq!(
            view["center_on_cursor"],
            json!(s.buffer.buffer_id),
            "{kind:?} frames the hunk nearest the live cursor instead"
        );
    }
}

#[test]
fn explorer_delete_confirms_then_trashes_and_relists() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
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
fn chooser_esc_over_placeholder_exits_and_keeps_the_picker() {
    use aether_protocol::picker::PickerKind;

    // The mandatory chooser: the Workspaces picker over a placeholder session (a no-args start,
    // or after `ToChooser`). Esc exits — there's nothing behind the picker to fall back to — and
    // the picker stays open (shells that can't exit, like the web, no-op `Exit` and keep it up).
    let mut s = session();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(quits(&fx), "Esc in the mandatory chooser exits");
    assert!(
        s.picker.is_some(),
        "the picker stays open (web keeps rendering it)"
    );
    assert!(
        !fx.0
            .iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if *method == "picker/hide")),
        "no picker/hide — the chooser wasn't dismissed"
    );

    // The same picker in a real session is an ordinary overlay: Esc closes it, no exit.
    let mut s = hint_session();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(!quits(&fx), "in-session Esc doesn't exit");
    assert!(s.picker.is_none(), "in-session Esc dismisses the picker");
}

#[test]
fn search_option_toggle_follows_its_hint() {
    // Search-mode bindings resolve in `on_search_key`, not `run_action` — the observation there
    // must fire, or the option hints (Alt-c/w/e) never follow and the corner sticks.
    let mut s = hint_session();
    adopt_hints(&mut s);
    let _ = key(&mut s, '/');
    assert_eq!(s.mode, aether_client::session::Mode::Search);
    let _ = s.on_hint_tick(1_000_000_004_000);
    let v = s.hint_view().expect("a search option hint displays");
    let (chord, id) = match v.keys {
        "Alt-c" => ('c', "search-case"),
        "Alt-w" => ('w', "search-word"),
        "Alt-e" => ('e', "search-regex"),
        other => panic!("unexpected search hint {other}"),
    };
    let keys_before = v.keys;

    // Fire the displayed hint's own chord: it must record a follow and rotate out.
    let fx = s.on_key(KeyCode::Char(chord), Mods::ALT, None, ROWS);
    assert!(
        hint_records(&fx)
            .iter()
            .any(|(i, ev)| i == id && ev == "followed"),
        "the option toggle follows its hint: {:?}",
        hint_records(&fx)
    );
    assert!(
        s.hint_view().map(|v2| v2.keys) != Some(keys_before),
        "the followed hint rotates out of the corner"
    );
}

#[test]
fn picker_esc_records_the_dismiss_gesture() {
    use aether_protocol::picker::PickerKind;

    // Esc in an in-session picker demonstrates the picker-dismiss binding: the close records a
    // `used` (or `followed`, if the hint happened to be on screen) for the hint's learning.
    let mut s = hint_session();
    adopt_hints(&mut s);
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(s.picker.is_none(), "Esc closes the picker");
    assert!(
        hint_records(&fx)
            .iter()
            .any(|(id, _)| id == "picker-dismiss"),
        "Esc-close records the dismiss demonstration: {:?}",
        hint_records(&fx)
    );

    // The mandatory chooser's Esc exits without closing — deliberately NOT a dismiss
    // demonstration (nothing closed, and the hint is suppressed there anyway).
    let mut s = session();
    adopt_hints(&mut s);
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(quits(&fx));
    assert!(
        !hint_records(&fx)
            .iter()
            .any(|(id, _)| id == "picker-dismiss"),
        "the exit gesture must not count as a picker dismissal"
    );
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
        }
    }

    let mut s = session();
    // The active editor buffer is id 0 (placeholder default).
    let _ = s.open_picker(PickerKind::Buffers, None, None, false, None);
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
        }
    }

    let mut s = session();
    // The active editor buffer is id 0 (placeholder default).
    let _ = s.open_picker(PickerKind::Buffers, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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
    // Creating a file is a terminal pick: the explorer closes rather than lingering over the
    // freshly-opened buffer (`Event::Switched` deliberately doesn't tear pickers down).
    assert!(s.picker.is_none(), "the explorer closes on file create");
}

#[test]
fn explorer_create_with_trailing_slash_makes_a_directory() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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
    // Unlike file create, dir create keeps exploring — the result steps into the new directory.
    assert!(s.picker.is_some(), "the explorer stays open on dir create");
}

/// Selecting the synthetic "+ Create …" row (the affordance that replaced the old Ctrl-n) runs the
/// create: a click on its absolute index routes through `picker_accept` → create-on-save.
#[test]
fn selecting_the_create_row_creates_the_file() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};

    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
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
            expanded_run: None,
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

/// Insert-mode `Tab` asks the server for an indent step rather than sending a literal `\t` of its
/// own — the buffer's indent style lives server-side, so the client can't compute the whitespace.
#[test]
fn insert_tab_requests_an_indent_step() {
    let mut s = session();
    key(&mut s, 'i');
    assert_eq!(s.mode, aether_client::session::Mode::Insert);

    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "input/tab");
    // No text on the wire: the payload is just the buffer.
    assert_eq!(params.get("text"), None);
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
fn space_n_shows_diagnostic_at_cursor() {
    // Space n → diagnostic at cursor (moved off Space j, which now opens the jumplist picker).
    // With no diagnostics loaded it reports "none" via a toast (resolved locally — no RPC),
    // which still proves the chord reaches `show_diagnostic`.
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('n'), Mods::NONE, Some("n".to_string()), ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Info,
                ..
            }
        )),
        "Space n with no diagnostics toasts an info message"
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
fn font_size_settings_step_and_persist_independently() {
    use aether_client::keymap::{KeyCode, Mods};
    use aether_client::session::AppSettingId;
    use aether_client::update::Event;
    use aether_protocol::settings::AppSettings;
    use aether_protocol::viewport::WrapMode;

    // Persisted font sizes are adopted into the session (render-only, like ligatures — no effects).
    let mut s = session();
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        wrap: WrapMode::Soft,
        ligatures: true,
        buffer_font_size: 16,
        ui_font_size: 12,
        ..AppSettings::default()
    })));
    assert_eq!(s.buffer_font_size, 16, "persisted buffer size is adopted");
    assert_eq!(s.ui_font_size, 12, "persisted UI size is adopted");
    assert!(
        fx.0.is_empty(),
        "font sizes are render-only — no reflow effect"
    );

    // Both rows sit in the app-settings overlay. Activating one (Enter/Space/click) steps it to the
    // next preset and persists via settings/set — and leaves the other size alone.
    s.open_app_settings();
    let row_index = |s: &aether_client::session::Session, want: AppSettingId| {
        s.app_setting_rows()
            .iter()
            .position(|r| r.id == want)
            .unwrap_or_else(|| panic!("a {want:?} row"))
    };
    let buffer_row = row_index(&s, AppSettingId::BufferFontSize);
    let fx = s.app_settings_toggle(buffer_row);
    assert_eq!(s.buffer_font_size, 18, "16 → next preset 18");
    assert_eq!(s.ui_font_size, 12, "the UI size is untouched");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["buffer_font_size"], json!(18));
    assert_eq!(params["ui_font_size"], json!(12));

    // Left steps down to the previous preset (no wrap), also persisting.
    let fx = s.on_key(KeyCode::Left, Mods::NONE, None, ROWS);
    assert_eq!(s.buffer_font_size, 16, "Left steps down a preset");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["buffer_font_size"], json!(16));

    // The UI row is its own stepper over the same presets, and moves only the UI size.
    let ui_row = row_index(&s, AppSettingId::UiFontSize);
    let fx = s.app_settings_toggle(ui_row);
    assert_eq!(s.ui_font_size, 13, "12 → next preset 13");
    assert_eq!(s.buffer_font_size, 16, "the buffer size is untouched");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["ui_font_size"], json!(13));

    let fx = s.on_key(KeyCode::Right, Mods::NONE, None, ROWS);
    assert_eq!(s.ui_font_size, 14, "Right steps up a preset");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["ui_font_size"], json!(14));
    assert_eq!(params["buffer_font_size"], json!(16));
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
    let methods: Vec<&str> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, .. } => Some(*method),
                _ => None,
            })
            .collect();
    // The connect sequence fetches the app settings, the hint snapshot and the workspace's
    // input-history lists together.
    assert_eq!(
        methods,
        vec!["settings/get", "hints/state", "history/state"]
    );
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
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
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
        expanded_run: None,
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
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
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
            expanded_run: None,
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
fn create_from_chooser_survives_hint_ticks_mid_flight() {
    use aether_protocol::picker::PickerKind;

    // The full boot-chooser create flow with hints on, a hint tick injected at every await
    // point — the TUI's 2s tick can land anywhere in the round-trip, and the in-flight session
    // is briefly a placeholder with no picker. No tick may bounce it to the chooser or exit.
    let mut s = session();
    adopt_hints(&mut s);
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
    s.picker.as_mut().unwrap().loaded = true;
    let _ = s.on_hint_tick(1_000_000_002_000); // the create hint displays
    let _ = s.picker_set_query("e2e-scratch".into());
    assert_eq!(s.picker.as_ref().unwrap().create_row_index(), Some(0));

    let no_bounce = |fx: &Effects, at: &str| {
        assert!(
            !fx.0
                .iter()
                .any(|e| matches!(e, Effect::ToChooser | Effect::Exit)),
            "no chooser bounce/exit {at}"
        );
    };

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    no_bounce(&fx, "on accept");
    let create_token = fx
        .0
        .iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "workspace/create" => Some(*token),
            _ => None,
        })
        .expect("workspace/create fired");

    // Tick while the create is in flight: picker closed, still a placeholder.
    let fx = s.on_hint_tick(1_000_000_004_000);
    no_bounce(&fx, "while create in flight");

    let fx = s.on_rpc_result(
        create_token,
        Ok(json!({
            "workspace": { "name": "e2e-scratch", "paths": [] },
            "server_started_at": 1,
        })),
    );
    no_bounce(&fx, "on create result");
    let open_token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, method, .. } if *method == "buffer/open" => Some(*token),
                _ => None,
            })
            .expect("scratch buffer/open fired");

    // Tick between the create result and the scratch landing: workspace set, buffer still 0.
    let fx = s.on_hint_tick(1_000_000_006_000);
    no_bounce(&fx, "while scratch open in flight");

    let fx = s.on_rpc_result(
        open_token,
        Ok(json!({
            "buffer_id": 1,
            "language": null,
            "line_count": 1,
            "byte_count": 0,
            "revision": 0,
            "saved_revision": 0,
            "path": null,
            "scratch_number": 1,
            "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
        })),
    );
    no_bounce(&fx, "on scratch adoption");
    assert!(!s.is_placeholder(), "the scratch landed");
    assert_eq!(s.workspace, "e2e-scratch");
    assert!(s.workspace_settings.is_some(), "settings overlay is up");

    // And a settled tick after landing.
    let fx = s.on_hint_tick(1_000_000_008_000);
    no_bounce(&fx, "after landing");
    assert!(
        s.workspace_settings.is_some(),
        "the settings overlay survives the tick"
    );
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
            projects: Vec::new(),
        },
        last_buffer_id: None,
        opened: None,
        server_started_at: 0,
    })));
    assert_eq!(s.workspace, "fresh");
    // Rather than leave the previous workspace's buffer behind, a scratch is opened (a `buffer/open`
    // with no buffer_id/path) so the user lands in some editor in the new workspace. The new
    // workspace's (empty) input-history lists are fetched alongside — the old ones were another
    // workspace's.
    let methods: Vec<&str> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, .. } => Some(*method),
                _ => None,
            })
            .collect();
    assert_eq!(
        methods,
        vec!["history/state", "buffer/open"],
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
    // Open focuses the name field; Tab down to the add-root input (past the single root).
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
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
        projects: Vec::new(),
    })));
    assert_eq!(s.workspace_paths, vec!["/a".to_string(), "/b".to_string()]);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.roots.len(), 2);
    assert!(
        ps.add.text.is_empty(),
        "the input clears after a successful add"
    );
}

/// The workspace-symbols picker distinguishes "your query matched nothing" from "nothing here can
/// answer" — the second is a config problem the user resolves elsewhere, so saying "No symbols
/// found" would send them looking in the wrong place.
#[test]
fn workspace_symbols_empty_note_names_the_missing_projects() {
    use aether_protocol::picker::PickerKind;
    use aether_protocol::workspace::WorkspaceProject;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];

    // No projects declared: the note points at where you'd declare one. (A fresh picker is
    // `ticking` until the first push settles it; the note is about the settled state.)
    let _ = s.open_picker(PickerKind::WorkspaceSymbols, None, None, false, None);
    s.picker.as_mut().unwrap().ticking = false;
    let note = s.picker.as_ref().unwrap().empty_note();
    assert!(
        note.is_some_and(|n| n.contains("No projects")),
        "expected a no-projects note, got {note:?}",
    );

    // With one declared, an empty result really is "no matches".
    s.workspace_projects = vec![WorkspaceProject {
        path_index: 0,
        relative_path: "Cargo.toml".into(),
        language: "rust".into(),
        error: None,
    }];
    let _ = s.open_picker(PickerKind::WorkspaceSymbols, None, None, false, None);
    s.picker.as_mut().unwrap().ticking = false;
    let note = s.picker.as_ref().unwrap().empty_note();
    assert!(
        note.is_some_and(|n| n.contains("No symbols")),
        "expected a no-matches note, got {note:?}",
    );
}

/// Boot seeds a session straight from the activation result, without going through
/// `sync_workspace_info` — so `Session::new` has to carry *everything* the server sent. Regression
/// test: it dropped the declared projects, and a freshly launched client showed an empty Projects
/// section (with no later workspace event to fix it) even though the server knew about them.
#[test]
fn a_booted_session_carries_the_workspace_declared_projects() {
    use aether_client::session::BufferInfo;
    use aether_protocol::workspace::{WorkspaceInfo, WorkspaceProject};

    let mut s = Session::new(
        WorkspaceInfo {
            name: "aether".into(),
            paths: vec!["/a".into()],
            projects: vec![WorkspaceProject {
                path_index: 0,
                relative_path: "Cargo.toml".into(),
                language: "rust".into(),
                error: None,
            }],
        },
        BufferInfo {
            buffer_id: 1,
            label: "a.rs".into(),
            path: Some("/a/a.rs".into()),
            language: None,
            revision: 0,
            saved_revision: 0,
            cursor: aether_protocol::cursor::CursorState::default(),
            scroll: None,
            transient: false,
            lsp_server: None,
        },
    );
    assert_eq!(s.workspace_projects.len(), 1);

    // ...and the settings overlay shows them without needing a workspace event first.
    s.open_workspace_settings();
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.projects.len(), 1);
    assert_eq!(ps.projects[0].relative_path, "Cargo.toml");
}

/// Tab/Shift-Tab traverse the dialog's fields, and step *through* the add-project row's two
/// segments on the way — the form convention, and why the editor no longer claims Tab.
#[test]
fn settings_tab_traverses_fields_including_the_editor_segments() {
    use aether_client::chips::ChipEditorField;
    use aether_client::session::SettingsRow;

    let mut s = session();
    s.workspace = "aether".into();
    // Multi-root, so the add-project row has a root segment as well as a path one. Labels that are
    // longer than a one-character prefix, so "typed" and "adopted" are distinguishable.
    s.workspace_paths = vec!["/alpha".into(), "/beta".into()];
    s.open_workspace_settings();

    // name → root(0) → root(1) → add-root → add-project.
    for expected in [
        SettingsRow::Root(0),
        SettingsRow::Root(1),
        SettingsRow::AddRoot,
        SettingsRow::AddProject,
    ] {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
        assert_eq!(s.workspace_settings.as_ref().unwrap().row(), expected);
    }

    // Inside the editor, Tab steps root → path rather than leaving the row — and *without*
    // adopting the root ghost, which is Alt-l's job. A partly-typed filter stays as typed.
    let _ = s.workspace_settings_set_add_project_root("be".into());
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.field, ChipEditorField::Root);
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert_eq!(ps.add_project.field, ChipEditorField::Path);
    assert_eq!(
        ps.add_project.root_filter.text, "be",
        "Tab traverses; it does not complete the root to its full label",
    );

    // Alt-l is what adopts the ghost — same traversal, but the filter becomes the full label.
    s.on_key(KeyCode::BackTab, Mods::NONE, None, ROWS);
    s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.root_filter.text, "beta");
    assert_eq!(ps.add_project.field, ChipEditorField::Path);

    // ...and Shift-Tab walks back out the same way.
    s.on_key(KeyCode::BackTab, Mods::NONE, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.field, ChipEditorField::Root);
    s.on_key(KeyCode::BackTab, Mods::NONE, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::AddRoot,
    );

    // Wrapping backwards into the row lands on its *last* segment, so reverse traversal retraces
    // the forward path rather than skipping a field. From add-root (index 3) that's four steps:
    // root(1), root(0), name, then round to add-project.
    for _ in 0..4 {
        s.on_key(KeyCode::BackTab, Mods::NONE, None, ROWS);
    }
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert_eq!(ps.add_project.field, ChipEditorField::Path);
}

/// The bug this replaced: on a multi-root workspace, Alt-j on the add-project row cycled root
/// candidates *and* was the only way out of the row, so it got stuck. Now the editor keeps Alt-j/k
/// for its candidates and Tab is how you leave.
#[test]
fn settings_alt_j_cycles_candidates_without_leaving_the_editor() {
    use aether_client::session::SettingsRow;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    s.open_workspace_settings();
    for _ in 0..4 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::AddProject,
    );

    let before = s
        .workspace_settings
        .as_ref()
        .unwrap()
        .add_project
        .root_selected;
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject, "still on the editor row");
    assert_ne!(
        ps.add_project.root_selected, before,
        "Alt-j cycled the root candidates rather than moving the row",
    );
}

/// Alt-j/k no longer traverse the dialog at all — they belong to the focused field. A key that
/// moved rows *except* when the field wanted it was the ambiguity Tab was introduced to remove.
#[test]
fn settings_alt_j_does_not_traverse_fields() {
    use aether_client::session::SettingsRow;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Name,
        "Alt-j is the focused field's key, not the dialog's",
    );

    // The arrows are the non-chord alternative to Tab for people who want one.
    s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Root(0),
    );
    s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Name,
    );
}

/// A ghost belongs to the segment being edited. Left showing while another has focus it reads as
/// part of the value — a path of `databricks/` trailed by a `.databricks/` suggestion looks like the
/// path you're about to commit.
#[test]
fn moving_off_a_segment_drops_its_ghost() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    // A listing gives the path segment something to suggest.
    let _ = s.workspace_settings_set_add_project("dat".into());
    if let Some(st) = s.workspace_settings.as_mut() {
        st.add_project
            .set_dir_listing(vec![aether_protocol::directory::DirectoryEntry {
                name: "databricks".into(),
                is_dir: true,
            }]);
    }
    assert!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_project
            .path_ghost()
            .is_some(),
        "the focused path segment suggests",
    );

    // Tab into the language segment: the path's suggestion is no longer being offered.
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert!(ps.on_add_project_language);
    assert!(
        ps.language_ghost().is_none(),
        "an empty language field ghosts nothing — it would read as a default",
    );
}

/// The language segment only accepts a language we have a server for. That's the point of it: a
/// typo silently sent to the server comes back as an error you can't act on from the dialog.
#[test]
fn settings_language_segment_only_accepts_supported_languages() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // name → root → add-root → add-project(path) → add-project(language).
    for _ in 0..4 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    assert!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .on_add_project_language
    );
    let _ = s.workspace_settings_set_add_project("databricks".into());

    // A prefix resolves to the one language it names, and Alt-l settles the text on it.
    let _ = s.workspace_settings_set_add_project_language("pyth".into());
    let ps = s.workspace_settings.as_ref().unwrap();
    assert!(!ps.language_invalid());
    assert_eq!(ps.chosen_language().as_deref(), Some("python"));
    assert_eq!(ps.language_ghost().as_deref(), Some("on"));
    s.on_key(KeyCode::Char('l'), Mods::ALT, None, ROWS);
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_project_language
            .text,
        "python"
    );

    // Nonsense is refused rather than sent.
    let _ = s.workspace_settings_set_add_project_language("cobol".into());
    assert!(s.workspace_settings.as_ref().unwrap().language_invalid());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        find_request(&fx, "workspace/add_project").is_none(),
        "an unsupported language must not reach the server",
    );
    assert!(s
        .workspace_settings
        .as_ref()
        .unwrap()
        .error
        .as_deref()
        .is_some_and(|e| e.contains("cobol")));

    // Empty means "infer", which is the common case and must still commit.
    let _ = s.workspace_settings_set_add_project_language(String::new());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let add = find_request(&fx, "workspace/add_project").expect("commits with no language");
    assert!(
        add.get("language").is_none(),
        "absent, so the server infers"
    );
}

/// Token + params of the (first) `method` request in `fx` — for results that must be fed back.
fn request_with_token<'a>(fx: &'a Effects, method: &str) -> Option<(u64, &'a serde_json::Value)> {
    fx.0.iter().find_map(|e| match e {
        Effect::Request {
            token,
            method: m,
            params,
        } if *m == method => Some((*token, params)),
        _ => None,
    })
}

/// Typing a directory into the add-project row asks the server what language declaring it would
/// pin (`workspace/infer_language`); the answer pre-fills the untouched language segment and
/// commits explicitly, like a typed language would.
#[test]
fn typing_a_project_path_infers_its_language() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();

    let fx = s.workspace_settings_set_add_project("databricks/".into());
    let (token, params) =
        request_with_token(&fx, "workspace/infer_language").expect("asks the server as you type");
    assert_eq!(params["workspace"], json!("aether"));
    assert_eq!(params["path_index"], json!(0));
    assert_eq!(params["relative_path"], json!("databricks/"));

    let _ = s.on_rpc_result(token, Ok(json!({"language": "python"})));
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project_language.text, "python");
    assert!(ps.language_inferred);

    // The suggestion commits as an explicit language, exactly as if it had been typed.
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS); // name → root → add-root → add-project
    }
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let add = find_request(&fx, "workspace/add_project").expect("commits");
    assert_eq!(add["language"], json!("python"));
}

/// The reply is keyed to the (root, path) pair it was asked about — one that lands after the
/// editor moved on must not fill the field for the wrong directory.
#[test]
fn a_stale_inference_reply_is_dropped() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();

    let fx = s.workspace_settings_set_add_project("databricks/".into());
    let (stale, _) = request_with_token(&fx, "workspace/infer_language").unwrap();
    let fx = s.workspace_settings_set_add_project("web/".into());
    let (current, _) = request_with_token(&fx, "workspace/infer_language").unwrap();

    // The first directory's answer arrives late: dropped.
    let _ = s.on_rpc_result(stale, Ok(json!({"language": "python"})));
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_project_language
            .text,
        ""
    );
    // The current directory's answer fills.
    let _ = s.on_rpc_result(current, Ok(json!({"language": "typescript"})));
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_project_language
            .text,
        "typescript"
    );
}

/// The field is the user's once they've typed in it: inference stops touching it. An *inferred*
/// value, by contrast, follows the path — replaced when the new directory infers differently,
/// cleared when it infers nothing (or the path empties).
#[test]
fn a_typed_language_beats_inference_but_an_inferred_one_follows_the_path() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();

    // Typed first, inferred later: the typed value stays.
    let fx = s.workspace_settings_set_add_project("databricks/".into());
    let (token, _) = request_with_token(&fx, "workspace/infer_language").unwrap();
    let _ = s.workspace_settings_set_add_project_language("go".into());
    let _ = s.on_rpc_result(token, Ok(json!({"language": "python"})));
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project_language.text, "go");
    assert!(!ps.language_inferred);

    // Inferred, then the path moves somewhere nothing infers for: the suggestion clears.
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    let fx = s.workspace_settings_set_add_project("pkg/".into());
    let (token, _) = request_with_token(&fx, "workspace/infer_language").unwrap();
    let _ = s.on_rpc_result(token, Ok(json!({"language": "rust"})));
    let fx = s.workspace_settings_set_add_project("plain/".into());
    let (token, _) = request_with_token(&fx, "workspace/infer_language").unwrap();
    let _ = s.on_rpc_result(token, Ok(json!({})));
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(
        ps.add_project_language.text, "",
        "nothing inferred any more"
    );
    assert!(!ps.language_inferred);

    // And emptying the path clears an inferred suggestion without waiting on the server.
    let fx = s.workspace_settings_set_add_project("pkg/".into());
    let (token, _) = request_with_token(&fx, "workspace/infer_language").unwrap();
    let _ = s.on_rpc_result(token, Ok(json!({"language": "rust"})));
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_project_language
            .text,
        "rust"
    );
    let _ = s.workspace_settings_set_add_project(String::new());
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project_language.text, "");
    assert!(!ps.language_inferred);
}

/// The overlay is two lists, each with a trailing input. This pins the whole index→row mapping,
/// which every shell's rendering and focus routing depends on.
#[test]
fn settings_selection_model_spans_both_lists() {
    use aether_client::session::SettingsRow;
    use aether_protocol::workspace::WorkspaceProject;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    s.workspace_projects = vec![
        WorkspaceProject {
            path_index: 0,
            relative_path: "Cargo.toml".into(),
            language: "rust".into(),
            error: None,
        },
        WorkspaceProject {
            path_index: 1,
            relative_path: "go.mod".into(),
            language: "go".into(),
            error: None,
        },
    ];
    s.open_workspace_settings();
    let ps = s.workspace_settings.as_ref().unwrap();

    // name, root, root, add-root, project, project, add-project
    assert_eq!(ps.row_count(), 7);
    let rows: Vec<SettingsRow> = (0..ps.row_count()).map(|i| ps.row_at(i)).collect();
    assert_eq!(
        rows,
        vec![
            SettingsRow::Name,
            SettingsRow::Root(0),
            SettingsRow::Root(1),
            SettingsRow::AddRoot,
            SettingsRow::Project(0),
            SettingsRow::Project(1),
            SettingsRow::AddProject,
        ]
    );
    assert_eq!(ps.input_index(), 3);
    assert_eq!(ps.add_project_index(), 6);
    // Past the end clamps onto the last row rather than panicking or wrapping.
    assert_eq!(ps.row_at(99), SettingsRow::AddProject);
}

/// With no projects declared the add-project row is still reachable — it's how you declare the
/// first one — so Alt-j must not stop at the add-root row the way it used to.
#[test]
fn settings_navigation_reaches_the_add_project_row() {
    use aether_client::session::SettingsRow;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // name → root → add-root → add-project.
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert!(ps.on_input(), "both add rows count as text inputs");

    // Tab off the path enters the row's trailing language segment, still on the same row...
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert!(ps.on_add_project_language);

    // ...and only Tab off *that* cycles round to the first field.
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Name,
    );
    // ...and Shift-Tab off the first wraps back to the last.
    s.on_key(KeyCode::BackTab, Mods::NONE, None, ROWS);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::AddProject,
    );
}

#[test]
fn settings_add_project_emits_request_and_its_result_updates_state() {
    use aether_client::update::Event;
    use aether_protocol::workspace::{WorkspaceInfo, WorkspaceProject};

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // name → root → add-root → add-project.
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    let _ = s.workspace_settings_set_add_project("Cargo.toml".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let add = find_request(&fx, "workspace/add_project").expect("workspace/add_project fired");
    assert_eq!(add["workspace"], json!("aether"));
    assert_eq!(add["path_index"], json!(0));
    assert_eq!(add["relative_path"], json!("Cargo.toml"));
    assert!(
        add.get("language").is_none(),
        "no language sent — the server infers it from the marker"
    );

    let _ = s.on_event(Event::WorkspaceProjectAdded(Ok(WorkspaceInfo {
        name: "aether".into(),
        paths: vec!["/a".into()],
        projects: vec![WorkspaceProject {
            path_index: 0,
            relative_path: "Cargo.toml".into(),
            language: "rust".into(),
            error: None,
        }],
    })));
    assert_eq!(s.workspace_projects.len(), 1);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.projects.len(), 1);
    assert!(
        ps.add_project.input.text.is_empty(),
        "the input clears after a successful add"
    );
}

/// Delete on a project row opens the shared confirm, and accepting it fires the remove — the same
/// two-step the root rows use, so a project can't vanish on a stray keypress.
#[test]
fn settings_delete_on_a_project_row_confirms_then_removes() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::workspace::WorkspaceProject;

    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.workspace_projects = vec![WorkspaceProject {
        path_index: 0,
        relative_path: "Cargo.toml".into(),
        language: "rust".into(),
        error: None,
    }];
    s.open_workspace_settings();
    // name → root → add-root → project(0).
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    }
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None, ROWS);
    assert!(
        find_request(&fx, "workspace/remove_project").is_none(),
        "delete confirms first, it doesn't fire straight away"
    );
    assert!(matches!(
        s.prompt,
        Some(Prompt::Confirm {
            kind: ConfirmKind::RemoveProject { .. },
            ..
        })
    ));

    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, None, ROWS);
    let req = find_request(&fx, "workspace/remove_project").expect("remove fired on accept");
    assert_eq!(req["path_index"], json!(0));
    assert_eq!(req["relative_path"], json!("Cargo.toml"));
}

#[test]
fn settings_rename_emits_request_and_its_result_updates_the_name() {
    use aether_client::update::Event;
    use aether_protocol::workspace::WorkspaceInfo;

    let mut s = session();
    s.workspace = "old".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();
    // The overlay opens focused on the name field.
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
        projects: Vec::new(),
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
    // Open focuses the name field (index 0); Tab down to the first root row (index 1).
    s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
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
            projects: Vec::new(),
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
    let fx = s.open_picker(PickerKind::DocumentSymbols, None, None, false, None);
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
    let _ = s.open_picker(PickerKind::DocumentSymbols, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.generation = 0;
        p.offset = 0;
    }
    let sym = |line: u32, name: &str| PickerItem::Symbol {
        path: "/a.rs".into(),
        display_path: String::new(),
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
            expanded_run: None,
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
    let _ = s.open_picker(PickerKind::DocumentSymbols, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.generation = 0;
        p.offset = 0; // the picker opened at the top
    }
    let sym = |line: u32, name: &str| PickerItem::Symbol {
        path: "/a.rs".into(),
        display_path: String::new(),
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
            expanded_run: None,
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
/// leaves the context. A session *launched* for the file (`ae /path`) tethers to it
/// (docs/tether.md), so the close quits, vim-like.
#[test]
fn ephemeral_last_buffer_close_when_launched_quits() {
    let mut s = session();
    s.workspace = "ephemeral/1".to_string();
    s.buffer.buffer_id = 7;
    s.tether = Some(7);

    let fx = s.close_buffer();
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(false),
        "no successor needed when closing the tether"
    );

    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a file-launched session quits when its tether closes"
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
    s.tether = None;

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

// ---- the tether (docs/tether.md) --------------------------------------------------------------

/// A quick-edit session in a *real* workspace (`ae file`, workspace inferred — the git-commit
/// case): `Space x` on the tethered buffer exits the client instead of switching to a successor.
#[test]
fn closing_the_tether_in_a_workspace_context_exits() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 7;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()), ROWS);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(
        params["open_next"],
        json!(false),
        "no successor: the close ends the client"
    );

    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(quits(&fx), "closing the tether exits, even mid-workspace");
}

/// Without a tether the same close switches to the server's successor — the pre-tether behavior
/// stays for ordinary sessions.
#[test]
fn closing_an_untethered_buffer_switches_to_the_successor() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 7;

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()), ROWS);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/close");
    assert_eq!(params["open_next"], json!(true), "adopt the MRU successor");
}

/// Another client closing the tether (the `buffer/closed` push) exits too — the contract the
/// future `ae --web file` waiter rides. It holds even when the client has switched to a
/// different buffer: the tether check runs before the current-buffer guard (and the server
/// pushes to all workspace clients, not just viewers).
#[test]
fn tether_closed_by_another_client_exits() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification};
    let push = || {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: "buffer/closed".into(),
            params: json!({ "buffer_id": 7 }),
        })
    };

    // Viewing the tether when it closes.
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 7;
    s.tether = Some(7);
    let fx = s.on_event(push());
    assert!(quits(&fx), "the tether closed out from under us — exit");

    // Browsing another buffer when the tether closes: still exit.
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 9;
    s.tether = Some(7);
    let fx = s.on_event(push());
    assert!(quits(&fx), "exit even while viewing something else");

    // No tether: a push for a background buffer is ignored.
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 9;
    let fx = s.on_event(push());
    assert!(!quits(&fx), "untethered clients ignore background closes");
}

/// Un-keeping the tethered buffer (`Space k`) releases the tether: the buffer demotes to an
/// ordinary transient and closing it no longer exits. One-way — a later re-keep is a plain keep,
/// not a re-arm.
#[test]
fn unkeep_releases_the_tether_one_way() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 7;
    s.tether = Some(7);

    // `Space k` on the (clean) tether: one set_transient request, demoting the buffer.
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/set_transient");
    assert_eq!(
        params["transient"],
        json!(true),
        "release demotes to transient"
    );
    assert_eq!(s.tether, Some(7), "released only once the server confirms");

    let fx = s.on_rpc_result(token, Ok(json!({ "transient": true })));
    assert_eq!(s.tether, None, "the tether is gone");
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })),
        "the release is announced"
    );

    // Re-keep (the transient flag itself rides a push; simulate it) — a plain keep, no re-arm.
    s.buffer.transient = true;
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/set_transient");
    assert_eq!(params["transient"], json!(false), "plain keep");
    assert_eq!(s.tether, None, "re-keeping does not re-arm the tether");

    // And closing now behaves like any ordinary buffer: successor, no exit.
    s.buffer.transient = false;
    let fx = s.close_buffer();
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["open_next"], json!(true));
}

/// Releasing a *dirty* tether is refused wholesale (the demotion would arm auto-close over
/// unsaved edits) — audibly, unlike the plain toggle's silent no-op, since the user asked for
/// something.
#[test]
fn unkeep_on_a_dirty_tether_refuses_with_a_warning() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.buffer.buffer_id = 7;
    s.buffer.revision = 3;
    s.buffer.saved_revision = 2;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()), ROWS);
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "no RPC — the release is refused"
    );
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Warning,
                ..
            }
        )),
        "the refusal is surfaced"
    );
    assert_eq!(s.tether, Some(7), "the tether stays armed");
}

/// `Space Alt-x` (save-and-close) on the tether: save first, then close, then exit — each step
/// deferred until the previous one lands, mirroring `Space Alt-q`.
#[test]
fn space_alt_x_saves_closes_and_exits_the_tethered_session() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.workspace_paths = vec!["/p".into()];
    s.buffer.buffer_id = 7;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None, ROWS);
    let params = find_request(&fx, "buffer/save").expect("Space Alt-x saves first");
    assert_eq!(params["overwrite"], json!(false));
    assert!(!quits(&fx), "no exit before the save lands");
    let token = save_token(&fx);

    // Save lands → the close fires (tether style: no successor). Still no exit.
    let fx = s.on_rpc_result(token, Ok(json!({ "saved_at_unix_ms": 0, "revision": 4 })));
    let close_token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, method, .. } if *method == "buffer/close" => Some(*token),
                _ => None,
            })
            .expect("the landed save closes the buffer");
    assert!(!quits(&fx), "no exit before the close lands");

    // Close lands → exit.
    let fx = s.on_rpc_result(close_token, Ok(json!({})));
    assert!(quits(&fx), "save-close-exit completed");
}

/// `Space Alt-x` without a tether is still save-and-close — it just lands on the successor
/// instead of exiting.
#[test]
fn space_alt_x_untethered_closes_to_the_successor() {
    let mut s = session();
    s.workspace = "proj".to_string();
    s.workspace_paths = vec!["/p".into()];
    s.buffer.buffer_id = 7;

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None, ROWS);
    let token = save_token(&fx);

    let fx = s.on_rpc_result(token, Ok(json!({ "saved_at_unix_ms": 0, "revision": 4 })));
    let close = fx.0.iter().find_map(|e| match e {
        Effect::Request { method, params, .. } if *method == "buffer/close" => Some(params.clone()),
        _ => None,
    });
    let params = close.expect("the landed save closes the buffer");
    assert_eq!(params["open_next"], json!(true), "ordinary successor close");
    assert!(!quits(&fx));
}

/// Buffer ids don't survive a daemon restart: reconnecting onto the same file remaps the tether
/// to the reopened buffer's id; reconnecting onto anything else drops it (a stale id could match
/// an unrelated new buffer and exit under the user).
#[test]
fn daemon_restart_remaps_the_tether_on_the_same_file_and_drops_it_otherwise() {
    use aether_client::update::Event;
    use aether_protocol::buffer::BufferOpenResult;
    use aether_protocol::workspace::WorkspaceInfo;

    let reopen = |path: &str, id: u64| -> BufferOpenResult {
        serde_json::from_value(json!({
            "buffer_id": id,
            "language": null,
            "line_count": 1,
            "byte_count": 0,
            "revision": 0,
            "saved_revision": 0,
            "path": path,
        }))
        .unwrap()
    };
    let workspace = || WorkspaceInfo {
        name: "proj".into(),
        paths: vec!["/p".into()],
        projects: vec![],
    };

    // Same file after a restart: the tether follows the new id.
    let mut s = session();
    s.buffer.buffer_id = 7;
    s.buffer.path = Some("/p/f.txt".into());
    s.tether = Some(7);
    let _ = s.on_event(Event::ConnectionLost);
    let _ = s.on_event(Event::Reestablished {
        workspace: workspace(),
        open: reopen("/p/f.txt", 9),
        restarted: true,
    });
    assert_eq!(s.tether, Some(9), "remapped onto the reopened buffer");

    // Different landing buffer after a restart: the tether is dropped, not left stale.
    let mut s = session();
    s.buffer.buffer_id = 7;
    s.buffer.path = Some("/p/f.txt".into());
    s.tether = Some(7);
    let _ = s.on_event(Event::ConnectionLost);
    let _ = s.on_event(Event::Reestablished {
        workspace: workspace(),
        open: reopen("/p/other.txt", 7),
        restarted: true,
    });
    assert_eq!(s.tether, None, "a stale id must not survive the restart");
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
    };
    let result = serde_json::to_value(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            name: "proj".into(),
            paths: vec![],
            projects: Vec::new(),
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
fn space_z_asks_the_shell_to_open_a_new_window() {
    let mut s = session();
    // `Space z` — was `Space Alt-x` until that chord became save-and-close (docs/tether.md).
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()), ROWS);
    let fx = s.on_key(KeyCode::Char('z'), Mods::NONE, Some("z".into()), ROWS);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::NewWindow(_)))),
        "Space z should emit ShellAction::NewWindow"
    );
    // It's a pure shell hand-off — no server traffic, and crucially not a buffer/close (that's
    // `Space x`).
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "opening a window issues no RPC"
    );
}

// ---- hints (docs/hints.md) --------------------------------------------------------

/// A non-placeholder session (hints display nowhere on the boot placeholder).
fn hint_session() -> Session {
    use aether_client::session::BufferInfo;
    Session::new(
        aether_protocol::workspace::WorkspaceInfo {
            name: "w".into(),
            paths: vec!["/tmp/w".into()],
            projects: Vec::new(),
        },
        BufferInfo {
            buffer_id: 1,
            label: "a.rs".into(),
            path: Some("/tmp/w/a.rs".into()),
            language: None,
            revision: 0,
            saved_revision: 0,
            cursor: aether_protocol::cursor::CursorState::default(),
            scroll: None,
            transient: false,
            lsp_server: None,
        },
    )
}

/// Every `hints/record` request in `fx`, as `(hint_id, event)` pairs.
fn hint_records(fx: &Effects) -> Vec<(String, String)> {
    fx.0.iter()
        .filter_map(|e| match e {
            Effect::Request { method, params, .. } if *method == "hints/record" => Some((
                params["hint_id"].as_str().unwrap().to_string(),
                params["event"].as_str().unwrap().to_string(),
            )),
            _ => None,
        })
        .collect()
}

/// Drive a session through the connect sequence with hints on: `startup()` → canned settings +
/// (empty) hints snapshot → one tick to stamp the clock and sample the first hint. Returns the
/// events the tick emitted.
fn adopt_hints(s: &mut Session) -> Effects {
    let fx = s.startup();
    let reqs: Vec<(u64, &str)> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { token, method, .. } => Some((*token, *method)),
                _ => None,
            })
            .collect();
    assert_eq!(
        reqs.iter().map(|(_, m)| *m).collect::<Vec<_>>(),
        vec!["settings/get", "hints/state", "history/state"],
        "startup fetches settings, the hint snapshot, then the input-history lists"
    );
    let settings = json!({ "wrap": "soft", "ligatures": true, "buffer_font_size": 14, "ui_font_size": 13, "hints": true });
    s.on_rpc_result(reqs[0].0, Ok(settings));
    s.on_rpc_result(reqs[1].0, Ok(json!({})));
    s.on_rpc_result(reqs[2].0, Ok(json!({})));
    s.on_hint_tick(1_000_000_000_000) // an arbitrary wall clock, ~2001
}

#[test]
fn hints_snapshot_adoption_requests_an_immediate_tick() {
    use aether_protocol::picker::PickerKind;

    // The boot chooser, as the shells drive it: placeholder session, Workspaces picker, then
    // startup's snapshot adopts. Adoption must ask the shell for one out-of-band tick
    // (`HintTickNow`) — the engine is clockless until a tick, and waiting for the periodic one
    // would hold the first hint back ~2s after boot.
    let mut s = session();
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);
    s.picker.as_mut().unwrap().loaded = true;
    let fx = s.startup();
    let reqs: Vec<(u64, &str)> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { token, method, .. } => Some((*token, *method)),
                _ => None,
            })
            .collect();
    let settings = json!({ "wrap": "soft", "ligatures": true, "buffer_font_size": 14, "ui_font_size": 13, "hints": true });
    let _ = s.on_rpc_result(reqs[0].0, Ok(settings));
    let fx = s.on_rpc_result(reqs[1].0, Ok(json!({})));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::HintTickNow)),
        "adopting the snapshot asks the shell for an immediate tick"
    );
    assert!(
        s.hint_view().is_none(),
        "still clockless until the tick lands"
    );

    // The shell answers with one tick: the first intro hint shows now, not seconds later.
    let _ = s.on_hint_tick(1_000_000_000_000);
    let v = s
        .hint_view()
        .expect("the chooser hint shows on the answering tick");
    assert!(
        v.text.contains("creates that workspace"),
        "an empty chooser leads with the create hint: {}",
        v.text
    );

    // A failed snapshot fetch asks for nothing — the engine stays dormant.
    let mut s = session();
    let fx = s.startup();
    let token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, method, .. } if *method == "hints/state" => Some(*token),
                _ => None,
            })
            .expect("hints/state fetched");
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "hints/state",
            code: -32601,
            message: "method not found".into(),
        }),
    );
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::HintTickNow)),
        "no tick request when adoption failed"
    );
}

#[test]
fn hints_first_tick_after_adoption_shows_a_survival_hint() {
    let mut s = hint_session();
    assert!(s.hint_view().is_none(), "nothing shows before the snapshot");
    let fx = adopt_hints(&mut s);
    let view = s
        .hint_view()
        .expect("a hint holds the corner after the first tick");
    let recs = hint_records(&fx);
    assert_eq!(recs.len(), 1, "exactly one Shown recorded: {recs:?}");
    assert_eq!(recs[0].1, "shown");
    let (before, keys, after) = view.parts();
    assert!(!keys.is_empty(), "the view carries a key label");
    assert!(
        !before.is_empty() || !after.is_empty(),
        "the view carries sentence text around the key slot"
    );
}

#[test]
fn hints_intro_teaches_dismiss_then_toggle() {
    let mut s = hint_session();
    let fx = adopt_hints(&mut s);
    // The tutorial opening: the very first hint teaches dismissal.
    assert_eq!(hint_records(&fx)[0].0, "dismiss");
    let view = s.hint_view().unwrap();
    assert_eq!(view.parts().1, "Space h");

    // Trying it advances the intro to the toggle hint — a follow, not a dismissal.
    key(&mut s, ' ');
    let fx = key(&mut s, 'h');
    let recs = hint_records(&fx);
    assert!(
        recs.iter()
            .any(|(id, ev)| id == "dismiss" && ev == "followed"),
        "Space h on the dismiss hint is its follow: {recs:?}"
    );
    assert!(!recs.iter().any(|(_, ev)| ev == "dismissed"));
    let view = s.hint_view().expect("the intro continues");
    assert_eq!(view.parts().1, "Space Alt-h", "the toggle hint is second");

    // Trying *that* follows the toggle hint, turns hints off, persists, and toasts the way back.
    key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(!s.hints_enabled);
    assert!(s.hint_view().is_none());
    let recs = hint_records(&fx);
    assert!(
        recs.iter()
            .any(|(id, ev)| id == "toggle" && ev == "followed"),
        "Space Alt-h on the toggle hint is its follow: {recs:?}"
    );
    let settings: Vec<_> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, params, .. } if *method == "settings/set" => {
                    Some(params.clone())
                }
                _ => None,
            })
            .collect();
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0]["hints"], json!(false));
    assert!(
        fx.0.iter().any(
            |e| matches!(e, Effect::Toast { message, .. } if message.contains("Hints disabled"))
        ),
        "turning hints off is confirmed with a toast"
    );
}

#[test]
fn hints_space_h_dismisses_and_rotates() {
    let mut s = hint_session();
    adopt_hints(&mut s);
    // Advance past the dismiss hint (following it is the intro's special case); the toggle hint
    // is an ordinary dismissal target.
    key(&mut s, ' ');
    key(&mut s, 'h');
    let before = s.hint_view().expect("the toggle hint is displayed");
    assert_eq!(before.parts().1, "Space Alt-h");

    key(&mut s, ' ');
    let fx = key(&mut s, 'h');
    let recs = hint_records(&fx);
    assert!(
        recs.iter()
            .any(|(id, ev)| id == "toggle" && ev == "dismissed"),
        "Space h reports the dismissal: {recs:?}"
    );
    assert!(
        recs.iter().any(|(id, ev)| id == "dismiss" && ev == "used"),
        "the press also demonstrates the dismiss binding: {recs:?}"
    );
    let after = s.hint_view();
    assert_ne!(after, Some(before), "a dismissed hint rotates away");
    // The replacement (if the pool had one) was recorded as shown.
    if after.is_some() {
        assert!(recs.iter().any(|(_, ev)| ev == "shown"));
    }
}

#[test]
fn hints_space_alt_h_toggles_and_persists() {
    let mut s = hint_session();
    adopt_hints(&mut s);
    assert!(s.hint_view().is_some());

    // Space Alt-h off: the corner empties, the flip persists, and a toast names the way back.
    key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(!s.hints_enabled);
    assert!(s.hint_view().is_none());
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, params, .. }
                if *method == "settings/set" && params["hints"] == json!(false)
        )),
        "the flip persists"
    );
    assert!(
        fx.0.iter().any(
            |e| matches!(e, Effect::Toast { message, .. } if message.contains("Hints disabled"))
        ),
        "turning hints off is confirmed with a toast"
    );

    // And back on.
    key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS);
    assert!(s.hints_enabled);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "settings/set");
    assert_eq!(params["hints"], json!(true));
    assert!(
        fx.0.iter().any(
            |e| matches!(e, Effect::Toast { message, .. } if message.contains("Hints enabled"))
        ),
        "turning hints back on is confirmed too"
    );
}

#[test]
fn hints_following_the_displayed_binding_records_followed() {
    let mut s = hint_session();
    let fx = adopt_hints(&mut s);
    let shown_id = hint_records(&fx)[0].0.clone();

    // "Press" the displayed hint's binding (the tier-0 hints map to these keys).
    let fx = match shown_id.as_str() {
        "dismiss" => {
            key(&mut s, ' ');
            key(&mut s, 'h')
        }
        "toggle" => {
            key(&mut s, ' ');
            s.on_key(KeyCode::Char('h'), Mods::ALT, None, ROWS)
        }
        "help" => {
            key(&mut s, ' ');
            key(&mut s, '/')
        }
        "quit" => {
            key(&mut s, ' ');
            key(&mut s, 'q')
        }
        "insert" => key(&mut s, 'i'),
        "motion-hjkl" => key(&mut s, 'j'),
        other => panic!("unexpected tier-0 hint in Normal: {other}"),
    };
    let recs = hint_records(&fx);
    assert!(
        recs.iter()
            .any(|(id, ev)| *id == shown_id && ev == "followed"),
        "the on-screen hint's own binding is a follow: {recs:?}"
    );
}

#[test]
fn hints_off_screen_binding_records_used() {
    let mut s = hint_session();
    let fx = adopt_hints(&mut s);
    let shown_id = hint_records(&fx)[0].0.clone();
    // Press a tier-0 binding that is NOT the displayed hint.
    let fx = if shown_id == "insert" {
        key(&mut s, 'j') // motion-hjkl
    } else {
        key(&mut s, 'i') // insert — entering Insert also samples that context's hint (a Shown)
    };
    let used: Vec<_> = hint_records(&fx)
        .into_iter()
        .filter(|(_, ev)| ev == "used")
        .collect();
    assert_eq!(used.len(), 1, "exactly one Used event: {used:?}");
    assert_ne!(used[0].0, shown_id, "an off-screen use is not a follow");
}

#[test]
fn hints_setting_gates_view_and_traffic() {
    let mut s = hint_session();
    adopt_hints(&mut s);
    assert!(s.hint_view().is_some());

    // Toggle the "Hints" row off via the settings overlay (as a click would).
    s.open_app_settings();
    let idx = s
        .app_setting_rows()
        .iter()
        .position(|r| r.label == "Hints")
        .expect("the hints row exists");
    let fx = s.on_event(aether_client::update::Event::AppSettingToggle(idx));
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "settings/set");
    assert_eq!(params["hints"], json!(false), "the flip persists");

    assert!(s.hint_view().is_none(), "the corner empties immediately");
    let fx = s.on_hint_tick(1_000_000_002_000);
    assert!(hint_records(&fx).is_empty(), "no traffic while off");
    let fx = key(&mut s, 'i');
    assert!(hint_records(&fx).is_empty(), "no observation while off");
}

#[test]
fn hints_context_follows_overlays_and_reverts() {
    let mut s = hint_session();
    adopt_hints(&mut s);
    let normal_view = s.hint_view().expect("a Normal-mode hint");

    // The app-settings overlay is its own context with (today) an empty pool: corner goes blank.
    s.open_app_settings();
    assert!(
        s.hint_view().is_none(),
        "no hints are eligible in the Settings context yet"
    );

    // Esc back to Normal: the frozen slot restores the same hint, with no fresh Shown.
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(
        s.hint_view(),
        Some(normal_view),
        "the previous hint returns"
    );
    assert!(
        hint_records(&fx).is_empty(),
        "restoring a frozen slot records nothing"
    );
}

#[test]
fn hints_state_fetch_failure_is_loud() {
    use aether_client::update::Event;
    // A daemon that predates the hints RPCs answers `hints/state` with method-not-found (the
    // version gate can't catch it — a dev rebuild keeps the version string). The engine staying
    // silently dormant is undebuggable, so the failure surfaces as an actionable toast.
    let mut s = hint_session();
    let fx = s.on_event(Event::HintsStateLoaded(Err("method not found".into())));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast { message, kind: ToastKind::Warning, .. }
                if message.contains("restart the Aether server")
        )),
        "a failed hints snapshot fetch must say so"
    );
    assert!(s.hint_view().is_none(), "the engine stays dormant");

    // With hints off the failure is irrelevant — stay quiet.
    let mut s = hint_session();
    s.hints_enabled = false;
    let fx = s.on_event(Event::HintsStateLoaded(Err("method not found".into())));
    assert!(fx.0.is_empty());
}

#[test]
fn hints_workspace_chooser_hint_tracks_the_list() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = hint_session();
    adopt_hints(&mut s);

    // The Workspaces picker opens with a *loaded but empty* list (a fresh install's chooser).
    // Picker state is set directly — the wire adoption path is covered by the picker tests.
    let mut p = aether_client::picker::PickerState::new(PickerKind::Workspaces);
    p.loaded = true;
    s.picker = Some(p);
    let fx = s.on_hint_tick(1_000_000_002_000);
    let view = s.hint_view().expect("the chooser offers a hint");
    assert!(
        view.text.contains("creates that workspace"),
        "empty list teaches creation: {}",
        view.text
    );
    assert!(hint_records(&fx)
        .iter()
        .any(|(id, ev)| id == "workspace-create" && ev == "shown"));

    // Workspaces exist: the hint flips to opening one.
    s.picker.as_mut().unwrap().items = vec![PickerItem::Workspace {
        name: "aether".into(),
        unsaved_buffers: 0,
        match_indices: Vec::new(),
    }];
    s.on_hint_tick(1_000_000_004_000);
    let view = s.hint_view().expect("still a chooser hint");
    assert!(
        view.text.contains("open the selected workspace"),
        "a populated list teaches opening: {}",
        view.text
    );

    // Before the list loads, neither chooser hint can fire (the pre-load flash must not burn
    // the create hint's intro slot).
    let mut s = hint_session();
    adopt_hints(&mut s);
    s.picker = Some(aether_client::picker::PickerState::new(
        PickerKind::Workspaces,
    ));
    s.on_hint_tick(1_000_000_002_000);
    if let Some(view) = s.hint_view() {
        assert!(
            !view.text.contains("workspace"),
            "unloaded list must not claim emptiness: {}",
            view.text
        );
    }
}

#[test]
fn hints_boot_chooser_drives_the_corner() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    // The boot chooser (every shell) is the core Workspaces picker over a placeholder session;
    // its hints run through the ordinary tick/view path — the picker context outranks the
    // placeholder check (docs/hints.md).
    let mut s = session();
    adopt_hints(&mut s);
    let _ = s.open_picker(PickerKind::Workspaces, None, None, false, None);

    // List not loaded yet: the chooser pair can't fire (the pre-load flash must not burn
    // the create hint's intro slot).
    s.on_hint_tick(1_000_000_002_000);
    if let Some(v) = s.hint_view() {
        assert!(
            !v.text.contains("workspace"),
            "unloaded list must not claim emptiness: {}",
            v.text
        );
    }

    // Loaded and empty (a fresh install): teach creating — preempting anything sampled.
    s.picker.as_mut().unwrap().loaded = true;
    let fx = s.on_hint_tick(1_000_000_004_000);
    let v = s.hint_view().expect("a chooser hint");
    assert!(
        v.text.contains("creates that workspace"),
        "empty chooser teaches creation: {}",
        v.text
    );
    assert!(hint_records(&fx)
        .iter()
        .any(|(id, ev)| id == "workspace-create" && ev == "shown"));

    // Populated: teach opening.
    s.picker.as_mut().unwrap().items = vec![PickerItem::Workspace {
        name: "aether".into(),
        unsaved_buffers: 0,
        match_indices: Vec::new(),
    }];
    s.on_hint_tick(1_000_000_006_000);
    let v = s.hint_view().expect("a chooser hint");
    assert!(
        v.text.contains("open the selected workspace"),
        "a populated chooser teaches opening: {}",
        v.text
    );
}

// ---- input history (docs/input-history.md) --------------------------------------------------

/// The plain values of one recall list — most assertions don't care about the carried filters.
fn hist(s: &Session, kind: aether_protocol::history::HistoryKind) -> Vec<&str> {
    s.history
        .list(kind)
        .iter()
        .map(|e| e.value.as_str())
        .collect()
}

/// Adopt a canned set of recall lists, as `history/state` would at boot. Entries may be written as
/// bare strings when the test doesn't care about the filters they carry; anything else is passed
/// through as the full `{ value, filters }` wire shape.
fn adopt_history(s: &mut Session, lists: serde_json::Value) {
    use aether_client::update::Event;
    use aether_protocol::history::HistoryStateResult;
    let expanded: serde_json::Value = lists
        .as_object()
        .expect("lists is an object")
        .iter()
        .map(|(kind, entries)| {
            let entries: Vec<serde_json::Value> = entries
                .as_array()
                .expect("a list of entries")
                .iter()
                .map(|e| match e {
                    serde_json::Value::String(v) => json!({ "value": v }),
                    other => other.clone(),
                })
                .collect();
            (kind.clone(), serde_json::Value::Array(entries))
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    let result: HistoryStateResult = serde_json::from_value(json!({ "lists": expanded })).unwrap();
    let _ = s.on_event(Event::HistoryLoaded(Ok(result)));
}

/// `Up`/`Down` in the search prompt walk the committed queries: `Up` steps towards older and stops
/// at the oldest, `Down` comes back and restores what was being typed. Each step re-runs the
/// incremental search so the matches preview as you go.
#[test]
fn search_up_down_walk_the_query_history_and_restore_the_draft() {
    use aether_client::session::Mode;
    let mut s = session();
    adopt_history(&mut s, json!({ "search": ["older", "newer"] }));

    let _ = key(&mut s, '/');
    assert_eq!(s.mode, Mode::Search);
    let _ = s.search_set_query("draft".into());

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "newer", "Up recalls the newest entry");
    assert_eq!(
        find_request(&fx, "search/set").map(|p| p["query"].clone()),
        Some(json!("newer")),
        "each recall previews its matches"
    );
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "older");
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "older", "the oldest entry doesn't wrap");

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "newer");
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(
        s.search.query, "draft",
        "stepping past the newest restores the typed draft"
    );
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "draft", "and stays there");

    // Alt-k/j remain as the unlisted alias.
    let _ = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert_eq!(s.search.query, "newer");
    let _ = s.on_key(KeyCode::Char('j'), Mods::ALT, None, ROWS);
    assert_eq!(s.search.query, "draft");
}

/// Typing abandons a walk: the next `Up` starts again from the newest entry and stashes the *new*
/// draft, rather than continuing from where the previous walk left off.
#[test]
fn typing_abandons_a_history_walk() {
    let mut s = session();
    adopt_history(&mut s, json!({ "search": ["one", "two"] }));
    let _ = key(&mut s, '/');
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "one");

    let _ = s.search_set_query("typed".into());
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "two", "the walk restarts from the newest");
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "typed", "and restores the newer draft");
}

/// Committing a search records it — once. The record is applied locally *and* sent to the server
/// (which persists it for other windows); a repeat of the newest entry sends nothing.
#[test]
fn committing_a_search_records_it_locally_and_server_side() {
    use aether_protocol::history::HistoryKind;
    let mut s = session();
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("needle".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "search", "value": "needle" }))
    );
    assert_eq!(hist(&s, HistoryKind::Search), ["needle"]);

    // Same query again: already the newest entry, so no list change and no traffic.
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("needle".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(find_request(&fx, "history/record").is_none());
    assert_eq!(hist(&s, HistoryKind::Search), ["needle"]);
}

/// The grep picker's query recalls on `Up`/`Down` — and only grep's does, since Alt-k/j own list
/// movement in every picker and the fuzzy kinds have no query worth recalling.
#[test]
fn grep_picker_query_recalls_on_up_down() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    adopt_history(&mut s, json!({ "grep": ["fn resolve", "wrap_state"] }));

    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    let fx = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "wrap_state");
    assert_eq!(
        find_request(&fx, "picker/query").map(|p| p["query"].clone()),
        Some(json!("wrap_state")),
        "the recalled query re-runs the search"
    );
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "fn resolve");

    // Alt-k still moves the highlight rather than the history.
    let _ = s.on_key(KeyCode::Char('k'), Mods::ALT, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "fn resolve");

    // Files has no query history: Up is inert there (and mustn't touch the query).
    let mut s = session();
    adopt_history(&mut s, json!({ "grep": ["fn resolve"] }));
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    let _ = s.picker_set_query("mai".into());
    let fx = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.picker.as_ref().unwrap().query, "mai");
    assert!(find_request(&fx, "picker/query").is_none());
}

/// Closing the grep picker is what commits its query to the history — not each keystroke, or the
/// list would fill with prefixes. Queries too short to have run a search aren't recorded.
#[test]
fn closing_grep_records_the_settled_query_only() {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    // Typing streams queries but records nothing.
    for q in ["w", "wr", "wrap"] {
        let fx = s.picker_set_query(q.into());
        assert!(find_request(&fx, "history/record").is_none());
    }
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "grep", "value": "wrap" })),
        "only the query the user settled on is recorded"
    );
    assert_eq!(hist(&s, HistoryKind::Grep), ["wrap"]);

    // A one-character query never ran a search, so it never enters the history.
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    let _ = s.picker_set_query("w".into());
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert!(find_request(&fx, "history/record").is_none());
    assert_eq!(hist(&s, HistoryKind::Grep), ["wrap"]);
}

/// Grep opens fully reset — query, hits and chips — like every kind but the changes pickers. The
/// server holds the filters, so all the client does is ask for the wiping scope.
#[test]
fn grep_opens_fully_reset() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    let fx = s.open_picker(PickerKind::Grep, None, None, false, None);
    let view = find_request(&fx, "picker/view").expect("opens via picker/view");
    assert_eq!(view["reset"], json!("all"));
    assert!(
        view.get("center_on_cursor").is_none(),
        "nothing to centre on — the hits went with the query"
    );
    // The client-side chip row starts empty too, so the render can't show chips the server has
    // just dropped (it adopts `filters` back from the view result).
    assert_eq!(
        s.picker.as_ref().map(|p| p.wire_filters()),
        Some(aether_protocol::picker::PickerFilters::default())
    );
}

/// The glob and path chip editors recall on `Up`/`Down` too, from separate lists, and commit their
/// field text on Enter. Alt-j/k stay on suggestion cycling.
#[test]
fn chip_editor_fields_recall_and_record_separately() {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    adopt_history(
        &mut s,
        json!({ "glob": ["*.toml", "*.rs"], "path": ["crates/aether-server"] }),
    );
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);

    // Alt-g opens the glob editor; Up walks the glob list (not the path one).
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None, ROWS);
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    let ed = s.picker.as_ref().unwrap().chip_editor.as_ref().unwrap();
    assert_eq!(ed.input.text, "*.rs");
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    let ed = s.picker.as_ref().unwrap().chip_editor.as_ref().unwrap();
    assert_eq!(ed.input.text, "*.toml");

    // Enter commits: the chip lands and the field text is recorded.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "glob", "value": "*.toml" }))
    );
    // `*.toml` was already in the list; re-committing moves it to newest rather than duplicating.
    assert_eq!(hist(&s, HistoryKind::Glob), ["*.rs", "*.toml"]);
    assert_eq!(
        hist(&s, HistoryKind::Path),
        ["crates/aether-server"],
        "the path list is untouched by a glob commit"
    );
}

/// A recalled search restores the *match options* it ran under, not just its text — a regex
/// recalled under literal matching would quietly match nothing. Stepping back off the walk
/// restores the options the user had, so a recall is never destructive.
#[test]
fn search_recall_restores_match_options_and_down_restores_yours() {
    use aether_protocol::picker::{CaseMode, MatchOptions};

    let mut s = session();
    adopt_history(
        &mut s,
        json!({ "search": [{ "value": "f.o", "filters": { "regex": true } }] }),
    );

    let _ = key(&mut s, '/');
    let _ = s.search_set_query("plain".into());
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None, ROWS); // smart -> sensitive
    assert_eq!(s.search.options.case, CaseMode::Sensitive);

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "f.o");
    assert_eq!(
        s.search.options,
        MatchOptions {
            regex: true,
            ..Default::default()
        },
        "the recalled entry's options replace the current ones wholesale"
    );
    // The re-run carries them, so the preview matches the way the recalled search did.
    assert_eq!(
        find_request(&fx, "search/set").map(|p| p["options"].clone()),
        Some(json!({ "regex": true }))
    );

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    assert_eq!(s.search.query, "plain");
    assert_eq!(
        s.search.options.case,
        CaseMode::Sensitive,
        "Down restores the options that were in effect before the walk"
    );
    assert!(!s.search.options.regex);
}

/// The same for grep, over the whole chip row: recall reproduces the search that was run — scope
/// included — and `Down` puts the row you had back.
#[test]
fn grep_recall_restores_the_chip_row() {
    use aether_protocol::picker::{PickerKind, ScopedPath};

    let mut s = session();
    adopt_history(
        &mut s,
        json!({ "grep": [{
            "value": "fn resolve",
            "filters": { "regex": true, "globs": ["*.ts"] }
        }] }),
    );
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    // Start from a different scope of the user's own: a `*.rs` glob.
    {
        let p = s.picker.as_mut().unwrap();
        p.chips = aether_client::chips::adopt_filters(&aether_protocol::picker::PickerFilters {
            globs: vec!["*.rs".into()],
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "crates".into(),
                is_file: false,
            }],
            ..Default::default()
        });
    }

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None, ROWS);
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.query, "fn resolve");
    let filters = p.wire_filters();
    assert_eq!(filters.globs, ["*.ts"], "the entry's globs replace the row");
    assert!(filters.regex, "and its match options come back too");
    assert!(
        filters.directories.is_empty(),
        "the dir scope the entry didn't have is gone"
    );
    // Query and the adopted filters travel together — one round-trip, no intermediate search
    // under a half-applied configuration.
    let q = find_request(&fx, "picker/query").expect("re-runs the search");
    assert_eq!(q["query"], json!("fn resolve"));
    assert_eq!(q["filters"]["globs"], json!(["*.ts"]));

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None, ROWS);
    let restored = s.picker.as_ref().unwrap().wire_filters();
    assert_eq!(restored.globs, ["*.rs"], "Down restores the user's own row");
    assert_eq!(restored.directories.len(), 1);
    assert!(!restored.regex);
}

/// Closing grep records the chip row alongside the query, so the entry can reproduce the search.
#[test]
fn closing_grep_records_the_chip_row_with_the_query() {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    let _ = s.picker_set_query("wrap".into());
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None, ROWS); // Alt-e: regex on
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "grep", "value": "wrap", "filters": { "regex": true } })),
        "the filters ride the record as flattened fields"
    );
    let entry = &s.history.list(HistoryKind::Grep)[0];
    assert!(entry.filters.regex);
}

/// Re-running a remembered term under different filters updates that entry rather than adding a
/// second row that reads identically while walking.
#[test]
fn re_recording_a_term_updates_its_filters_in_place() {
    use aether_protocol::history::HistoryKind;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    for regex in [false, true] {
        let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
        let _ = s.picker_set_query("wrap".into());
        if regex {
            let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None, ROWS);
        }
        let _ = s.on_key(KeyCode::Esc, Mods::NONE, None, ROWS);
    }
    assert_eq!(hist(&s, HistoryKind::Grep), ["wrap"], "one row, not two");
    assert!(
        s.history.list(HistoryKind::Grep)[0].filters.regex,
        "the newest configuration wins"
    );
}

// ---- markdown reading view (docs/markdown-view.md) ----------------------------------------------

fn md_session() -> Session {
    let mut s = session();
    s.buffer.language = Some("markdown".into());
    s
}

fn leader(s: &mut Session, c: char) -> Effects {
    let _ = key(s, ' ');
    key(s, c)
}

/// A canned reading-view setup: `Space v` on a markdown buffer, content fetched and parsed.
/// Layout: heading (line 0), paragraph (line 2), paragraph with a link (line 4).
fn read_session() -> Session {
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "buffer/content");
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "revision": 1,
            "text": "# Title\n\nFirst para.\n\nSee [docs](https://x.y) here.\n",
        })),
    );
    s
}

#[test]
fn space_v_enters_reading_view_and_fetches_content() {
    use aether_client::session::Mode;
    let s = read_session();
    assert_eq!(s.mode, Mode::Read);
    let read = s.read.as_ref().expect("reading view active");
    assert_eq!(read.revision, 1);
    assert!(!read.loading);
    // heading + 2 paragraphs + the link, in document order.
    assert_eq!(read.elements.len(), 4);
}

#[test]
fn space_v_on_non_markdown_toasts_and_stays_normal() {
    use aether_client::session::Mode;
    let mut s = session(); // language: None
    let fx = leader(&mut s, 'v');
    assert_eq!(s.mode, Mode::Normal);
    assert!(s.read.is_none());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
}

#[test]
fn read_j_steps_focus_via_goto_to_next_block() {
    let mut s = read_session();
    // Cursor at 0,0 → focus is the heading; `j` lands on the first paragraph's start (line 2).
    let fx = key(&mut s, 'j');
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["kind"], json!("goto"));
    assert_eq!(params["motion"]["position"], json!({"line": 2, "col": 0}));
    assert_eq!(params["extend_selection"], json!(false));
}

#[test]
fn read_count_applies_to_element_steps() {
    let mut s = read_session();
    // `2j` from the heading skips to the second paragraph (line 4).
    let _ = key(&mut s, '2');
    let fx = key(&mut s, 'j');
    let (_t, _m, params) = the_request(&fx);
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 0}));
}

/// Walk `read_session` to the link paragraph and step into its link: `j` `j` (blocks), then
/// `l` (the within-block link ring). Adopts each Goto's cursor so focus derives.
fn focus_the_link(s: &mut Session) {
    for line in [2u32, 4] {
        let fx = key(s, 'j');
        let (t, m, _p) = the_request(&fx);
        assert_eq!(m, "cursor/move");
        let _ = s.on_rpc_result(
            t,
            Ok(json!({
                "position": {"line": line, "col": 0},
                "anchor": {"line": line, "col": 0},
            })),
        );
    }
    // `l` enters the block's link ring at its first link (line 4, col 4 — "See " precedes).
    let fx = key(s, 'l');
    let (t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 4}));
    let _ = s.on_rpc_result(
        t,
        Ok(json!({
            "position": {"line": 4, "col": 4},
            "anchor": {"line": 4, "col": 4},
        })),
    );
}

#[test]
fn read_l_focuses_the_link_in_block_and_enter_opens_it() {
    let mut s = read_session();
    focus_the_link(&mut s);
    // Enter follows the focused link with the system opener.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y"
        )),
        "Enter on a link opens it externally"
    );
    // At the ring's end, another `l` is a quiet no-op (single-link block).
    let fx = key(&mut s, 'l');
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "no Goto past the last link in the block"
    );
}

/// `v`/`Alt-v`: the editor's half-page cursor motion, verbatim — the server resolves it in
/// editor wrap geometry and the returned cursor derives focus (best-effort distance, framed
/// landing — docs/markdown-view.md §2.3).
#[test]
fn read_v_rides_the_editor_half_page_motion() {
    let mut s = read_session();
    // The viewport subscription stays alive in Read (§1.5); the motion needs its id for the
    // editor wrap geometry.
    s.viewport_id = Some(7);
    let fx = key(&mut s, 'v');
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["kind"], json!("visual_line"));
    assert_eq!(params["motion"]["direction"], json!("down"));
    assert_eq!(params["motion"]["count"], json!(ROWS / 2));
}

/// `z`/`Alt-z`: the server's cursor-motion history, verbatim — the returned cursor derives
/// focus, so this is "step back/forward through reading positions".
#[test]
fn read_z_walks_the_reading_position_history() {
    let mut s = read_session();
    let fx = key(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "cursor/undo");
    let fx = s.on_key(KeyCode::Char('z'), Mods::ALT, None, ROWS);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "cursor/redo");
}

/// `;`/`Alt-;`: the editor's place-cursor keys — in Read each shell places the *focused
/// element* at the fraction (read scroll is shell-owned).
#[test]
fn read_semicolon_emits_place_cursor() {
    use aether_client::keymap::ViewportPlace;
    let mut s = read_session();
    let fx = key(&mut s, ';');
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::PlaceCursor(ViewportPlace::Upper))
        )),
        "; places the focused element near the top"
    );
}

/// `h` from the first element steps back OUT: the cursor returns to the block's rest byte, so
/// the target clears and the bar stands alone; `h` with nothing selected is a quiet no-op.
#[test]
fn read_h_deselects_back_to_the_block() {
    let mut s = read_session();
    focus_the_link(&mut s);
    // `h` from the (first) link: Goto the paragraph's rest byte — its start, since "See "
    // precedes the link.
    let fx = key(&mut s, 'h');
    let (t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 0}));
    let _ = s.on_rpc_result(
        t,
        Ok(json!({"position": {"line": 4, "col": 0}, "anchor": {"line": 4, "col": 0}})),
    );
    {
        let read = s.read.as_ref().unwrap();
        let cursor = s.buffer.cursor.position;
        assert_eq!(read.target_focus(cursor), None, "deselected — bar alone");
        assert!(read.block_focus(cursor).is_some());
    }
    // Another `h`: nothing selected → quiet no-op.
    let fx = key(&mut s, 'h');
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "h with no target does nothing"
    );
}

#[test]
fn read_tab_shows_the_focused_target_without_following() {
    use aether_client::session::HoverText;
    let mut s = read_session();
    // On a plain block: quiet no-op.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    assert!(
        fx.0.is_empty(),
        "Tab on a non-interactive block does nothing"
    );
    // On a focused link: the URL in the hover popover (whose own keys then apply — Ctrl-c
    // copies it via `keymap::hover_action`), no open, no cursor move.
    focus_the_link(&mut s);
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShowHover(HoverText::Blocks(b))
                if b.len() == 1 && b[0].text == "https://x.y" && b[0].severity.is_none()
        )),
        "Tab reveals the link target in the popover"
    );
    assert!(
        !fx.0
            .iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::OpenUrl(_)))),
        "Tab must not follow the link"
    );
}

#[test]
fn read_ctrl_c_copies_the_focused_elements_source() {
    let mut s = read_session();
    // Cursor on the heading: `Ctrl-c` (the editor's clipboard chord) copies its markdown source.
    let fx = ctrl(&mut s, 'c');
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::WriteClipboard(text) if text == "# Title"
        )),
        "Ctrl-c copies the element source"
    );
    // The old vim-style `y` is gone: bare `y` does nothing in Read.
    let fx = key(&mut s, 'y');
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::WriteClipboard(_))),
        "bare y no longer copies"
    );
}

#[test]
fn read_shift_j_extends_selection_block_wise() {
    let mut s = read_session();
    // Shift-j from the heading: a whole-line block selection heading..=first-paragraph via
    // cursor/set + Line granularity — the anchor plants at the heading's line, the cursor
    // lands on the paragraph's; the server snaps both to the normal form.
    let fx = s.on_key(KeyCode::Char('j'), Mods::SHIFT, Some("J".into()), ROWS);
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/set");
    assert_eq!(p["granularity"], json!("line"));
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(2));
}

#[test]
fn read_x_snaps_then_walks_and_shift_grows() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // First press: the focused block alone, whole-line form (the editor's `x` snaps the
    // current line before walking; a one-line heading: both ends line 0).
    let fx = key(&mut s, 'x');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/set");
    assert_eq!(p["granularity"], json!("line"));
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(0));
    // With the whole heading selected (as the server would hold it), plain `x` WALKS: the
    // next block alone — not an extension.
    s.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 7 };
    let fx = key(&mut s, 'x');
    let (_t, _method, p) = the_request(&fx);
    assert_eq!(p["anchor"]["line"], json!(2));
    assert_eq!(p["position"]["line"], json!(2));
    // Shift-x from the same whole-block selection GROWS the bottom instead.
    let fx = s.on_key(KeyCode::Char('x'), Mods::SHIFT, Some("X".into()), ROWS);
    let (_t, _method, p) = the_request(&fx);
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(2));
}

#[test]
fn read_alt_x_selects_the_previous_block_and_saturates() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // The editor's first-press asymmetry: Alt-x from a bare reading position selects the
    // block *above* (cursor on the first paragraph → the heading), not the focused one.
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 0 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None, ROWS);
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/set");
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(0));
    // At the document top it saturates: Alt-x on the heading selects the heading itself.
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 0 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None, ROWS);
    let (_t, _method, p) = the_request(&fx);
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(0));
}

#[test]
fn read_x_snaps_a_partial_selection_before_advancing() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // A non-whole selection (a few chars inside the first paragraph): plain `x` collapses
    // to the direction's edge block whole — consuming the press without advancing, exactly
    // like the editor's snap-before-walk.
    s.buffer.cursor.anchor = LogicalPosition { line: 2, col: 1 };
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 5 };
    let fx = key(&mut s, 'x');
    let (_t, _method, p) = the_request(&fx);
    assert_eq!(p["anchor"]["line"], json!(2));
    assert_eq!(p["position"]["line"], json!(2));
}

#[test]
fn read_ctrl_c_copies_the_extended_selections_source() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // A whole-line selection over heading + first paragraph, as the server would hold it:
    // copy takes the source slice, inclusive of the end cursor's newline.
    s.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 11 };
    let fx = ctrl(&mut s, 'c');
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::WriteClipboard(text) if text == "# Title\n\nFirst para.\n"
        )),
        "selection source copied"
    );
}

#[test]
fn read_projections_pause_while_the_parse_is_being_refreshed() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::input::BlockEditResult;
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // An edit's new cursor is adopted from its own response, a round trip before the re-parse
    // lands. Deriving focus against the old parse meanwhile painted the bar on whatever block
    // happened to sit at those bytes in the *previous* document — a flash on an unrelated
    // block. Nothing is drawn until the new parse arrives.
    let read = s.read.as_ref().unwrap();
    assert!(read.display_block_focus(&s.buffer.cursor).is_some());
    let cursor = CursorState {
        position: LogicalPosition { line: 4, col: 0 },
        anchor: LogicalPosition { line: 4, col: 0 },
        ..Default::default()
    };
    let fx = s.on_event(Event::BlockEditDone(Ok(BlockEditResult {
        applied: true,
        reason: None,
        revision: 2,
        cursor,
        text: None,
    })));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, .. } if *method == "buffer/content"
        )),
        "the refresh is in flight"
    );
    let read = s.read.as_ref().unwrap();
    assert!(read.loading, "…and the view knows its parse is stale");
    assert_eq!(read.display_block_focus(&s.buffer.cursor), None);
    assert_eq!(read.display_target(&s.buffer.cursor), None);
    assert_eq!(read.display_selection(&s.buffer.cursor), None);
    // The new parse restores them.
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 2, "text": "# Title\n\nMoved.\n\nFirst para.\n" })),
    );
    let read = s.read.as_ref().unwrap();
    assert!(!read.loading);
    assert!(read.display_block_focus(&s.buffer.cursor).is_some());
}

#[test]
fn read_extended_selection_suppresses_the_display_target() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // Cursor inside the link span: the pill shows while the selection is a point…
    s.buffer.cursor.position = LogicalPosition { line: 4, col: 5 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let read = s.read.as_ref().unwrap();
    assert!(read.target_focus(s.buffer.cursor.position).is_some());
    assert!(read.display_target(&s.buffer.cursor).is_some());
    // …and goes away as soon as the selection is extended (§12: one selection at a time).
    s.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    let read = s.read.as_ref().unwrap();
    assert!(read.target_focus(s.buffer.cursor.position).is_some());
    assert!(read.display_target(&s.buffer.cursor).is_none());
}

#[test]
fn read_i_and_a_enter_insert_at_the_blocks_edges() {
    use aether_client::session::Mode;
    use aether_protocol::LogicalPosition;
    // `i` from a bare reading position on the first paragraph: caret at the block's start,
    // editor in Insert, the reading view gone — and no presentation preference recorded.
    let mut s = read_session();
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 0 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = key(&mut s, 'i');
    assert_eq!(s.mode, Mode::Insert);
    assert!(s.read.is_none());
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(p["motion"]["kind"], json!("goto"));
    assert_eq!(p["motion"]["position"], json!({"line": 2, "col": 0}));
    // `a`: the append position — the caret gap before the block's terminating newline.
    let mut s = read_session();
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 0 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = key(&mut s, 'a');
    assert_eq!(s.mode, Mode::Insert);
    let (_t, _m, p) = the_request(&fx);
    assert_eq!(p["motion"]["position"], json!({"line": 2, "col": 11}));
}

#[test]
fn read_i_extended_uses_the_editors_selection_edge() {
    use aether_client::session::Mode;
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // An extended whole-line selection: `i` hands the landing to the server's own
    // Insert-entry motion instead of a client-computed Goto.
    s.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 11 };
    let fx = key(&mut s, 'i');
    assert_eq!(s.mode, Mode::Insert);
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(p["motion"]["kind"], json!("selection_edge"));
}

#[test]
fn read_ctrl_e_changes_block_content_keeping_the_newline() {
    use aether_client::session::Mode;
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // Rewrite the first paragraph: the selection re-materializes over the *content* only —
    // (2,0)..(2,10), the final '.' — so the terminating newline and both separators survive
    // the editor's Change; then Insert on the emptied line.
    s.buffer.cursor.position = LogicalPosition { line: 2, col: 0 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = ctrl(&mut s, 'e');
    assert_eq!(s.mode, Mode::Insert);
    assert!(s.read.is_none());
    let reqs = all_requests(&fx);
    assert_eq!(reqs.len(), 2, "cursor/set then input/change: {reqs:?}");
    assert_eq!(reqs[0].0, "cursor/set");
    assert_eq!(reqs[0].1["anchor"], json!({"line": 2, "col": 0}));
    assert_eq!(reqs[0].1["position"], json!({"line": 2, "col": 10}));
    assert!(
        reqs[0].1.get("granularity").is_none(),
        "exact char range, no snap"
    );
    assert_eq!(reqs[1].0, "input/change");
}

#[test]
fn read_ctrl_o_opens_a_block_via_the_server_then_enters_insert() {
    use aether_client::session::Mode;
    // One RPC — what gets opened (sibling item / paragraph) is the server's parse to decide,
    // not ours — and the mode waits for it: the caret comes back parked in the new block.
    let mut s = read_session();
    let fx = ctrl(&mut s, 'o');
    let (token, method, p) = the_request(&fx);
    assert_eq!(method, "input/open_block");
    assert_eq!(p["above"], json!(false));
    assert_eq!(s.mode, Mode::Read, "still reading until the edit lands");
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "applied": true,
            "revision": 2,
            "cursor": { "position": {"line": 4, "col": 2}, "anchor": {"line": 4, "col": 2} },
        })),
    );
    assert_eq!(s.mode, Mode::Insert);
    assert!(s.read.is_none(), "handed over to the editor");
    assert_eq!(s.buffer.cursor.position.col, 2, "parked past the marker");
    assert!(
        all_requests(&fx).is_empty(),
        "the landing needs no correcting move"
    );

    let mut s = read_session();
    let (_, _, p) = the_request(&ctrl_alt(&mut s, 'o'));
    assert_eq!(p["above"], json!(true));
}

#[test]
fn read_ctrl_o_refused_stays_in_the_reading_view() {
    use aether_client::session::Mode;
    // Opening above front matter would demote it; the refusal must not strand the user in
    // Insert over a document the server never changed.
    let mut s = read_session();
    let token = the_request(&ctrl_alt(&mut s, 'o')).0;
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "applied": false,
            "reason": "Front matter stays at the top",
            "revision": 1,
            "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
        })),
    );
    assert_eq!(s.mode, Mode::Read);
    assert!(s.read.is_some());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
}

#[test]
fn read_transitions_do_not_record_a_presentation_preference() {
    use aether_client::session::Mode;
    // `Space v` back into the editor records "source"; the edit transitions must not — the
    // buffer's remembered choice stays "read", so the next open still renders the view.
    let mut s = read_session();
    let buffer = s.buffer.buffer_id;
    assert_eq!(s.read_preference(buffer), Some(true), "entry recorded read");
    let _ = key(&mut s, 'i');
    assert_eq!(s.mode, Mode::Insert);
    assert_eq!(
        s.read_preference(buffer),
        Some(true),
        "the transition left the preference alone"
    );
    // Contrast: Space v out of Read is the explicit "I prefer source" signal.
    let mut s = read_session();
    let _ = leader(&mut s, 'v');
    assert_eq!(s.read_preference(s.buffer.buffer_id), Some(false));
}

#[test]
fn read_ctrl_z_undoes_from_the_reading_view() {
    let mut s = read_session();
    let fx = ctrl(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "edit/undo");
    let fx = ctrl_alt(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "edit/redo");
}

#[test]
fn read_ctrl_j_k_move_blocks_and_the_editor_moves_paragraphs() {
    // Read: block grain, with the Ctrl-Alt aliases.
    let mut s = read_session();
    let fx = ctrl(&mut s, 'j');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "input/move_block");
    assert_eq!(p["direction"], json!("down"));
    assert_eq!(p["unit"], json!("block"));
    let fx = ctrl_alt(&mut s, 'k');
    let (_t, _m, p) = the_request(&fx);
    assert_eq!(
        (&p["direction"], &p["unit"]),
        (&json!("up"), &json!("block"))
    );
    // Editor: the Ctrl-Alt chords move blank-line paragraphs, any file type.
    let mut s = session();
    let fx = ctrl_alt(&mut s, 'j');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "input/move_block");
    assert_eq!(p["unit"], json!("paragraph"));
}

#[test]
fn read_ctrl_x_cuts_and_the_response_lands_on_the_clipboard() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::input::BlockEditResult;
    let mut s = read_session();
    let fx = ctrl(&mut s, 'x');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "input/delete_block");
    let fx = s.on_event(Event::BlockEditDone(Ok(BlockEditResult {
        applied: true,
        reason: None,
        revision: 2,
        cursor: CursorState::default(),
        text: Some("Beta.\n".into()),
    })));
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::WriteClipboard(t) if t == "Beta.\n")),
        "cut payload reaches the clipboard"
    );
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, .. } if *method == "buffer/content"
        )),
        "the parse refreshes off the edit response"
    );
}

#[test]
fn read_r_reverses_the_selection_and_alt_r_orients_it_forward() {
    // The editor's own pair, reused verbatim: focus derives from the cursor, so swapping the
    // ends moves the bar to the other edge — and Shift-j/k then grow from there, because
    // `read_step` extends from the cursor's block and keeps the anchor.
    let mut s = read_session();
    let (_t, method, _p) = the_request(&key(&mut s, 'x'));
    assert_eq!(method, "cursor/set", "a block selection to reverse");
    let (_t, method, p) = the_request(&key(&mut s, 'r'));
    assert_eq!(method, "cursor/swap_anchor");
    // `forward_only: false` is the wire default and skips (the plain toggle).
    assert!(p.get("forward_only").is_none(), "{p}");
    let fx = s.on_key(KeyCode::Char('r'), Mods::ALT, None, ROWS);
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "cursor/swap_anchor");
    assert_eq!(p["forward_only"], json!(true));
}

#[test]
fn read_ctrl_d_deletes_the_blocks_without_touching_the_clipboard() {
    use aether_client::session::Mode;
    // Same removal and the same RPC as `Ctrl-x`; what separates them is that the removed
    // source is dropped rather than clipboarded — the editor's `Ctrl-d` vs `Ctrl-x`, at
    // block grain. Driven through `on_rpc_result` so the request's own mapping runs.
    let mut s = read_session();
    let (token, method, _p) = the_request(&ctrl(&mut s, 'd'));
    assert_eq!(method, "input/delete_block");
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "applied": true,
            "revision": 2,
            "cursor": { "position": {"line": 2, "col": 0}, "anchor": {"line": 2, "col": 0} },
            "text": "Beta.\n",
        })),
    );
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::WriteClipboard(_))),
        "the payload the server always sends is dropped, not clipboarded"
    );
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, .. } if *method == "buffer/content"
        )),
        "the parse refreshes off the edit response"
    );
    assert_eq!(s.buffer.revision, 2);
    assert_eq!(s.mode, Mode::Read, "a deletion is not a transition");
}

#[test]
fn read_ctrl_v_pastes_through_the_clipboard_flow() {
    use aether_client::session::PasteKind;
    let mut s = read_session();
    let fx = ctrl(&mut s, 'v');
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ReadClipboard(PasteKind::Block { replace: false })
        )),
        "Ctrl-v asks the shell for the clipboard with the block kind"
    );
    // The shell's callback lands as input/paste_block.
    let fx = s.paste(PasteKind::Block { replace: true }, "New block.".into());
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "input/paste_block");
    assert_eq!(p["text"], json!("New block."));
    assert_eq!(p["replace"], json!(true));
}

#[test]
fn read_ctrl_h_l_change_depth_and_refusals_toast_only_with_reason() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::input::BlockEditResult;
    let mut s = read_session();
    let fx = ctrl(&mut s, 'l');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "input/block_depth");
    assert_eq!(p["deeper"], json!(true));
    // A reasoned refusal toasts…
    let refusal = |reason: Option<&str>| {
        Event::BlockEditDone(Ok(BlockEditResult {
            applied: false,
            reason: reason.map(str::to_string),
            revision: 1,
            cursor: CursorState::default(),
            text: None,
        }))
    };
    let fx = s.on_event(refusal(Some("Depth applies to headings and list items")));
    assert!(fx
        .0
        .iter()
        .any(|e| matches!(e, Effect::Toast { message, .. } if message.contains("Depth"))),);
    // …a quiet boundary no-op doesn't.
    let fx = s.on_event(refusal(None));
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
}

#[test]
fn enter_toggles_a_task_items_checkbox() {
    use aether_client::update::Event;
    use aether_protocol::LogicalPosition;
    // A read fixture with task items.
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "buffer/content");
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "- [ ] open\n- [x] done\n" })),
    );
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 6 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "input/toggle_task");
    let _ = Event::BlockEditDone; // (adoption covered by the cut test)
}

#[test]
fn j_steps_one_block_from_a_selected_fence() {
    use aether_protocol::LogicalPosition;
    // `x` on a fence leaves the cursor on the closing fence line's newline — a byte a Code span
    // does not reach, unlike a paragraph's. Resolving the step origin there fell forward to the
    // block *after* the fence, so `j` landed two blocks down and skipped one.
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _method, _) = the_request(&fx);
    let text = "Intro.\n\n```rust\nfn a() {}\n```\n\nMiddle.\n\nLast.\n";
    let _ = s.on_rpc_result(token, Ok(json!({ "revision": 1, "text": text })));
    // Cursor on the closing fence line's newline, as a whole-line block selection leaves it.
    s.buffer.cursor.anchor = LogicalPosition { line: 2, col: 0 };
    s.buffer.cursor.position = LogicalPosition { line: 4, col: 3 };
    let fx = s.on_key(KeyCode::Char('j'), Mods::NONE, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["kind"], "goto");
    let line = params["motion"]["position"]["line"]
        .as_u64()
        .expect("a goto line");
    assert_eq!(
        line, 6,
        "the block right after the fence, not the one past it"
    );
}

#[test]
fn ctrl_a_checks_a_task_item_in_markdown_and_still_adjusts_numbers_elsewhere() {
    // One pair of keys, one meaning — "adjust what's under the cursor, up or down" — resolving to
    // whatever the buffer has. Markdown gives up number adjustment for it, deliberately.
    let mut s = md_session();
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "input/toggle_task");
    assert_eq!(params["set"], json!(true), "up checks the box");
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL_ALT, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "input/toggle_task");
    assert_eq!(params["set"], json!(false), "down unchecks it");
    // The same chord in the reading view resolves the same way — that is the point of it.
    let fx = leader(&mut s, 'v');
    let (token, _method, _) = the_request(&fx);
    let _ = s.on_rpc_result(token, Ok(json!({ "revision": 1, "text": "- [ ] open\n" })));
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "input/toggle_task");
    assert_eq!(params["set"], json!(true));
    // A non-markdown buffer keeps the number adjust.
    let mut s = session();
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None, ROWS);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "input/adjust_number");
    assert_eq!(params["delta"], json!(1));
}

#[test]
fn enter_toggles_a_task_item_holding_more_than_one_block() {
    use aether_protocol::LogicalPosition;
    // An item with a sub-list (or a second paragraph) lists its inner block as an element of its
    // own, so innermost-first resolution stops there and never sees the checkbox. The item around
    // it owns the box — the same outward walk the server's `resolve_toggle_task` does.
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _method, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "- [ ] outer\n\n  - [x] inner\n" })),
    );
    // On the outer item's own text, whose innermost element is the paragraph, not the item.
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 8 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "input/toggle_task");
}

#[test]
fn enter_does_not_follow_a_link_the_selection_has_un_armed() {
    use aether_protocol::LogicalPosition;
    // With the selection extended the shells hide the target pill, so nothing on screen says a
    // link is armed. Enter must not follow one it isn't showing.
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _method, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "[docs](https://example.com) and text.\n" })),
    );
    // Point cursor on the link: Enter follows it.
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 2 };
    s.buffer.cursor.anchor = s.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::OpenUrl(_)))),
        "an armed link still follows"
    );
    // Same cursor, selection extended over the block: no navigation, no request.
    s.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.buffer.cursor.position = LogicalPosition { line: 0, col: 30 };
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        !fx.0
            .iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::OpenUrl(_)))),
        "the un-armed link is not followed"
    );
}

#[test]
fn space_v_toggles_back_to_the_editor() {
    use aether_client::session::Mode;
    let mut s = read_session();
    let fx = leader(&mut s, 'v');
    assert_eq!(s.mode, Mode::Normal);
    assert!(s.read.is_none());
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::RevealCursor(_))),
        "leaving the reading view frames the reading position"
    );
    // The choice is remembered: `Space v` again re-enters without consulting the default.
    s.markdown_read_default = false;
    let fx = leader(&mut s, 'v');
    assert_eq!(s.mode, Mode::Read);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "buffer/content");
}

#[test]
fn read_table_contains_no_editing_action() {
    use aether_client::keymap::{table, Action, KeyContext};
    for b in table(KeyContext::Read) {
        assert!(
            matches!(
                b.action,
                Action::ReadStep(_)
                    | Action::ReadStepLink(_)
                    | Action::ReadShowTarget
                    | Action::ReadActivateNewWindow
                    | Action::ReadStepHeading(_)
                    | Action::PageMotion { .. }
                    | Action::PlaceCursor(_)
                    | Action::MotionUndo
                    | Action::MotionRedo
                    | Action::ReadEnds { .. }
                    | Action::ReadActivate
                    | Action::ReadCopy
                    | Action::ReadSelectBlock(_)
                    // Selection orientation: a cursor/anchor swap, no text touched.
                    | Action::SwapAnchor { .. }
                    // §12's curated edits: undo/redo act on the buffer but create no new
                    // text shape from Read; each future edit action is added here
                    // deliberately, keeping the §1.4 discipline as an explicit list.
                    | Action::Undo
                    | Action::Redo
                    // §12 phase 2: the to-the-editor transitions — they place the cursor
                    // and hand over to the editor's own insert/change machinery.
                    | Action::ReadInsert { .. }
                    | Action::ReadChange
                    | Action::ReadOpenBlock { .. }
                    // §12 phase 3: the structural edits — selection-relative server ops,
                    // atomic, refusals as applied:false.
                    | Action::MoveBlock { .. }
                    | Action::ReadCutBlock
                    | Action::ReadDeleteBlock
                    | Action::ReadPasteBlock { .. }
                    | Action::ReadBlockDepth { .. }
                    // The editor's adjust-the-value pair, re-declared in the Read table so
                    // `Ctrl-a`/`Ctrl-Alt-a` check and uncheck a task item on both sides of
                    // `Space v`. In markdown they never touch a number.
                    | Action::IncrementNumber
                    | Action::DecrementNumber
                    | Action::Scroll { .. }
                    | Action::EnterSearch
                    | Action::SearchCycle(_)
                    | Action::DropSearch
                    | Action::NavBack
                    | Action::NavForward
                    | Action::JumplistStep(_)
                    | Action::JumplistStepInFile(_)
                    | Action::BeginLeader
            ),
            "read-table action {:?} is not on the read-only allowlist",
            b.action
        );
    }
}

#[test]
fn buffer_changed_with_newer_revision_refetches_content() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification};
    let mut s = read_session();
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "buffer/changed".into(),
        params: json!({"buffer_id": s.buffer.buffer_id, "revision": 2}),
    }));
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "buffer/content", "newer revision → re-fetch");
    // A repeat of the same signal is quiet: a fetch is already in flight.
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "buffer/changed".into(),
        params: json!({"buffer_id": s.buffer.buffer_id, "revision": 2}),
    }));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "no duplicate fetch while one is already in flight"
    );
}

#[test]
fn read_undo_refetches_despite_the_restored_older_revision() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::envelope::{JsonRpc, Notification};
    use aether_protocol::input::UndoResult;
    // Undo restores the undone entry's revision NUMBER — revisions identify states, they
    // don't order them — so a change signal can arrive with an *older* revision than the
    // parse. It must still refetch (the old `>` guard left the view rendering undone text).
    let mut s = read_session(); // parse at revision 1
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "buffer/changed".into(),
        params: json!({"buffer_id": s.buffer.buffer_id, "revision": 0}),
    }));
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(
        method, "buffer/content",
        "older revision → still a re-fetch"
    );
    // And the client's own undo response refreshes directly, without waiting for the
    // server's change push.
    let mut s = read_session();
    let fx = s.on_event(Event::UndoRedoDone(Ok(UndoResult {
        revision: 0,
        applied: true,
        cursor: CursorState::default(),
    })));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, .. } if *method == "buffer/content"
        )),
        "own undo response → re-fetch"
    );
}

#[test]
fn jump_shaped_open_lands_in_editor_file_shaped_in_read() {
    use aether_client::session::Mode;
    use aether_protocol::LogicalPosition;
    let mut s = md_session();

    // Jump-shaped (a grep hit): markdown target still opens in the editor.
    let fx = s.open_path_at(
        "/tmp/doc.md".into(),
        Some(LogicalPosition { line: 3, col: 0 }),
        None,
    );
    let (token, method, _p) = the_request(&fx);
    assert_eq!(method, "buffer/open");
    let open = json!({
        "buffer_id": 7, "language": "markdown", "line_count": 5, "byte_count": 40,
        "revision": 0, "saved_revision": 0, "path": "/tmp/doc.md",
    });
    let fx = s.on_rpc_result(token, Ok(open.clone()));
    assert_eq!(s.mode, Mode::Normal, "jump-shaped → editor");
    assert!(s.read.is_none());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));

    // File-shaped (files picker / a doc link): opens as a reading view.
    let fx = s.open_path_at("/tmp/other.md".into(), None, None);
    let (token, _m, _p) = the_request(&fx);
    let other = json!({
        "buffer_id": 8, "language": "markdown", "line_count": 5, "byte_count": 40,
        "revision": 0, "saved_revision": 0, "path": "/tmp/other.md",
    });
    let fx = s.on_rpc_result(token, Ok(other));
    assert_eq!(s.mode, Mode::Read, "file-shaped → reading view");
    assert!(s.read.as_ref().is_some_and(|r| r.loading));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Request { method, .. } if *method == "buffer/content"
        )),
        "the content fetch rides the switch"
    );
}

#[test]
fn read_adopt_requests_fence_highlights_and_adopts_them() {
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "revision": 1,
            "text": "# T\n\n```rust\nfn x() {}\n```\n",
        })),
    );
    // The parse fans out one highlight request per fenced block.
    let (hl_token, method, params) = the_request(&fx);
    assert_eq!(method, "syntax/highlight_snippet");
    assert_eq!(params["language"], json!("rust"));
    assert_eq!(params["text"], json!("fn x() {}"));

    // The result lands keyed by the fence's span start and bumps the layout generation.
    let gen_before = s.read.as_ref().unwrap().hl_gen;
    let _ = s.on_rpc_result(
        hl_token,
        Ok(json!({"highlights": [{"start": 0, "end": 2, "kind": "keyword"}]})),
    );
    let read = s.read.as_ref().unwrap();
    assert_eq!(read.hl_gen, gen_before + 1);
    let fence_start = "# T\n\n".len() as u32;
    assert_eq!(
        read.code_highlights.get(&fence_start).map(|h| h.len()),
        Some(1)
    );
}

#[test]
fn read_click_focuses_via_goto_at_the_clicked_byte() {
    let mut s = read_session();
    // The shell hit-tests a click on the first paragraph to its span start (byte 9 → line 2).
    let fx = s.read_click(9);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["kind"], json!("goto"));
    assert_eq!(params["motion"]["position"], json!({"line": 2, "col": 0}));
    assert_eq!(params["extend_selection"], json!(false));
}

/// A click that lands ON a link follows it like Enter — pointing at a target and clicking
/// should act: the arm Goto still rides along (so `Alt-Left`/`z` return to the link), plus
/// the link's action.
#[test]
fn read_click_activate_follows_a_link() {
    let mut s = read_session();
    // The link's span starts at byte 26 (line 4, after "See ").
    let fx = s.read_click_activate(26);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 4}));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y"
        )),
        "the external link opens like Enter"
    );
}

/// A click on a footnote reference jumps to its definition (two Gotos: the ref arms, the
/// definition is where reading continues — `z` steps back to the ref).
#[test]
fn read_click_activate_jumps_to_a_footnote_definition() {
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "A claim[^1].\n\n[^1]: The definition.\n" })),
    );
    // The ref's span starts at byte 7.
    let fx = s.read_click_activate(7);
    let gotos: Vec<_> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, params, .. } if *method == "cursor/move" => {
                    Some(params["motion"]["position"].clone())
                }
                _ => None,
            })
            .collect();
    assert_eq!(
        gotos,
        vec![json!({"line": 0, "col": 7}), json!({"line": 2, "col": 0})],
        "arm the ref, then land on the definition"
    );
}

/// A click on an image arms it and nothing more — Enter opens it externally, which a stray
/// click shouldn't.
#[test]
fn read_click_activate_on_an_image_arms_only() {
    let mut s = md_session();
    s.buffer.path = Some("/ws/docs/doc.md".into());
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "![d](../img.png)\n" })),
    );
    let fx = s.read_click_activate(0);
    let (_t, method, _params) = the_request(&fx);
    assert_eq!(method, "cursor/move");
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::ShellAction(_))),
        "no open action from a click"
    );
}

/// Ctrl-click on a relative-path link — the pointer sibling of `Ctrl-Enter`: a `NewWindow`
/// target for the resolved path; anything else falls back to the plain click-follow.
#[test]
fn read_click_new_window_opens_relative_links() {
    use aether_client::effect::{WindowOpen, WindowTarget};
    let mut s = md_session();
    s.buffer.path = Some("/ws/docs/doc.md".into());
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "[next](./other.md)\n" })),
    );
    let fx = s.read_click_new_window(0);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::NewWindow(WindowTarget {
                open: WindowOpen::Path { path, at: None },
                ..
            })) if path == "/ws/docs/./other.md"
        )),
        "Ctrl-click emits a new-window target for the resolved path"
    );

    // An external link falls back to the plain click-follow (open externally).
    let mut s = read_session();
    let fx = s.read_click_new_window(26);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y"
        )),
        "Ctrl-click on an external link follows like a plain click"
    );
}

/// Regression: a lone-link paragraph must not trap `k`. The Goto to the paragraph start derives
/// focus to the *link* (innermost element at that byte); stepping blocks anchors at the link's
/// containing paragraph, so the next `k` reaches the block above instead of re-targeting the
/// link's own paragraph forever.
#[test]
fn read_k_steps_past_a_lone_link_paragraph() {
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "buffer/content");
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "revision": 1,
            "text": "# Title\n\nFirst para.\n\n[docs](https://x.y)\n\nLast para.\n",
        })),
    );
    // Walk down: heading → First para (2,0) → the link paragraph, landing at its rest byte
    // AFTER the link (4,19) so the bar shows alone — `l` opts into the link — → Last para.
    for (line, col) in [(2u32, 0u32), (4, 19), (6, 0)] {
        let fx = key(&mut s, 'j');
        let (t, m, params) = the_request(&fx);
        assert_eq!(m, "cursor/move");
        assert_eq!(
            params["motion"]["position"],
            json!({"line": line, "col": col})
        );
        let _ = s.on_rpc_result(
            t,
            Ok(json!({
                "position": {"line": line, "col": col},
                "anchor": {"line": line, "col": col},
            })),
        );
    }
    // Back up: Last para → the link paragraph (bar alone — no auto-target)…
    let fx = key(&mut s, 'k');
    let (t, _m, params) = the_request(&fx);
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 19}));
    let _ = s.on_rpc_result(
        t,
        Ok(json!({"position": {"line": 4, "col": 19}, "anchor": {"line": 4, "col": 19}})),
    );
    {
        let read = s.read.as_ref().unwrap();
        let cursor = s.buffer.cursor.position;
        assert_eq!(read.target_focus(cursor), None, "no auto-selected link");
        assert!(
            read.block_focus(cursor).is_some(),
            "the bar marks the paragraph"
        );
    }
    // …and past it.
    let fx = key(&mut s, 'k');
    let (_t, _m, params) = the_request(&fx);
    assert_eq!(params["motion"]["position"], json!({"line": 2, "col": 0}));
}

/// `Enter` on a remote image opens the URL itself — not a fabricated buffer-relative path like
/// `/docs/https:/…`.
#[test]
fn read_enter_on_a_remote_image_opens_the_url() {
    let mut s = md_session();
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "revision": 1,
            "text": "![logo](https://x.y/logo.svg)\n",
        })),
    );
    // The lone image promotes to a block element; the boot cursor (0,0) focuses it.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y/logo.svg"
        )),
        "Enter on a remote image opens the URL"
    );
}

/// The two focus projections (docs/markdown-view.md §1.3): the block-grain reading position
/// (the bar) and the interactive-grain Enter target (the pill) both derive from the one server
/// cursor. A Tab-focused link keeps its containing paragraph as the position, with the link as
/// the target; stepping to a plain paragraph clears the target with no invalidation logic.
#[test]
fn focus_projections_compose_block_bar_and_link_target() {
    use aether_client::markdown::Element;
    let mut s = read_session();
    // Step to the link paragraph and into its link (`j` `j` `l`), adopting each cursor.
    focus_the_link(&mut s);
    let cursor = s.buffer.cursor.position;
    let read = s.read.as_ref().unwrap();
    let target = read
        .target_focus(cursor)
        .expect("cursor sits inside the link");
    assert!(matches!(read.elements[target], Element::Link { .. }));
    let block = read
        .block_focus(cursor)
        .expect("block position always present");
    assert!(matches!(read.elements[block], Element::Block { .. }));
    assert!(
        read.elements[block]
            .span()
            .contains(read.elements[target].span().start),
        "the bar sits on the link's containing paragraph"
    );
    // `k` to the first paragraph: the target clears (the cursor left the link's span).
    let fx = key(&mut s, 'k');
    let (t, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        t,
        Ok(json!({"position": {"line": 2, "col": 0}, "anchor": {"line": 2, "col": 0}})),
    );
    let cursor = s.buffer.cursor.position;
    let read = s.read.as_ref().unwrap();
    assert_eq!(
        read.target_focus(cursor),
        None,
        "target vanished with the cursor"
    );
    assert!(read.block_focus(cursor).is_some(), "the bar never vanishes");
}

/// `Ctrl-Enter` on a relative-path link: the picker's open-in-new-window at reading grain —
/// a `NewWindow` target carrying the resolved path (GUI spawns a window on it, the web opens
/// an app tab). On anything else (an external link here) it behaves exactly like `Enter`.
#[test]
fn read_ctrl_enter_opens_relative_links_in_a_new_window() {
    use aether_client::effect::{WindowOpen, WindowTarget};
    let mut s = md_session();
    s.buffer.path = Some("/ws/docs/doc.md".into());
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "[next](./other.md)\n" })),
    );
    // The boot cursor (0,0) sits inside the link — Ctrl-Enter opens it in a new window.
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::NewWindow(WindowTarget {
                open: WindowOpen::Path { path, at: None },
                ..
            })) if path == "/ws/docs/./other.md"
        )),
        "Ctrl-Enter emits a new-window target for the resolved path"
    );

    // An external link falls back to Enter behaviour (open externally).
    let mut s = read_session();
    focus_the_link(&mut s);
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y"
        )),
        "Ctrl-Enter on an external link opens it like Enter"
    );
}

/// `Enter` on a *local* image emits the buffer-file action: native shells open the absolute
/// path with the system handler; the web opens the confined `/asset/` route built
/// from `(buffer_id, relative)` — a browser can't open local paths.
#[test]
fn read_enter_on_a_local_image_emits_open_buffer_file() {
    let mut s = md_session();
    s.buffer.path = Some("/ws/docs/doc.md".into());
    let fx = leader(&mut s, 'v');
    let (token, _m, _p) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({ "revision": 1, "text": "![d](../img.png)\n" })),
    );
    let id = s.buffer.buffer_id;
    // The boot cursor (0,0) sits inside the image markup — armed; Enter opens.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None, ROWS);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenBufferFile { absolute, buffer_id, relative })
                if absolute == "/ws/docs/../img.png"
                    && *buffer_id == id
                    && relative == "../img.png"
        )),
        "local image Enter carries the absolute path and the asset-route pieces"
    );
}

/// The web shell's `view=read|source` URL param: an explicit boot presentation overrides both
/// the jump rules and the app default, so a refresh restores exactly what was on screen.
#[test]
fn explicit_boot_presentation_overrides_default_and_jump_rules() {
    use aether_client::session::Mode;
    let mut s = md_session();
    // view=source lands in the editor even though the read default is on.
    let fx = s.boot_read_presentation_explicit(false);
    assert_eq!(s.mode, Mode::Normal);
    assert!(s.read.is_none());
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));
    // view=read opens the reading view even with the default off.
    s.markdown_read_default = false;
    let fx = s.boot_read_presentation_explicit(true);
    assert_eq!(s.mode, Mode::Read);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "buffer/content");
}
