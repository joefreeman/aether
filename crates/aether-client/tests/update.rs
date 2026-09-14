//! The sans-IO payoff: the update loop tested as a pure state machine — key events in,
//! `Effect::Request`s out, canned JSON results back in — with no transport, no mock, no async
//! runtime.

use aether_client::effect::{Effect, Effects, ShellAction, ToastKind};
use aether_client::keymap::{KeyCode, Mods};
use aether_client::session::Session;
use aether_client::transport::RpcError;
use aether_protocol::ViewId;
use serde_json::json;

fn session() -> Session {
    Session::placeholder()
}

fn key(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::NONE, Some(c.to_string()))
}

fn ctrl(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::CTRL, None)
}

fn ctrl_alt(s: &mut Session, c: char) -> Effects {
    s.on_key(KeyCode::Char(c), Mods::CTRL_ALT, None)
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

/// The single `Effect::Request` in `fx` other than a `view/set_read` — what an edit transition
/// out of the reading view sends beside its ask to see the source.
fn the_request_beside_open(fx: &Effects) -> (u64, &'static str, serde_json::Value) {
    let mut reqs = fx.0.iter().filter_map(|e| match e {
        Effect::Request {
            token,
            method,
            params,
        } if *method != "view/set_read" => Some((*token, *method, params.clone())),
        _ => None,
    });
    let req = reqs
        .next()
        .expect("an Effect::Request beside the mode flip");
    assert!(
        reqs.next().is_none(),
        "exactly one request beside the mode flip expected"
    );
    req
}

/// The token of the (single) plain-save request in `fx`.
///
/// `view/save`, not `buffer/save`: a plain save saves the **view** — every document its elements
/// window, which for an ordinary view is the one document. Save-*as* still names a single file and
/// still goes through `buffer/save`.
fn save_token(fx: &Effects) -> u64 {
    fx.0.iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "view/save" => Some(*token),
            _ => None,
        })
        .expect("a view/save request was emitted")
}

/// A `view/save` reply that wrote `n` documents.
fn view_saved(n: u32) -> serde_json::Value {
    json!({ "saved": n, "focused": { "saved_at_unix_ms": 0, "revision": 4 } })
}

fn quits(fx: &Effects) -> bool {
    fx.0.iter().any(|e| matches!(e, Effect::Exit))
}

/// The full text of every toast an update produced — title, then body if it has one — for asserting
/// on wording that carries a concrete next step ("save first", "switch away first"). Flattened
/// because most assertions care that the words reached the user, not which line they landed on; use
/// [`toast_parts`] when the split itself is the point.
fn toast_messages(fx: &Effects) -> Vec<String> {
    toast_parts(fx)
        .into_iter()
        .map(|(title, body)| match body {
            Some(b) => format!("{title} — {b}"),
            None => title,
        })
        .collect()
}

/// Every toast's `(title, body)`, unflattened.
fn toast_parts(fx: &Effects) -> Vec<(String, Option<String>)> {
    fx.0.iter()
        .filter_map(|e| match e {
            Effect::Toast { title, body, .. } => Some((title.clone(), body.clone())),
            _ => None,
        })
        .collect()
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

/// The lifetime policy lives on the kind so all three shells read it from one place. Errors are the
/// ones that pin — they carry detail worth reading and are rare enough to hold the corner — and a
/// pinned toast still expires eventually so a forgotten one can't sit there forever.
#[test]
fn only_errors_pin_and_every_kind_outlives_a_glance() {
    assert!(ToastKind::Error.pinned());
    for kind in [ToastKind::Info, ToastKind::Success, ToastKind::Warning] {
        assert!(!kind.pinned(), "{kind:?} must not pin");
    }
    assert!(
        ToastKind::Error.ttl() > ToastKind::Warning.ttl(),
        "a pinned error's backstop outlasts a warning"
    );
    assert!(
        ToastKind::Warning.ttl() > ToastKind::Info.ttl(),
        "a warning is a sentence to finish; an info is read at a glance"
    );
    assert_eq!(ToastKind::Info.ttl(), ToastKind::Success.ttl());
}

/// A body built from a formatted error can come back blank (a server that failed with nothing to
/// say). Rendering that would leave a dangling empty line under the title, so it degrades to a
/// title-only toast.
#[test]
fn a_blank_toast_body_degrades_to_a_plain_toast() {
    assert_eq!(
        toast_parts(&Effects::error_detail("Save failed", "")),
        vec![("Save failed".to_string(), None)]
    );
    assert_eq!(
        toast_parts(&Effects::error_detail("Save failed", "   ")),
        vec![("Save failed".to_string(), None)]
    );
    assert_eq!(
        toast_parts(&Effects::error_detail("Save failed", "permission denied")),
        vec![(
            "Save failed".to_string(),
            Some("permission denied".to_string())
        )]
    );
}

#[test]
fn insert_entry_is_one_selection_edge_request() {
    let mut s = session();
    let fx = key(&mut s, 'i');
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);

    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
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
    assert_eq!(s.view.buffer.cursor.position.line, 2);
    assert_eq!(s.view.buffer.cursor.position.col, 5);
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

/// A window whose focus ring has exactly one stop: an input element.
///
/// `Tab` walks the things a view offers to act on, so a test that wants a focus request has to
/// give it something to walk to — a view with nothing actionable in it is one `Tab` correctly does
/// nothing in.
/// A conversation's shape: a box whose title carries a disclosure, around the element it folds.
///
/// The **button comes before the rows it belongs to**, and carries the same element id — which is
/// what made "where am I on the ring" ambiguous, and what this fixture exists to keep honest.
fn window_with_a_box(element: u32) -> aether_protocol::viewport::Window {
    let editor = |element: u32| aether_protocol::viewport::Element::Editor {
        collapsed: false,
        element,
        buffer: 10 + element as u64,
        rows: 3,
        first_row: aether_protocol::coords::ElementRow(0),
        laid_out_by: aether_protocol::ui::LayoutOwner::Server,
        role: aether_protocol::ui::ElementRole::Field,
        first_buffer_line: 0,
        lines: vec![],
    };
    aether_protocol::viewport::Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::column(vec![
            aether_protocol::viewport::Element::titled(
                aether_protocol::ui::Edges {
                    border: aether_protocol::ui::Sides::all(1),
                    padding: aether_protocol::ui::Sides::ZERO,
                    collapse: false,
                },
                aether_protocol::ui::Band::Chrome,
                vec![aether_protocol::viewport::Element::Action {
                    action: aether_protocol::ui::ViewAction::Expand { expand: None },
                    label: vec![aether_protocol::viewport::Element::text("▾", Vec::new())],
                    enabled: true,
                }],
                vec![editor(element)],
            ),
            editor(element + 1),
        ]),
    }
}

fn window_with_an_input(element: u32) -> aether_protocol::viewport::Window {
    aether_protocol::viewport::Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::column(vec![
            aether_protocol::viewport::Element::Editor {
                collapsed: false,
                element,
                buffer: 0,
                rows: 1,
                first_row: aether_protocol::coords::ElementRow(0),
                laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                role: aether_protocol::ui::ElementRole::Input,
                first_buffer_line: 0,
                lines: vec![],
            },
        ]),
    }
}

#[test]
fn goto_line_from_end_counts_up_from_the_bottom() {
    use aether_protocol::viewport::Window;
    let mut s = session();
    s.view.window = Some(Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::Editor {
            collapsed: false,
            element: 0,
            buffer: 0,
            rows: 0,
            first_row: aether_protocol::coords::ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            first_buffer_line: 0,
            lines: vec![],
        },
    });

    let alt_g = |s: &mut Session| -> serde_json::Value {
        let fx = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
        let (_, method, params) = the_request(&fx);
        assert_eq!(method, "element/move");
        params["motion"].clone()
    };

    // **Bare `Alt-g` asks for the field's end, and lets the server say where that is.** It used to
    // synthesise an absolute line from `view_line_count` — a *view* line count standing in for a
    // *buffer* line, which is only the same number while the view has one element. `buffer_end`
    // resolves against the motion scope, so in a composed view it lands on the focused hunk's last
    // line instead of a clamped guess.
    assert_eq!(alt_g(&mut s)["kind"], "buffer_end");

    // Counted, `3 Alt-g` is three lines up from the end — and the server counts, from the end of
    // the *field*: the client no longer knows a line count to subtract from, since a view's height
    // is rows of every element, not lines of one buffer.
    let _ = key(&mut s, '3');
    let counted = alt_g(&mut s);
    assert_eq!(counted["kind"], "line_from_end");
    assert_eq!(counted["count"].as_u64().unwrap(), 3);

    // And its mirror: bare `g` asks for the field's start rather than absolute line 0.
    let fx = s.on_key(KeyCode::Char('g'), Mods::NONE, Some("g".into()));
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["motion"]["kind"], "buffer_start");
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

/// A step that found its entry gone from the review says so and stays put — or shows the review
/// when it had to be brought back to look — and never opens the entry's file in an editor.
#[test]
fn a_gone_jumplist_entry_toasts_instead_of_opening_the_file() {
    use aether_client::update::Event;
    use aether_protocol::jumplist::{JumplistStepResult, JumplistStepScope};

    let mut s = session();
    let before = s.view.buffer.buffer_id;
    let fx = s.on_event(Event::JumplistStepped(
        Ok(JumplistStepResult::Gone {
            index: 2,
            total: 4,
            skipped: 1,
            opened: None,
        }),
        aether_protocol::cursor::Direction::Forward,
        JumplistStepScope::Full,
    ));
    assert_eq!(
        toast_messages(&fx),
        vec!["This entry is no longer in the review"]
    );
    assert!(
        fx.0.iter().all(|e| !matches!(e, Effect::Request { .. })),
        "nothing is opened"
    );
    assert_eq!(s.view.buffer.buffer_id, before);

    // Several passed over, and the review reopened to look: it is shown, and the toast counts.
    let mut s = session();
    let fx = s.on_event(Event::JumplistStepped(
        Ok(JumplistStepResult::Gone {
            index: 4,
            total: 4,
            skipped: 3,
            opened: Some(Box::new(aether_protocol::view::ViewOpenResult {
                view_id: aether_protocol::ViewId(9),
                scroll: None,
                transient: true,
                read: false,
                buffer: aether_protocol::view::BufferDescription {
                    buffer_id: 9,
                    language: None,
                    line_count: 40,
                    byte_count: 400,
                    revision: 1,
                    saved_revision: 1,
                    path: None,
                    scratch_number: None,
                    cursor: Default::default(),
                    lsp_server: None,
                    title: Some("Working changes".into()),
                    commit: None,
                    read_only: true,
                    is_patch: true,
                },
            })),
        }),
        aether_protocol::cursor::Direction::Forward,
        JumplistStepScope::Full,
    ));
    assert_eq!(s.view.buffer.buffer_id, 9, "the review is shown");
    assert_eq!(
        toast_messages(&fx),
        vec!["3 entries are no longer in the review"]
    );
}

/// A step that passed over gone entries on the way to one still there lands, and says how many.
#[test]
fn a_step_over_gone_entries_lands_and_counts_them() {
    use aether_client::update::Event;
    use aether_protocol::jumplist::{JumplistStepResult, JumplistStepScope, JumplistStepTarget};
    use aether_protocol::viewport::ViewSeat;

    let mut s = session();
    s.view.viewport_id = Some(7);
    let fx = s.on_event(Event::JumplistStepped(
        Ok(JumplistStepResult::Moved(Box::new(JumplistStepTarget {
            path: Some("/repo/a.rs".into()),
            view_id: None,
            position: Some(aether_protocol::LogicalPosition { line: 42, col: 0 }),
            anchor: None,
            index: 3,
            total: 4,
            opened: None,
            seat: Some(ViewSeat {
                element: 1,
                buffer_id: 5,
            }),
            skipped: 2,
        }))),
        aether_protocol::cursor::Direction::Forward,
        JumplistStepScope::Full,
    ));
    assert_eq!(
        toast_messages(&fx),
        vec!["Skipped 2 entries that are no longer in the review"]
    );
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Request { method, .. } if *method == "element/set")),
        "and it still seats"
    );
}

/// Enter on a jumplist row whose change is gone from its review toasts rather than opening the
/// row's file.
#[test]
fn selecting_a_gone_jumplist_row_toasts() {
    use aether_client::update::Event;
    use aether_protocol::picker::PickerSelectResult;

    let mut s = session();
    let fx = s.on_event(Event::PickerSelected {
        result: Ok(PickerSelectResult::Gone { open: None }),
    });
    assert_eq!(
        toast_messages(&fx),
        vec!["This entry is no longer in the review"]
    );
    assert!(
        fx.0.iter().all(|e| !matches!(e, Effect::Request { .. })),
        "nothing is opened"
    );
}

#[test]
fn shift_extends_hunk_and_diagnostic_navigation() {
    // Plain `c`/`d` collapse to the target (no extend on the wire); Shift grows the selection.
    let press = |c: char, mods: Mods| -> serde_json::Value {
        let mut s = session();
        s.view.viewport_id = Some(7);
        let fx = s.on_key(KeyCode::Char(c), mods, None);
        the_request(&fx).2
    };

    // `c` → view/navigate_change, no extend; `Shift-c` → extend: true.
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
        s.view.viewport_id = Some(7);
        let fx = s.on_key(KeyCode::Char('o'), mods, None);
        let (_, method, params) = the_request(&fx);
        assert_eq!(method, "view/navigate_change");
        params
    };
    let shift_alt = Mods {
        shift: true,
        ..Mods::ALT
    };
    // `o`/`Alt-o` step the view's outline — whatever kind of view it is, the view answers —
    // and `Shift-o`/`Shift-Alt-o` extend the selection (same step, extend flag set).
    assert_eq!(press(Mods::NONE)["grain"], json!("outline"));
    assert_eq!(press(Mods::NONE)["direction"], json!("next"));
    assert_eq!(press(Mods::NONE)["extend"], json!(null));
    assert_eq!(press(Mods::SHIFT)["extend"], json!(true));
    assert_eq!(press(Mods::ALT)["direction"], json!("previous"));
    assert_eq!(press(Mods::ALT)["extend"], json!(null));
    assert_eq!(press(shift_alt)["extend"], json!(true));
}

#[test]
fn shift_arrow_in_insert_mode_does_not_extend_selection() {
    // Insert mode never holds a selection, so Shift+Arrow must not extend one (unlike Normal mode,
    // where Shift extends — see `shift_extends_symbol_navigation`). It just moves the caret.
    let mut s = session();
    key(&mut s, 'i');
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);

    let fx = s.on_key(KeyCode::Right, Mods::SHIFT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
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
    s.view.buffer.buffer_id = 7;
    let same_buffer_open = json!({
        "buffer": 0,
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
    assert_eq!(s.view.buffer.cursor.position.line, 150);
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
    // A same-buffer move keeps the viewport binding rather than resubscribing.
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a same-buffer nav jump must not resubscribe"
    );

    // A jump into a DIFFERENT buffer still resubscribes (full switch).
    let mut s = session();
    s.view.buffer.buffer_id = 7;
    let other_open = json!({
        "buffer": 0,
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
    let params = find_request(&fx, "view/open").expect("goto-def opens the target buffer");
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
    let params = find_request(&fx, "view/open").expect("goto-def opens the target buffer");
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
    let params = find_request(&fx, "view/open").expect("goto-def opens the external buffer");
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
    s.view.buffer.buffer_id = 4;
    let same = json!({
        "buffer": 0,
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
    assert_eq!(s.view.buffer.cursor.position.line, 250);
    assert_eq!(reveal_style(&fx), Some(RevealStyle::Jump));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a same-buffer goto-def must not resubscribe"
    );

    // A definition in another file is still a full switch.
    let mut s = session();
    s.view.buffer.buffer_id = 4;
    let other = json!({
        "buffer": 0,
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
        true,
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
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert!(s.prompt.is_none(), "Esc closes the save-as prompt");
}

#[test]
fn save_as_completes_dir_and_files_then_saves_the_literal_path() {
    use aether_client::session::Prompt;
    use aether_client::update::{Event, PathEditorOwner};
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    // `Space Alt-s` opens the save-as prompt and fires a directory/list for the root (empty path).
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('s'), Mods::ALT, None);
    let params = find_request(&fx, "directory/list").expect("open fires a directory/list");
    assert_eq!(params["path"], json!("/p"));

    // The listing lands with a directory and a file — unlike the dir-scope chip, files are kept.
    let _ = s.on_event(Event::PathEditorListing {
        owner: PathEditorOwner::SaveAs,
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, None);
    let _ = s.save_as_set_input("existing.md".into());

    // Enter saves with the confirm flag unset.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None);
    // Saves in place (overwrite:false), and does NOT quit yet.
    let params = find_request(&fx, "view/save").expect("Space Alt-q saves first");
    assert_eq!(params["overwrite"], json!(false));
    assert!(!quits(&fx), "quit is deferred until the save succeeds");
    let token = save_token(&fx);

    // Save lands → now it quits.
    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    assert!(quits(&fx), "a successful save quits");
}

/// A failed save must not quit — `Space Alt-q` is save-*and*-quit, not quit-regardless.
#[test]
fn space_alt_q_does_not_quit_when_the_save_fails() {
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('q'), Mods::ALT, None);
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
    let params = find_request(&fx, "view/save").expect("the confirmed save retries");
    assert_eq!(params["overwrite"], json!(true));
    assert!(!quits(&fx), "no quit until the retry succeeds");
    let token = save_token(&fx);

    // Retry succeeds → now it quits.
    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, None);
    let _ = s.save_as_set_input("existing.md".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(s.prompt.is_none(), "Enter dismisses the confirm");
    assert!(
        find_request(&fx, "buffer/reload").is_none(),
        "Enter must not run the destructive action"
    );

    // `y` accepts → the action runs (reload forced).
    stage(&mut s);
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    assert!(s.prompt.is_none());
    let params = find_request(&fx, "buffer/reload").expect("`y` runs the confirmed action");
    assert_eq!(params["force"], json!(true));

    // `Y` (shifted) accepts too.
    stage(&mut s);
    let fx = s.on_key(KeyCode::Char('Y'), Mods::NONE, Some("Y".into()));
    assert!(
        find_request(&fx, "buffer/reload").is_some(),
        "`Y` also accepts"
    );
}

/// A `view/state` push moves the presented view's transient flag, and only that view's.
///
/// Transience is the view's, not the buffer's: a file's reader and its editor are kept or dropped
/// independently, so a push naming the sibling must leave this one alone.
#[test]
fn view_state_push_moves_only_the_presented_view() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::view::{ViewState, ViewStateParams};
    let mut s = session();
    s.view.view_id = aether_protocol::ViewId(7);
    s.view.view_transient = true;

    let push = |view_id: u64, transient: bool| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: ViewState::NAME.into(),
            params: serde_json::to_value(ViewStateParams {
                view_id: aether_protocol::ViewId(view_id),
                transient,
            })
            .unwrap(),
        })
    };

    // Another window's keep of the same file says nothing about this one.
    let _ = s.on_event(push(8, false));
    assert!(s.view.view_transient, "another view's flag is not ours");

    let _ = s.on_event(push(7, false));
    assert!(!s.view.view_transient, "our own promotion is adopted");
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
    s.view.buffer.buffer_id = 10;
    s.view.buffer.path = Some("/p/foo.md".into());
    s.view.buffer.label = "foo.md".into();

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
                path: path.map(Into::into),
            })
            .unwrap(),
        })
    };

    // Another client saved-as foo.md -> sub/bar.md: we follow, relabelling to the new rel path.
    let _ = s.on_event(push(Some("/p/sub/bar.md")));
    assert_eq!(s.view.buffer.path.as_deref(), Some("/p/sub/bar.md"));
    assert_eq!(s.view.buffer.label.name, "sub/bar.md");

    // An in-place save (same path) is a no-op for the label; a legacy push (no path) too.
    let _ = s.on_event(push(Some("/p/sub/bar.md")));
    assert_eq!(s.view.buffer.label.name, "sub/bar.md");
    let _ = s.on_event(push(None));
    assert_eq!(s.view.buffer.path.as_deref(), Some("/p/sub/bar.md"));
    assert_eq!(s.view.buffer.label.name, "sub/bar.md");
}

/// Focus crossing into another buffer rebinds what the view is *showing*, not what it *is*.
///
/// A patch is one view over a file per hunk: stepping to the next hunk can land in a different
/// file, and everything the view says about its content — label, path, read-only, revision — has to
/// follow. Its identity must not: closing is still closing the patch. Rebinding the id alone would
/// leave the old file's label over the new file's text.
#[test]
fn focusing_another_buffer_rebinds_the_view_content_but_not_its_identity() {
    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.view_id = ViewId(10);
    s.view.view_buffer = 10;
    s.view.buffer.buffer_id = 10;
    s.view.window = Some(window_with_an_input(3));

    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "view/focus_element");

    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "element": 3,
            "buffer": {
                "buffer_id": 42,
                "line_count": 12,
                "byte_count": 100,
                "revision": 5,
                "saved_revision": 5,
                "path": "/p/src/other.rs",
                "cursor": {"position": {"line": 4, "col": 0}, "anchor": {"line": 4, "col": 0}},
                "transient": false,
                "read_only": true,
                "is_patch": false,
            },
        })),
    );

    assert_eq!(s.view.focused_element, 3);
    assert_eq!(
        s.view.buffer.buffer_id, 42,
        "the view now shows the focused element's buffer"
    );
    assert_eq!(s.view.buffer.path.as_deref(), Some("/p/src/other.rs"));
    assert_eq!(s.view.buffer.revision, 5, "and that buffer's revision");
    assert!(
        s.view.buffer.read_only,
        "read-only travels too, or edits would be attempted against a blob"
    );
    assert_eq!(
        s.view.buffer.cursor.position.line, 4,
        "the cursor lands where the server put it"
    );
    assert_eq!(
        s.view.view_id,
        ViewId(10),
        "but the view is still the patch — closing must not close the file it is showing"
    );
}

/// A view's identity and the buffer it is editing are addressed separately.
///
/// They are equal for every view that exists today and diverge for a patch — one view over a file
/// per hunk. Closing must close the *patch*, not whichever file the cursor happens to be in; edits
/// must land in the file, not the patch. Constructed by hand because nothing produces a divergent
/// view until the driver does, which is exactly why the distinction needs pinning now: getting it
/// wrong is silent.
#[test]
fn view_scoped_and_buffer_scoped_operations_address_different_ids() {
    let mut s = session();
    // A patch view (id 10) whose focused element windows one of its files (id 42).
    s.view.view_id = ViewId(10);
    s.view.view_buffer = 10;
    s.view.buffer.buffer_id = 42;
    s.view.buffer.read_only = false;

    // A text operation addresses the buffer under the cursor.
    let fx = key(&mut s, 'x');
    let (_, method, params) = the_request(&fx);
    assert_eq!(
        params["buffer_id"], 42,
        "{method} acts on the file being edited, not on the patch"
    );

    // Closing (`Space x`) addresses the view.
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'x');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
    assert_eq!(
        params["view_id"], 10,
        "closing closes the view, not the file it happens to be showing"
    );
}

/// `Tab` moves **one stop at a time**, and never backwards.
///
/// A box's buttons carry the id of the element they act on and sit *before* its rows, so resolving
/// "where am I" as "the first stop at or after the focused element" put the cursor's position on
/// the disclosure *above* it: `Tab` from the first line of an element went back to the button, then
/// forward to the element again, and only then made progress. Walking the ring and asserting the
/// sequence is strictly increasing is the property that catches it, whatever the ring holds.
#[test]
fn tab_walks_the_ring_one_stop_at_a_time() {
    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.window = Some(window_with_a_box(0));
    // In the *text* of the boxed element — arrived by `j`, not by `Tab`.
    s.view.focused_element = 0;
    s.view.focus = aether_client::grid::Focus::Text;

    // Where the client's focus sits on the ring of the window it is showing, resolved afresh each
    // time: the ring borrows the window, and pressing a key needs the session back.
    let position = |s: &aether_client::session::Session| {
        let root = &s.view.window.as_ref().unwrap().root;
        s.view
            .focus
            .position(&aether_client::grid::focus_ring(root))
    };
    let here = {
        let root = &s.view.window.as_ref().unwrap().root;
        aether_client::grid::focus_ring(root)
            .iter()
            .position(|st| matches!(st, aether_client::grid::Stop::Element { element: 0 }))
            .expect("the boxed element is a stop")
    };

    // The very first press must go *forward* from where the cursor is.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/focus_element");
    let landed = params["target"]["element"].as_u64().unwrap() as u32;
    assert_eq!(
        position(&s),
        Some(here + 1),
        "`Tab` from an element's text did not step to the stop after it (landed on element {landed})"
    );

    // And back the way it came: `Shift-Tab` returns to the disclosure above, one stop.
    s.view.focus = aether_client::grid::Focus::Text;
    let _ = s.on_key(KeyCode::BackTab, Mods::NONE, None);
    assert_eq!(
        position(&s),
        Some(here - 1),
        "`Shift-Tab` from an element's text did not step to the stop before it"
    );
}

/// **Two presses step two stops**, even before the server has said anything.
///
/// `Tab` tells the server which element now holds the cursor and hears back a round trip later.
/// The client therefore cannot read "where am I" off the focused element — under key repeat it is
/// still the element two presses ago — so it reads the stop it last landed on. Naming that stop
/// rather than numbering it is what keeps this true across the window the focus reply arrives with.
#[test]
fn tab_steps_again_before_the_focus_reply_lands() {
    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.window = Some(window_with_a_box(0));
    s.view.focused_element = 0;
    s.view.focus = aether_client::grid::Focus::Text;

    let position = |s: &aether_client::session::Session| {
        let root = &s.view.window.as_ref().unwrap().root;
        s.view
            .focus
            .position(&aether_client::grid::focus_ring(root))
    };

    // From the ring's first stop — the box's disclosure — so there are two steps left in it.
    s.view.focus = {
        let root = &s.view.window.as_ref().unwrap().root;
        let ring = aether_client::grid::focus_ring(root);
        assert!(ring.len() >= 3, "unexpected ring: {ring:?}");
        aether_client::grid::focus_of(&ring, 0).expect("the disclosure is the first stop")
    };

    let _ = s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert_eq!(
        position(&s),
        Some(1),
        "the first press did not step one stop"
    );
    // No `view/focus_element` reply: the focused element is still where it was.
    let _ = s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert_eq!(
        position(&s),
        Some(2),
        "the second press stepped from the focused element again, not from where the first landed"
    );
}

/// A reveal follows the **stop**, not the cursor: a button lives in chrome, and a folded block has
/// no rows of its own, so scrolling to where the cursor is leaves `Tab` off screen. One answer in
/// the core, because all three shells were asking it and only the terminal asked it correctly.
#[test]
fn a_reveal_follows_the_button_tab_reached() {
    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.window = Some(window_with_a_box(0));
    s.view.focused_element = 0;
    let measured = aether_client::grid::Measured::default();

    // In the text: the cursor's own row, inside the box — below its title.
    s.view.focus = aether_client::grid::Focus::Text;
    s.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 0, col: 0 };
    let in_text = s.view.reveal_row(&measured);

    // On the disclosure: the title row the button rides, which is the box's top edge — above it.
    let on_fold = {
        let root = &s.view.window.as_ref().unwrap().root;
        let ring = aether_client::grid::focus_ring(root);
        aether_client::grid::focus_of(&ring, 0).expect("the disclosure is the first stop")
    };
    s.view.focus = on_fold;
    let on_button = s
        .view
        .reveal_row(&measured)
        .expect("the button is drawn somewhere");
    assert!(
        in_text.is_none_or(|row| on_button < row),
        "the reveal aimed at the cursor rather than at the button on the border above it"
    );
}

/// `Tab` asks the server to move focus to the next editor element; `Shift-Tab` the previous.
///
/// The request needs a viewport — focus is a property of a presentation, and there is nothing to
/// step through before the first window arrives.
#[test]
fn tab_steps_focus_between_editor_elements() {
    let mut s = session();

    // No viewport yet: nothing to focus within.
    s.view.window = Some(window_with_an_input(2));
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert!(no_request(&fx), "no viewport, no focus step");

    s.view.viewport_id = Some(7);
    // **Absolute, not a step.** `Tab` walks the view's *focus ring* — the buttons and inputs it
    // offers to act on — and the client knows that ring, because it has the window. The server is
    // told which element the landing belongs to, since that is what decides which buffer an edit
    // acts on; it is not asked to re-derive where "next" is.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/focus_element");
    assert_eq!(params["target"], json!({"to": "element", "element": 2}));
    assert_eq!(params["viewport_id"], 7);

    // And it stops at the ends rather than wrapping: one stop, so there is nowhere further.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert!(no_request(&fx), "`Tab` wrapped past the last stop");
    let fx = s.on_key(KeyCode::BackTab, Mods::NONE, None);
    assert!(no_request(&fx), "`Shift-Tab` wrapped past the first stop");

    // A click names an element outright — stepping towards it is not something a pointer can do.
    s.view.focused_element = 0;
    let fx = s.focus_clicked_element(3);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/focus_element");
    assert_eq!(params["target"], json!({"to": "element", "element": 3}));

    // Clicking inside the element already focused asks for nothing.
    s.view.focused_element = 3;
    assert!(no_request(&s.focus_clicked_element(3)));
}

/// A `lines_changed` push carries **whose** revision it is, and a push for another buffer must not
/// move this view's.
///
/// A view is several editors over several buffers, so only the buffer a push rendered has moved.
/// Filing someone else's revision here would make the next push for the view's *own* buffer look
/// stale — the failure that rules out redirecting a buffer id behind the client's back, and the
/// reason the id travels explicitly instead.
#[test]
fn a_lines_changed_push_for_another_buffer_leaves_our_revision_alone() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;

    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.buffer.revision = 3;

    let push = |buffer: u64, revision: u64| {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: ViewportLinesChanged::NAME.into(),
            params: json!({
                "viewport_id": 7,
                "buffer": buffer,
                "revision": revision,
                "window": {
                    "root": {"node": "editor", "element": 0, "buffer": buffer, "rows": 0,
                             "first_row": 0, "first_buffer_line": 0, "lines": []},
                    "max_line_width": 0,
                },
            }),
        })
    };

    // Another element's buffer moved. Ours did not.
    let _ = s.on_event(push(42, 99));
    assert_eq!(
        s.view.buffer.revision, 3,
        "another buffer's revision is not ours to adopt"
    );

    // Our own buffer's push still lands.
    let _ = s.on_event(push(0, 9));
    assert_eq!(s.view.buffer.revision, 9);
}

#[test]
fn lines_changed_push_adopts_the_server_cursor() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;
    use aether_protocol::LogicalPosition;

    let mut s = session();
    s.view.viewport_id = Some(7);

    let push = |cursor: serde_json::Value| {
        let mut params = json!({
            "viewport_id": 7,
            "buffer": 0,
            "revision": 9,
            "window": {
                "root": {"node": "editor", "element": 0, "buffer": 3, "rows": 6, "first_row": 0,
                         "first_buffer_line": 0, "lines": []},
                "max_line_width": 0,
            },
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
        s.view.buffer.cursor.position,
        LogicalPosition { line: 5, col: 2 },
        "the pushed cursor is adopted"
    );

    // No cursor on the push (nothing stored server-side): local state is kept.
    let _ = s.on_event(push(serde_json::Value::Null));
    assert_eq!(
        s.view.buffer.cursor.position,
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
        focus_run: None,
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
        collapsible: false,
        update: Some(update(Some(vec![]), 0)),
        truncated: false,
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
    // Request pipelining: the user types into a fresh picker before its `picker/view` response has
    // arrived. Typing claims the generation (the server adopts `picker/query`'s number), so the
    // response's carried snapshot — the slot's pre-reopen generation and its resumed (empty) query —
    // must not regress either: adopting them would clobber the typed query and orphan the query's
    // own push. (Pushes can't race the response itself — shells deliver server messages in wire
    // order — so pipelining is the one case this gates.)
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
        collapsible: false,
        update: None,
        truncated: false,
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(view),
    });
    {
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.query, "f", "typed query survives the late response");
        assert_eq!(
            p.generation, 1,
            "claimed generation survives the late response"
        );
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
        focus_run: None,
        center_on: None,
        explorer_peek_missing: false,
    };
    let _ = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: PickerUpdate::NAME.into(),
        params: serde_json::to_value(&push).unwrap(),
    }));
    let p = s.picker.as_ref().unwrap();
    assert_eq!(
        p.items.len(),
        2,
        "the query's push applies under the claimed generation"
    );
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
        focus_run: None,
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
        focus_run: None,
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
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()));
    assert_eq!(
        glob_open(&s),
        "",
        "the core must not key-edit the chip editor"
    );
    // The shell's value-sync entry point drives it.
    let _ = s.chip_editor_set_input("*.rs".into());
    assert_eq!(glob_open(&s), "*.rs");
    // Esc is a command the core owns: it closes the editor.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    assert!(s
        .picker
        .as_ref()
        .unwrap()
        .chips
        .iter()
        .any(|c| matches!(c, ChipValue::Word)));
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.picker.as_ref().unwrap().chip_selected, Some(0));
    // Typing while a chip is selected deselects it and lands the char in the query (append).
    let _ = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
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
    let fx = s.on_key(KeyCode::Char('.'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char('.'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
    assert!(
        s.picker.as_ref().unwrap().chip_editor.is_none(),
        "Alt-g must not open the glob editor without the path_filterable echo"
    );
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None);
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
        collapsible: false,
        update: None,
        truncated: false,
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(view),
    });
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
    assert!(
        s.picker.as_ref().unwrap().chip_editor.is_some(),
        "Alt-g opens the glob editor once the echo lands"
    );
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);

    // The pattern chips never apply to the Jumplist — its query is a fuzzy match over the
    // captured row text, not a content regex — regardless of the flag.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "Alt-w stays a no-op on the Jumplist picker"
    );
}

#[test]
fn workspace_symbols_picker_offers_path_chips_and_removal_requeries() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::WorkspaceSymbols, None, None, false, None);

    // Unlike the Jumplist there is no data gate — results are live and workspace-scoped, so
    // Alt-g opens the glob editor straight away, and typing live-previews through the query.
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
    assert!(s.picker.as_ref().unwrap().chip_editor.is_some());
    let fx = s.chip_editor_set_input("*.rs".into());
    let params = find_request(&fx, "picker/query").expect("the glob preview re-queries");
    assert_eq!(params["filters"]["globs"], json!(["*.rs"]));

    // Enter commits the chip.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert_eq!(s.picker.as_ref().unwrap().chips.len(), 1);

    // Left (at the empty query's start) selects the chip; Backspace removes it — and the
    // removal must reach the server as a re-query with the filter gone, not just reshape the
    // local chip row.
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None);
    let fx = s.on_key(KeyCode::Backspace, Mods::NONE, None);
    assert!(s.picker.as_ref().unwrap().chips.is_empty());
    let params = find_request(&fx, "picker/query").expect("chip removal re-queries");
    assert_eq!(params["filters"]["globs"], json!(null));

    // The pattern chips never apply — the LSP server ran the match, not our regex engine.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    assert!(s.picker.as_ref().unwrap().chips.is_empty());
}

#[test]
fn jumplist_chip_removal_requeries() {
    use aether_client::update::Event;
    use aether_protocol::picker::{PickerKind, PickerViewResult};
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::Jumplist, None, None, false, None);
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
        collapsible: false,
        update: None,
        truncated: false,
    };
    let _ = s.on_event(Event::PickerViewed {
        initial: true,
        result: Ok(view),
    });
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
    let _ = s.chip_editor_set_input("*.rs".into());
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert_eq!(s.picker.as_ref().unwrap().chips.len(), 1);

    // Regression: removal used to reshape the chip row without telling the server, leaving the
    // results filtered by a chip that was no longer showing.
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None);
    let fx = s.on_key(KeyCode::Backspace, Mods::NONE, None);
    assert!(s.picker.as_ref().unwrap().chips.is_empty());
    let params = find_request(&fx, "picker/query").expect("chip removal re-queries");
    assert_eq!(params["filters"]["globs"], json!(null));
}

#[test]
fn lsp_picker_centers_on_the_current_buffers_server() {
    use aether_protocol::lsp::LspServerRef;
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    s.view.buffer.lsp_server = Some(LspServerRef {
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
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    let fx = s.open_picker(PickerKind::Buffers, None, None, false, None);
    let params = find_request(&fx, "picker/view").expect("view picker opens via picker/view");
    // The view is anchored on the active buffer (matched by buffer_id), so it opens selected.
    assert_eq!(params["center_on"]["kind"], "buffer");
    assert_eq!(params["center_on"]["buffer_id"], 7);
}

/// In a composed view the picker centres on the **view**, not on whichever file the cursor is in.
///
/// The picker lists views, so a patch's row is the one to land on. Centring on `view.buffer` — the
/// focused element's file — highlighted a different row when that file happened to be open too, and
/// nothing at all when it wasn't.
#[test]
fn buffers_picker_centers_on_the_view_not_the_focused_element() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.view.view_id = ViewId(10);
    s.view.view_buffer = 10;
    s.view.buffer.buffer_id = 7;
    let fx = s.open_picker(PickerKind::Buffers, None, None, false, None);
    let params = find_request(&fx, "picker/view").expect("view picker opens via picker/view");
    assert_eq!(params["center_on"]["kind"], "buffer");
    assert_eq!(
        params["center_on"]["buffer_id"], 10,
        "the patch's row, not the hunk's file"
    );
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
fn space_y_opens_the_keybindings_picker_with_its_rows() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'y');
    let params = find_request(&fx, "picker/view").expect("Space y opens via picker/view");
    assert_eq!(params["kind"], "keybindings");
    assert_eq!(params["reset"], "all");
    // The rows ride the open: the keymap tables live client-side, the server only matches.
    let rows = params["keybindings"].as_array().expect("rows shipped");
    assert!(
        rows.len() > 50,
        "the whole keymap ships ({} rows)",
        rows.len()
    );
    assert!(rows.iter().any(|r| r["keys"] == "Space y"
        && r["desc"] == "Show keyboard shortcuts"
        && r["mode"] == "Application"));
    // The `Space g` sub-leader's rows carry their prefix in the label and list as Application
    // rows like any other leader chord.
    assert!(rows.iter().any(|r| r["keys"] == "Space g c"
        && r["desc"] == "Commit staged changes"
        && r["group"] == "Git"
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
fn alt_l_opens_the_highlighted_row_like_enter() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    let p = s.picker.as_mut().unwrap();
    p.items = (0..3)
        .map(|n| PickerItem::File {
            path_index: 0,
            relative_path: format!("src/f{n}.rs"),
            match_indices: vec![],
            git_status: None,
        })
        .collect();
    p.total_matches = 3;
    p.selected = 1;
    // Alt-h has no counterpart on a flat kind — you can't un-open — and it must not wipe the
    // query (that ladder is Alt-Backspace's). Checked first: Alt-l closes the picker below.
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().selected, 1);
    // A flat kind has no level below its rows, so "deeper" is the row itself: Alt-l resolves the
    // pick exactly as Enter does.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let params = find_request(&fx, "picker/select").expect("Alt-l selects the highlighted row");
    assert_eq!(params["kind"], "files");
    assert_eq!(params["item"]["relative_path"], json!("src/f1.rs"));
    assert!(
        s.picker.is_none(),
        "opening closes the picker, as Enter does"
    );
}

#[test]
fn alt_l_leaves_inert_rows_alone() {
    use aether_protocol::picker::{PickerItem, PickerKind};
    // Keybindings is a reference list: Enter deliberately doesn't fire the binding, so neither
    // does Alt-l. (It used to jump by group here — that meaning is gone.)
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
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    assert!(no_request(&fx), "a keybinding row isn't a jump target");
    assert_eq!(s.picker.as_ref().unwrap().selected, 4, "and nothing moves");
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    assert!(no_request(&fx));
}

#[test]
fn alt_l_declines_the_create_row() {
    use aether_protocol::picker::PickerKind;
    // "+ Create …" makes something on disk. Alt-l sits next to Alt-j/k, so an overshoot must not
    // create a file — that stays Enter's alone.
    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj".into());
        p.query = "novel.rs".into();
        p.items = vec![];
        p.total_matches = 0;
        p.selected = 0;
    }
    assert!(
        s.picker.as_ref().unwrap().selected_is_create(),
        "fixture should land on the synthetic create row",
    );
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().query, "novel.rs");
}

/// A collapsible picker window: a.rs collapsed with 2 hidden hits, b.rs expanded (the focused
/// group) with its 2 hits inline. Row space: [0]=a.rs hdr, [1]=b.rs hdr, [2,3]=hits.
fn grep_with_groups(s: &mut Session) {
    use aether_protocol::picker::{GroupHeader, GroupRunRows, GroupSpan, PickerItem, PickerKind};
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
    p.focus_run = Some(GroupRunRows {
        header_row: 1,
        len: 2,
    });
}

#[test]
fn alt_l_expands_the_highlighted_group_and_enters_it() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    // On a collapsed header: Alt-l expands the group and moves into it. The reply's geometry is
    // what seats the selection, so the row waits for the round trip.
    s.picker.as_mut().unwrap().selected = 0;
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("Alt-l expands the group");
    assert_eq!(params["kind"], "grep");
    assert_eq!(params["action"]["action"], "expand");
    assert_eq!(params["action"]["header"]["relative_path"], "a.rs");
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 0,
            len: 2,
        })),
        GroupLanding::RunStart,
    ));
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        1,
        "lands on the run's first item"
    );
    // On an already-open header the same request goes out — idempotent server-side, and the
    // reply is still what carries the run's geometry.
    {
        let p = s.picker.as_mut().unwrap();
        p.selected = 1;
        p.level = aether_client::picker::PickerLevel::Group;
    }
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("Alt-l re-enters an open group");
    assert_eq!(params["action"]["action"], "expand");
    assert_eq!(params["action"]["header"]["relative_path"], "b.rs");
}

#[test]
fn alt_l_on_a_group_item_opens_it() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    // Header → first item (the expand above), then one press further: an item row has no level
    // below it, so Alt-l opens the hit and the picker closes. It used to be a dead key here.
    s.picker.as_mut().unwrap().selected = 1;
    let _ = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 1,
            len: 2,
        })),
        GroupLanding::RunStart,
    ));
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let params = find_request(&fx, "picker/select").expect("Alt-l opens the hit");
    assert_eq!(params["kind"], "grep");
    assert_eq!(params["item"]["relative_path"], json!("b.rs"));
    assert_eq!(params["item"]["line"], json!(1), "the run's first hit");
    assert!(
        s.picker.is_none(),
        "opening closes the picker, as Enter does"
    );
}

#[test]
fn alt_h_collapses_the_group_and_never_touches_the_query() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    // On an item row: collapse the run the highlight is in and land back on its header. The
    // group is read off the window's spans, so it resolves even deep inside a long run.
    {
        let p = s.picker.as_mut().unwrap();
        p.selected = 3;
        p.level = aether_client::picker::PickerLevel::Item;
    }
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("Alt-h collapses the group");
    assert_eq!(params["action"]["action"], "collapse");
    assert_eq!(params["action"]["header"]["relative_path"], "b.rs");
    // The reply reports the now-empty run; the landing seats the highlight on its header.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 1,
            len: 0,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        1,
        "lands on the header"
    );
    // A collapse has no run to frame, so it reveals like any other row move.
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::RevealPickerSelection(aether_client::picker::Reveal::Minimal)
        )),
        "collapsing reveals minimally"
    );
    // On an already-collapsed header: as shallow as it goes — a no-op. Alt-h never wipes the
    // query (that's Alt-Backspace's).
    let p = s.picker.as_mut().unwrap();
    p.selected = 0;
    p.query = "needle".into();
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(
        s.picker.as_ref().unwrap().query,
        "needle",
        "the query survives Alt-h"
    );
    // Alt-Backspace is the unwind: clear the query — and never a group gesture.
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
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
    // Group level (selection on a header): Alt-j/k are a server-resolved group *step* — the
    // neighbour may sit past the fetched window.
    s.picker.as_mut().unwrap().selected = 1;
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("group-level Alt-j steps");
    assert_eq!(params["action"]["action"], "step");
    assert_eq!(params["action"]["direction"], "forward");
    assert_eq!(
        params["action"]["expand"], false,
        "walking headers leaves expansion alone"
    );
    // Resolve the gesture (a stop releases the single-flight guard at reply time — no
    // reshaping push follows a stop) so the next key isn't swallowed.
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::Header));
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("group-level Alt-k steps");
    assert_eq!(params["action"]["direction"], "backward");
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::Header));
    // Item level — entered by the *expand gesture* (Alt-l), which is what flips the stored
    // level bit; poking `selected` into the run alone must not (that's the held-key guard,
    // see `PickerLevel`). Local moves clamp to the run.
    let _ = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(aether_protocol::picker::GroupRunRows {
            header_row: 1,
            len: 2,
        })),
        GroupLanding::RunStart,
    ));
    s.picker.as_mut().unwrap().group_gesture_in_flight = false;
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 3);
    // At the run's last row Alt-j *spills* into the next group — an RPC, not a local
    // walk-out; the selection waits for the reply. A spill walks into the neighbour's items,
    // so it opens it (and leaves this run open). (Landings are exercised in
    // item_level_spills_across_group_edges.)
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("edge spill steps the group");
    assert_eq!(params["action"]["direction"], "forward");
    assert_eq!(
        params["action"]["expand"], true,
        "a spill opens the group it walks into"
    );
    assert_eq!(s.picker.as_ref().unwrap().selected, 3);
    let _ = s.on_event(Event::GroupSet(Ok(None), GroupLanding::RunStart)); // the very end: a stop
                                                                           // Back inside, Alt-k walks locally…
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    assert!(find_request(&fx, "picker/set_group").is_none());
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    // …and at the run's first row it spills backward.
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("upward spill steps back");
    assert_eq!(params["action"]["direction"], "backward");
}

/// `Alt-a` asks the server to toggle every group; the reply re-seats the highlight where it was —
/// the same offset into the focused run, or its header when the run shut under it.
#[test]
fn alt_a_toggles_every_group_and_keeps_the_selection() {
    use aether_client::picker::{GroupLanding, PickerLevel, Reveal};
    use aether_client::update::Event;
    use aether_protocol::picker::{GroupRunRows, PickerKind};
    let mut s = session();
    grep_with_groups(&mut s);
    // From inside b.rs's run (row 3 = its second item).
    {
        let p = s.picker.as_mut().unwrap();
        p.selected = 3;
        p.level = PickerLevel::Item;
    }
    let fx = s.on_key(KeyCode::Char('a'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("Alt-a toggles every group");
    assert_eq!(params["kind"], "grep");
    assert_eq!(params["action"], json!({ "action": "toggle_all" }));
    // Expanding everything pushes b.rs's run down past a.rs's two hits; the selection follows
    // its item, one row further into the run than the header.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 3,
            len: 2,
        })),
        GroupLanding::Keep { offset: Some(1) },
    ));
    assert_eq!(
        s.picker.as_ref().unwrap().selected,
        5,
        "same offset, new row"
    );
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::RevealPickerSelection(Reveal::Minimal))),
        "a toggle-all doesn't frame a run"
    );
    // Collapsing everything closes the run under the selection: it falls back to the header.
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 1,
            len: 0,
        })),
        GroupLanding::Keep { offset: Some(1) },
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 1);
    // On a header the offset is `None` — the highlight stays on the header wherever it moved to.
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 4,
            len: 3,
        })),
        GroupLanding::Keep { offset: None },
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 4);

    // Flat pickers have no groups to toggle: Alt-a is a dead key there (and must not type an `a`).
    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    s.picker.as_mut().unwrap().query = "needle".into();
    let fx = s.on_key(KeyCode::Char('a'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().query, "needle");
}

/// The held-`Alt-j` race (see `PickerLevel` / `group_gesture_in_flight`): a group step's
/// outcome arrives as two order-independent messages — the reply moves `selected`, the
/// reshaping push moves `focus_run`. A repeat firing between them used to derive "item
/// level" from the *new* selection row against the *stale* run interval and walk into the
/// run. Now repeats during a gesture are swallowed (single-flight, released by the push's
/// adoption), and the stored level bit backstops the routing either way.
#[test]
fn held_group_step_keeps_stepping_through_the_reply_push_gap() {
    use aether_client::picker::GroupLanding;
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    s.picker.as_mut().unwrap().selected = 1; // b.rs's header — group level
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert!(find_request(&fx, "picker/set_group").is_some());
    // A repeat while the gesture is mid-reshape is swallowed, not misrouted.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert!(no_request(&fx), "repeat during the gesture is swallowed");
    // The reply lands the next group's header row *in the incoming row space* (row 2), while
    // the stale local `focus_run` ({header_row: 1, len: 2}) still claims rows 2..=3 as
    // b.rs's items — the misclassifying pair (the reshaping push hasn't been adopted yet).
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 2,
            len: 4,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 2);
    // A repeat in the reply→push gap: still swallowed — and crucially NOT a local walk into
    // the stale interval.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().selected, 2, "no local walk");
    // The reshaping push adopts (fresh run + guard release): stepping resumes.
    {
        let p = s.picker.as_mut().unwrap();
        p.focus_run = Some(GroupRunRows {
            header_row: 2,
            len: 4,
        });
        p.group_gesture_in_flight = false;
    }
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("stepping resumes after adoption");
    assert_eq!(params["action"]["direction"], "forward");
}

/// Item-level `Alt-j`/`Alt-k` spill over the run's edges: down off the last item enters the next
/// group at its *first* item, up off the first enters the previous at its *last* — both staying at
/// item level, revealed minimally (a continuous scan, not a run framing).
#[test]
fn item_level_spills_across_group_edges() {
    use aether_client::picker::{GroupLanding, Reveal};
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    // Enter b.rs's run (header row 1, items 2..=3) and walk to its last item.
    s.picker.as_mut().unwrap().selected = 1;
    let _ = s.on_key(KeyCode::Char('l'), Mods::ALT, None); // expand + enter
    let _ = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 1,
            len: 2,
        })),
        GroupLanding::RunStart,
    )); // → 2
    s.picker.as_mut().unwrap().group_gesture_in_flight = false;
    let _ = s.on_key(KeyCode::Char('j'), Mods::ALT, None); // → 3 (last)
                                                           // Down off the last item: the same step RPC as group navigation — the landing intent
                                                           // stays client-side.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let params = find_request(&fx, "picker/set_group").expect("edge spill steps the group");
    assert_eq!(params["action"]["direction"], "forward");
    assert_eq!(params["action"]["expand"], true);
    // The reply carries the newly selected run's geometry; a RunStart landing enters at its
    // first item — item level, minimal reveal.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
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
        Ok(Some(GroupRunRows {
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
        p.focus_run = Some(GroupRunRows {
            header_row: 0,
            len: 4,
        });
        p.group_gesture_in_flight = false;
    }
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
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
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.picker.as_ref().unwrap().query, "needle");
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
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
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    let view = find_request(&fx, "picker/view").expect("ascends via picker/view");
    assert_eq!(view["directory_path"], json!("/proj/src"));
}

#[test]
fn enter_on_a_group_header_jumps_to_its_first_item() {
    let mut s = session();
    grep_with_groups(&mut s);
    // Enter on a header IS a jump: the Group row rides `picker/select` and the server resolves it
    // to the group's first item — so type-query-then-Enter takes the top hit without a mandatory
    // descend. The picker closes like any accept.
    s.picker.as_mut().unwrap().selected = 0;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(find_request(&fx, "picker/set_group").is_none());
    let params = find_request(&fx, "picker/select").expect("Enter selects the header");
    assert_eq!(params["item"]["kind"], "group");
    assert_eq!(params["item"]["header"]["relative_path"], "a.rs");
    assert!(find_request(&fx, "picker/hide").is_some());
    assert!(s.picker.is_none(), "accept closes the picker");
}

#[test]
fn clicking_a_group_header_toggles_it_instead_of_jumping() {
    use aether_client::update::Event;
    let mut s = session();
    grep_with_groups(&mut s);
    // A header click is the disclosure gesture: open a shut group, shut an open one — no select,
    // no close. The mouse path to a jump is clicking a visible item row.
    let fx = s.on_event(Event::PickerClicked(0));
    let params = find_request(&fx, "picker/set_group").expect("click discloses the group");
    assert_eq!(params["action"]["action"], "expand");
    assert_eq!(params["action"]["header"]["relative_path"], "a.rs");
    assert!(find_request(&fx, "picker/select").is_none());
    assert!(s.picker.is_some(), "the picker stays open");
    // b.rs is the open one, so clicking its header closes it.
    s.picker.as_mut().unwrap().group_gesture_in_flight = false;
    let fx = s.on_event(Event::PickerClicked(1));
    let params = find_request(&fx, "picker/set_group").expect("click discloses the group");
    assert_eq!(params["action"]["action"], "collapse");
    assert_eq!(params["action"]["header"]["relative_path"], "b.rs");
    // Clicking an item row accepts it, as ever.
    let fx = s.on_event(Event::PickerClicked(2));
    assert!(find_request(&fx, "picker/select").is_some());
}

#[test]
fn group_set_landing_seats_the_selection() {
    use aether_client::picker::{GroupLanding, Reveal};
    use aether_client::update::Event;
    use aether_protocol::picker::GroupRunRows;
    let mut s = session();
    grep_with_groups(&mut s);
    s.picker.as_mut().unwrap().selected = 3;
    // A Header landing seats the selection on the selected run's header row.
    let fx = s.on_event(Event::GroupSet(
        Ok(Some(GroupRunRows {
            header_row: 1,
            len: 2,
        })),
        GroupLanding::Header,
    ));
    assert_eq!(s.picker.as_ref().unwrap().selected, 1);
    assert!(no_request(&fx), "in-window: no refetch needed");
    // Group navigation frames the whole freshly-opened run, not just its header row — immediately
    // and re-armed for the reshaped push.
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
        Ok(Some(GroupRunRows {
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
        keys: "Space y".into(),
        match_indices: vec![],
    }];
    p.total_matches = 1;
    // Informational rows: Enter does nothing — the panel stays open, no hide, no `picker/select`.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(matches!(s.prompt, Some(Prompt::LspInfo(_))), "dialog opens");
    assert!(
        s.picker.is_some(),
        "the LSP picker stays open underneath the dialog"
    );
    // Closing the dialog (Esc) returns to the picker rather than the editor.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);

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
        focus_run: None,
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    // A terminal reports `?` with SHIFT held; the binding uses `IgnoreShift` so both that and the
    // GUI/web's already-resolved character hit it.
    let fx = s.on_key(KeyCode::Char('?'), Mods::SHIFT, Some("?".into()));
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
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('?'), Mods::SHIFT, Some("?".into()));
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
    let fx = s.on_key(KeyCode::Char('c'), Mods::CTRL, None);
    let copied = written_clipboard(&fx).expect("Ctrl-c copies");
    // The copied text is the rendered dialog, so a row can't exist in one and not the other.
    assert!(copied.contains("0.9.9") && copied.contains("dev") && copied.contains("Paths"));
    assert!(
        matches!(s.prompt, Some(Prompt::AppInfo(_))),
        "copying leaves the dialog up"
    );

    // A bare `c` is not the copy chord — it closes like any other key.
    let fx = s.on_key(KeyCode::Char('c'), Mods::NONE, Some("c".into()));
    assert!(s.prompt.is_none(), "any other key closes");
    assert!(written_clipboard(&fx).is_none());

    s.prompt = Some(Prompt::AppInfo(Some(Box::new(app_info()))));
    let fx = s.on_key(KeyCode::Char('q'), Mods::NONE, Some("q".into()));
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
    // Titled by what failed, with the server's own words as the detail line.
    assert_eq!(
        toast_parts(&fx),
        vec![(
            "App info failed".to_string(),
            Some("server gone".to_string())
        )]
    );
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
        views_open: 2,
        documents_unsaved: 0,
        workspaces_active: 1,
        git_version: Some("git version 2.43.0".into()),
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
    let fx = s.on_key(KeyCode::Char('r'), Mods::NONE, Some("r".into()));
    assert!(s.prompt.is_none(), "any non-Ctrl key closes the dialog");
    assert!(
        find_request(&fx, "lsp/restart_server").is_none(),
        "plain r no longer restarts"
    );

    // Ctrl-r restarts the server AND keeps the dialog open, showing Restarting immediately.
    s.prompt = Some(Prompt::LspInfo(status()));
    let fx = s.on_key(KeyCode::Char('r'), Mods::CTRL, None);
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
        Effect::Toast { title, group, .. } => Some((title.clone(), group.clone())),
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
    let fx = s.on_key(KeyCode::Char('r'), Mods::CTRL, None);
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
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::Editor {
            collapsed: false,
            element: 0,
            buffer: 0,
            rows: 0,
            first_row: aether_protocol::coords::ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            first_buffer_line: 0,
            lines: vec![],
        },
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
        Some(("Jumplist is empty".into(), Some("jumplist".into()))),
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
            "No jumplist entries in this file".into(),
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
    assert_eq!(
        s.view.mode,
        Mode::Normal,
        "insert is refused while connecting"
    );
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
    assert_eq!(s.view.mode, Mode::Insert);
    let _ = s.on_event(Event::ConnectionLost);
    assert_eq!(
        s.view.mode,
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
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Char('p'), Mods::ALT, None);
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
    s.view.buffer.path = Some("/p/src/main.rs".into());
    // `Space Alt-c`: the modal file-changes picker — its own kind, locked to the active buffer via
    // `buffer_id` (intrinsic, like Diagnostics), not a filter chip.
    let fx = s.open_picker(PickerKind::GitChangesFile, None, None, false, None);
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("git_changes_file"));
    assert_eq!(
        params["buffer_id"],
        json!(s.view.buffer.buffer_id),
        "locked to the active buffer"
    );
    assert!(
        params["filters"].is_null(),
        "no filter chips — the scope is intrinsic"
    );
}

/// A **file at a revision** is labelled by its path, with the commit it is shown at beside it —
/// two fields, so a shell paints the name in the body colour and the hash muted after it. The
/// plain-string surfaces (a window title, a confirm prompt) spell the pair out as one string.
///
/// A save-as replaces the label wholesale, which is what drops a stale hash: a file you wrote to
/// disk is a file, not a revision, and nothing can leave the old commit sitting beside its name.
#[test]
fn a_file_at_a_revision_labels_by_path_with_the_commit_beside_it() {
    use aether_client::session::buffer_info;
    use aether_protocol::view::BufferDescription;

    let roots = vec!["/p".to_string()];
    let info = buffer_info(
        serde_json::from_value::<BufferDescription>(json!({
            "buffer_id": 7,
            "line_count": 3,
            "byte_count": 20,
            "revision": 0,
            "saved_revision": 0,
            "path": null,
            "title": "src/main.rs",
            "commit": "abc1234",
            "read_only": true,
        }))
        .unwrap(),
        &roots,
    );
    assert_eq!(info.label.name, "src/main.rs");
    assert_eq!(info.label.commit.as_deref(), Some("abc1234"));
    assert_eq!(
        info.label.joined(),
        "src/main.rs (abc1234)",
        "one string where there is no second shade to paint in — brackets included, since there \
         is no colour there to say the revision annotates the name"
    );
    assert_eq!(info.label.commit_suffix().as_deref(), Some(" (abc1234)"));

    let mut s = session();
    s.view.buffer = info;
    s.view.view_buffer = s.view.buffer.buffer_id;
    s.view.view_label = s.view.buffer.label.clone();

    // A save-as relabels: the new name arrives on its own, and the revision goes with the old one.
    s.view
        .relabel_focused(aether_client::labels::Label::from("src/renamed.rs"));
    assert_eq!(s.view.buffer.label.commit, None);
    assert_eq!(s.view.view_label.joined(), "src/renamed.rs");
}

/// A virtual buffer (a revision materialised by `git/show`) labels itself with the server's title
/// rather than "(scratch)", and declines edits locally: the server refuses them anyway, so holding
/// a key down should be quiet rather than a stream of round trips. Insert mode is refused at the
/// door for the same reason.
#[test]
fn a_read_only_buffer_labels_by_title_and_declines_edits_locally() {
    use aether_client::session::buffer_info;
    use aether_protocol::view::BufferDescription;

    let roots = vec!["/p".to_string()];
    let info = buffer_info(
        serde_json::from_value::<BufferDescription>(json!({
            "buffer_id": 7,
            "language": null,
            "line_count": 3,
            "byte_count": 20,
            "revision": 0,
            "saved_revision": 0,
            "path": null,
            "title": "abc1234 — Add commit grammar",
            "read_only": true,
            "transient": true,
        }))
        .unwrap(),
        &roots,
    );
    assert_eq!(info.label.name, "abc1234 — Add commit grammar");
    assert_eq!(
        info.label.commit, None,
        "a commit's patch is named by the commit"
    );
    assert!(info.read_only);

    let mut s = session();
    s.view.buffer = info;

    // Motions still work — reading a diff means moving around in it.
    let fx = s.on_key(KeyCode::Char('j'), Mods::NONE, Some("j".into()));
    assert!(
        find_request(&fx, "element/move").is_some(),
        "navigation is unaffected"
    );

    // Edits are dropped with a warning rather than sent. The warning is grouped, so holding a key
    // down refreshes one toast in place instead of stacking a column of identical ones.
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None);
    assert!(no_request(&fx), "no edit RPC leaves the client");
    assert_eq!(
        first_toast(&fx),
        Some((
            "This view is read-only".to_string(),
            Some("read-only".to_string())
        ))
    );

    //...and `i` doesn't even change mode, so the next keystroke isn't text either.
    let fx = s.on_key(KeyCode::Char('i'), Mods::NONE, Some("i".into()));
    assert!(no_request(&fx));
    assert!(matches!(s.view.mode, aether_client::session::Mode::Normal));
    // ...on the same key as the edit refusal: `i` then a delete is one toast, not two.
    assert_eq!(
        first_toast(&fx).and_then(|(_, group)| group),
        Some("read-only".to_string())
    );

    // The gestures that don't reach the wire through `Session::edit` are covered too, because the
    // refusal keys off `RpcMethod::MUTATES_TEXT` in the request funnel rather than off the edit
    // helper's signature. `Ctrl-j`/`Ctrl-k` send `input/move_lines` directly and used to sail
    // straight past; `Ctrl-x` sends `buffer/cut`, whose result type the helper can't even name.
    for (key, mods, what) in [
        (KeyCode::Char('j'), Mods::CTRL, "element/move_lines down"),
        (KeyCode::Char('k'), Mods::CTRL, "element/move_lines up"),
        (KeyCode::Char('x'), Mods::CTRL, "buffer/cut"),
    ] {
        let fx = s.on_key(key, mods, None);
        assert!(no_request(&fx), "{what} left the client");
    }

    // The same gestures on a writable buffer do reach the wire — what's being asserted above is
    // the refusal, not three inert bindings.
    s.view.buffer.read_only = false;
    let fx = s.on_key(KeyCode::Char('j'), Mods::CTRL, None);
    assert!(find_request(&fx, "element/move_lines").is_some());
    let fx = s.on_key(KeyCode::Char('x'), Mods::CTRL, None);
    assert!(find_request(&fx, "buffer/cut").is_some());
}

/// Enter on a commit row opens it as a read-only virtual buffer: the row already carries the repo
/// and the hash, so nothing is re-resolved, and `git/show` returns a `view/open`-shaped result
/// the ordinary switch path adopts.
#[test]
fn enter_on_a_log_row_shows_the_commit() {
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::GitLog, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![PickerItem::GitCommit {
            path: None,
            repo_id: "/p".into(),
            hash: "abc1234def5678".into(),
            short_hash: "abc1234".into(),
            subject: "Add commit grammar".into(),
            decorations: Vec::new(),
            match_indices: Vec::new(),
            hash_match_len: 0,
        }];
        p.selected = 0;
    }
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let params = find_request(&fx, "git/show").expect("Enter shows the commit");
    assert_eq!(params["repo_id"], json!("/p"));
    // The target is tagged: a commit's whole diff, not one file within it.
    assert_eq!(params["target"]["kind"], json!("commit"));
    assert_eq!(params["target"]["rev"], json!("abc1234def5678"));
    assert!(params["target"].get("path").is_none());
    // The picker closes onto the diff, like the branch picker's checkout.
    assert!(find_request(&fx, "picker/hide").is_some());
    // And the view being left is recorded, so `Backspace` from the diff returns here — the
    // view's own buffer, as `Enter` in a review records the review.
    assert_eq!(
        params["record_nav_from"],
        json!(s.view.view_buffer),
        "a commit shown from the log is a jump with an origin"
    );
}

/// `Space g w` is a jump too: the working changes opened over a file record that file, so
/// `Backspace` from the review returns to it. Recorded once, as the view's own buffer — from a
/// review over the same file it would be the review.
#[test]
fn space_g_w_records_where_it_was_asked_from() {
    let mut s = session();
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = key(&mut s, 'w');
    let params = find_request(&fx, "git/show").expect("Space g w shows the working changes");
    assert_eq!(params["target"]["kind"], json!("working_changes"));
    assert_eq!(params["record_nav_from"], json!(7));
}

/// The stash picker's row actions: Enter previews the entry (a stash is a commit, so it goes
/// through `git/show` exactly as a log row does), `Ctrl-p` pops it, `Ctrl-Alt-p` applies without
/// dropping, and `Ctrl-d` asks first — a dropped stash is the one stash action the editor can't
/// give back.
#[test]
fn stash_picker_rows_preview_pop_apply_and_confirm_a_drop() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{PickerItem, PickerKind};

    let row = || PickerItem::GitStash {
        repo_id: "/p".into(),
        index: 0,
        oid: "abc1234def".into(),
        message: "WIP on main: 1234567 subject".into(),
        timestamp: 1_700_000_000,
        match_indices: Vec::new(),
    };
    let open = |s: &mut aether_client::session::Session| {
        let _ = s.open_picker(PickerKind::GitStash, None, None, false, None);
        let p = s.picker.as_mut().unwrap();
        p.items = vec![row()];
        p.selected = 0;
    };

    // Enter previews.
    let mut s = session();
    open(&mut s);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let params = find_request(&fx, "git/show").expect("Enter previews the entry");
    assert_eq!(params["target"]["kind"], json!("commit"));
    assert_eq!(params["target"]["rev"], json!("abc1234def"));

    // Ctrl-p pops; Ctrl-Alt-p applies without dropping.
    let mut s = session();
    open(&mut s);
    let fx = s.on_key(KeyCode::Char('p'), Mods::CTRL, None);
    let params = find_request(&fx, "git/stash_apply").expect("Ctrl-p pops");
    assert_eq!(params["oid"], json!("abc1234def"));
    assert_eq!(params["pop"], json!(true));

    let mut s = session();
    open(&mut s);
    let fx = s.on_key(KeyCode::Char('p'), Mods::CTRL_ALT, None);
    let params = find_request(&fx, "git/stash_apply").expect("Ctrl-Alt-p applies");
    assert!(
        params.get("pop").is_none(),
        "apply leaves the entry in place"
    );

    // Ctrl-d asks before discarding, and the action carries the row's identity so a moved
    // highlight can't redirect it.
    let mut s = session();
    open(&mut s);
    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);
    assert!(
        no_request(&fx),
        "nothing fires until the confirm is accepted"
    );
    assert!(matches!(
        &s.prompt,
        Some(Prompt::Confirm {
            kind: ConfirmKind::DropStash { message },
            ..
        }) if message.contains("WIP on main")
    ));
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    let params = find_request(&fx, "git/stash_drop").expect("accepting drops it");
    assert_eq!(params["oid"], json!("abc1234def"));
}

/// `Space c`: the workspace changes picker lists every root, whatever repos they span, so it needs
/// no repo-resolution hint — it sends the buffer only as the *centring* target, to land on the hunk
/// nearest the cursor.
#[test]
fn space_c_centres_on_the_cursor_without_a_resolution_hint() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    s.view.buffer.path = Some("/p/src/main.rs".into());
    let fx = s.open_picker(PickerKind::GitChanges, None, None, false, None);
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("git_changes"));
    assert_eq!(params["center_on_cursor"], json!(s.view.buffer.buffer_id));
    assert!(
        params["buffer_id"].is_null(),
        "nothing to resolve: the list is the workspace's, not a repo's"
    );
}

#[test]
fn space_alt_f_seeds_a_removable_directory_chip() {
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    s.view.buffer.path = Some("/p/src/main.rs".into());
    // `Space Alt-f`: Files pre-scoped to the buffer's directory as an ordinary, composable dir chip.
    let fx = s.open_files_in_file_dir();
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
    s.view.buffer.path = None; // scratch buffer — no directory to scope to
    let fx = s.open_files_in_file_dir();
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert!(
        params["filters"].is_null(),
        "a scratch buffer opens the whole workspace"
    );
}

#[test]
fn space_alt_slash_opens_grep_from_selection() {
    // `Space Alt-/`: open Grep asking the server to seed the query from the buffer's selection.
    // The client carries no selection text — it just sets `from_selection` + the buffer id and
    // lets the server slice + search (the query/generation ride back via the `PickerViewed` echo).
    let mut s = session();
    s.workspace_paths = vec!["/p".into()];
    s.view.buffer.path = Some("/p/src/main.rs".into());
    let fx = s.open_grep_from_selection();
    let params = find_request(&fx, "picker/view").expect("opens the picker");
    assert_eq!(params["kind"], json!("grep"));
    assert_eq!(params["from_selection"], json!(true));
    assert_eq!(
        params["buffer_id"],
        json!(s.view.buffer.buffer_id),
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
    assert_eq!(s.view.mode, Mode::Search);
    // A typed char reaching the core must NOT edit the query — text is the shell's input's job.
    let _ = key(&mut s, 'a');
    assert_eq!(
        s.view.search.query, "",
        "the core must not key-edit the search query"
    );
    // The shell's value-sync entry point drives it and re-runs the incremental search.
    let _ = s.search_set_query("ab".into());
    assert_eq!(s.view.search.query, "ab");
    // Esc is a command the core owns: it aborts search.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert_eq!(s.view.mode, Mode::Normal, "Esc aborts search");
}

/// Alt-Backspace is the one editing key the core owns in the search bar: word-grain delete, the
/// same unit the pickers and the buffer use. It goes through the query setter, so the search
/// re-runs against the shortened pattern.
#[test]
fn search_alt_backspace_drops_one_query_word() {
    use aether_client::keymap::Mods;
    let mut s = session();
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("fn parse".into());

    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(s.view.search.query, "fn ");
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params["query"], json!("fn "));

    let _ = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(s.view.search.query, "");
    // Nothing left to take, and no ladder behind the search bar — its option chips have their own
    // toggle chords.
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert!(no_request(&fx));
    assert_eq!(s.view.search.query, "");
}

#[test]
fn search_option_toggles_cycle_and_ride_the_request() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::CaseMode;
    let mut s = session();
    let _ = key(&mut s, '/'); // enter search
    let _ = s.search_set_query("foo".into());

    // Alt-e toggles regex; the new query goes back out with the options in the params.
    let fx = s.on_key(KeyCode::Char('e'), Mods::ALT, None);
    assert!(s.view.search.options.regex, "Alt-e enables regex");
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params["options"], json!({"regex": true}));

    // Alt-w toggles whole-word; Alt-c cycles smart -> sensitive -> insensitive -> smart.
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    assert!(s.view.search.options.whole_word);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    assert_eq!(s.view.search.options.case, CaseMode::Sensitive);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    assert_eq!(s.view.search.options.case, CaseMode::Insensitive);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    assert_eq!(
        s.view.search.options.case,
        CaseMode::Smart,
        "third Alt-c returns to smart"
    );

    // Esc restores the pre-prompt options (a cancelled search reverts its toggles too).
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert_eq!(
        s.view.search.options,
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
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None);
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(s.view.search.active);
    assert!(s.view.search.options.regex && s.view.search.options.case == CaseMode::Sensitive);

    // Re-opening the prompt starts clean: no leftover regex to silently change what the next
    // query matches, and no chips rendered above it.
    let _ = key(&mut s, '/');
    assert_eq!(s.view.search.options, MatchOptions::default());
    assert_eq!(s.view.search.query, "");
    assert!(s.view.search.option_chips().is_empty());

    // The next search runs literally, without inheriting anything.
    let fx = s.search_set_query("fn".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "search/set");
    assert_eq!(params.get("options"), None, "all-default options, skipped");

    // Esc puts the previous search back exactly as it was — query, active flag and options.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert_eq!(s.view.search.query, "fn \\w+");
    assert!(s.view.search.active);
    assert!(s.view.search.options.regex && s.view.search.options.case == CaseMode::Sensitive);
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
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None); // regex
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None); // whole-word
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);

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
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    let _ = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    assert_eq!(s.view.search.option_chips().len(), 2);
    assert_eq!(s.view.search.chip_selected, None);

    // Left at the query start steps into the row, selecting the rightmost (word) chip; Left again
    // walks to the case chip; Right walks back.
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.view.search.chip_selected, Some(1));
    let _ = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.view.search.chip_selected, Some(0));
    let _ = s.on_key(KeyCode::Right, Mods::NONE, None);
    assert_eq!(s.view.search.chip_selected, Some(1));

    // Enter on the word chip toggles it off — the chip vanishes, selection clamps onto the case chip.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(!s.view.search.options.whole_word);
    assert_eq!(s.view.search.option_chips().len(), 1);
    assert_eq!(s.view.search.chip_selected, Some(0));

    // Enter on the case chip cycles it (sensitive → insensitive); it stays present and selected.
    let _ = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert_eq!(s.view.search.options.case, CaseMode::Insensitive);
    assert_eq!(s.view.search.chip_selected, Some(0));

    // Backspace removes the selected case chip; the row empties and selection clears.
    let _ = s.on_key(KeyCode::Backspace, Mods::NONE, None);
    assert_eq!(s.view.search.options.case, CaseMode::Smart);
    assert!(s.view.search.option_chips().is_empty());
    assert_eq!(s.view.search.chip_selected, None);

    // Esc with no chip selected aborts search as usual.
    let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert_eq!(s.view.mode, aether_client::session::Mode::Normal);
}

#[test]
fn count_prefix_rides_the_request() {
    let mut s = session();
    let _ = key(&mut s, '3');
    // Ctrl-g = join lines; the count lives in the params, not a client loop.
    let fx = ctrl(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/join_lines");
    assert_eq!(params["count"], json!(3));
}

/// `100 Alt-j` sends the count the user typed, under the variant whose count the server refuses
/// when it cannot honour it — the other half of the `100 j` / `100 Alt-j` pair.
///
/// `v` is what made these two differ: it borrowed this variant to carry a row span the shell
/// derived from its own height, so the field could not mean "a count someone typed" and the server
/// had to clamp both. The page motion has its own variant now (`read_v_rides_the_editor_half_page_motion`).
#[test]
fn a_counted_visual_row_motion_sends_the_typed_count() {
    let mut s = session();
    s.view.viewport_id = Some(7);
    for c in "100".chars() {
        let _ = key(&mut s, c);
    }
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["kind"], json!("visual_line"));
    assert_eq!(params["motion"]["count"], json!(100));

    // Uncounted, the same key is a bare step — which the server clamps rather than refuses.
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["motion"]["count"], json!(1));
}

#[test]
fn ctrl_alt_g_unjoins_in_both_modes() {
    // Join's dual: `Ctrl-Alt-g` un-joins — the break lands at the cursor and the cursor stays
    // before it (`park_before`), so a following join re-joins the same pair. From the Global
    // table, so it works in Normal mode as well as Insert.
    let mut s = session();
    let fx = ctrl_alt(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/newline_and_indent");
    assert_eq!(params["park_before"], json!(true));

    let _ = key(&mut s, 'i');
    let fx = ctrl_alt(&mut s, 'g');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/newline_and_indent");
    assert_eq!(params["park_before"], json!(true));
}

#[test]
fn enter_is_newline_and_indent_in_insert() {
    let mut s = session();
    let _ = key(&mut s, 'i');
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/newline_and_indent");
    // Enter advances onto the new line — no parking.
    assert!(params.get("park_before").is_none());
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);
}

#[test]
fn paste_text_routes_by_mode() {
    // Insert: plain insert at the caret, exactly like the Ctrl-v gesture.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let fx = s.paste_text("one\ntwo".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/text");
    assert_eq!(params["text"], json!("one\ntwo"));
    assert_eq!(params["select_pasted"], json!(false));
    assert!(params.get("at").is_none());

    // Normal: paste before the selection, selecting the pasted text.
    let mut s = session();
    let fx = s.paste_text("one\ntwo".into());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/text");
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
    assert_eq!(method, "element/undo");
    assert!(params.get("count").is_none(), "count 1 stays off the wire");

    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "applied": true,
            "revision": 7,
            "cursor": {"position": {"line": 1, "col": 0}, "anchor": {"line": 1, "col": 0}},
        })),
    );
    assert_eq!(s.view.buffer.revision, 7);
    assert_eq!(s.view.buffer.cursor.position.line, 1);
}

#[test]
fn symbol_highlight_follow_is_subscription_shaped() {
    use aether_protocol::lsp::LspServerRef;
    let mut s = session();
    s.view.buffer.buffer_id = 1;
    s.view.buffer.lsp_server = Some(LspServerRef {
        language: "rust".into(),
        workspace_root: "/p".into(),
    });

    // The first step in an LSP-backed Normal-mode buffer subscribes exactly once…
    let fx = key(&mut s, 'j');
    let params = find_request(&fx, "lsp/document_highlight").expect("first step subscribes");
    assert_eq!(params["active"], true);

    // …and further moves and edits send nothing: the server re-arms the debounced refresh from
    // its own cursor updates (every edit lands there too, which also covers "the server dropped
    // the stale set on mutation" — the old per-move/per-edit re-request is gone).
    let fx = key(&mut s, 'j');
    assert!(
        find_request(&fx, "lsp/document_highlight").is_none(),
        "no per-move request"
    );
    let fx = ctrl(&mut s, 'y'); // toggle comment: an edit without cursor motion
    assert!(
        find_request(&fx, "lsp/document_highlight").is_none(),
        "no per-edit request"
    );

    // Entering Insert unsubscribes (stale highlights must not linger)…
    let fx = key(&mut s, 'i');
    let params = find_request(&fx, "lsp/document_highlight").expect("Insert unsubscribes");
    assert_eq!(params["active"], false);
    // …and returning to Normal re-subscribes.
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    let params = find_request(&fx, "lsp/document_highlight").expect("Normal re-subscribes");
    assert_eq!(params["active"], true);
}

#[test]
fn blame_follow_tracks_mode_transitions_only() {
    let mut s = session();
    s.view.buffer.buffer_id = 1;
    s.view.buffer.path = Some("/p/a.rs".into());

    // Landing in a file-backed Normal-mode buffer enables blame follow once…
    let fx = key(&mut s, 'j');
    let params = find_request(&fx, "git/set_blame_follow").expect("first step follows");
    assert_eq!(params["enabled"], true);

    // …moves send nothing (the server watches its own cursor)…
    let fx = key(&mut s, 'j');
    assert!(
        find_request(&fx, "git/set_blame_follow").is_none(),
        "no per-move request"
    );

    // …Insert unfollows (typing must not thrash server-side blame recomputes)…
    let fx = key(&mut s, 'i');
    let params = find_request(&fx, "git/set_blame_follow").expect("Insert unfollows");
    assert_eq!(params["enabled"], false);

    // …and Normal re-follows.
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    let params = find_request(&fx, "git/set_blame_follow").expect("Normal re-follows");
    assert_eq!(params["enabled"], true);
}

#[test]
fn rpc_error_surfaces_as_an_error_toast() {
    let mut s = session();
    let fx = ctrl(&mut s, 'z');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "element/undo",
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
            method: "element/undo",
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

    //...but client-only actions still run, so the user can always quit (`Space q` → Exit).
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
    s.view.mode = aether_client::session::Mode::Normal; // back out without a round-trip
    let fx = ctrl(&mut s, 'z');
    let (t2, _, _) = the_request(&fx);
    assert!(t2 > t1, "tokens are allocated in emission order");
}

#[test]
fn a_jumplist_change_elsewhere_re_views_an_open_jumplist_picker() {
    // Another client in the context captured or cleared. The push carries nothing: the client
    // re-views, because a capture can flip the list between grouped and flat and that gate rides
    // the view response. Selection goes back to the top — it's a different list now.
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::jumplist::JumplistChanged;
    use aether_protocol::picker::PickerKind;

    let changed = || {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: JumplistChanged::NAME.into(),
            params: serde_json::json!({}),
        })
    };

    let mut s = session();
    let _ = s.open_picker(PickerKind::Jumplist, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.loaded = true;
        p.selected = 7;
        p.offset = 5;
    }
    let fx = s.on_event(changed());
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "picker/view");
    assert_eq!(params["kind"], "jumplist");
    assert_eq!(params["offset"], 0, "re-views from the top of the new list");
    assert_eq!(
        params["reset"], "keep",
        "the typed query survives — it filters whatever the list is now"
    );
    assert_eq!(s.picker.as_ref().unwrap().selected, 0);

    // A different picker open, or none at all: nothing to refresh, no traffic.
    let _ = s.close_picker();
    let _ = s.open_picker(PickerKind::Buffers, None, None, false, None);
    assert!(no_request(&s.on_event(changed())));
    let _ = s.close_picker();
    assert!(no_request(&s.on_event(changed())));
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
fn clearing_the_jumplist_adopts_the_undecorated_cursor_and_toasts() {
    // `Space Alt-j`. The response carries the caller's cursor with the `k/N` stamp already gone —
    // adopting it is what makes the status segment disappear on the clear rather than on the next
    // keystroke. Nothing captured is a keyed Info toast, not an error.
    use aether_client::update::Event;
    use aether_protocol::cursor::{CursorState, JumplistPosition};
    use aether_protocol::jumplist::JumplistClearResult;
    use aether_protocol::LogicalPosition;

    let mut s = session();
    s.view.buffer.cursor = CursorState {
        position: LogicalPosition { line: 4, col: 9 },
        anchor: LogicalPosition { line: 4, col: 2 },
        jumplist_position: Some(JumplistPosition {
            current: 3,
            total: 17,
        }),
        ..Default::default()
    };

    // The chord: Space arms the leader, Alt-j discards. The buffer rides along so the response
    // can bring back a cursor to adopt.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "jumplist/clear");
    assert_eq!(params["buffer_id"], s.view.buffer.buffer_id);

    let fx = s.on_event(Event::JumplistCleared(Ok(JumplistClearResult {
        cleared: 17,
        cursor: Some(CursorState {
            position: LogicalPosition { line: 4, col: 9 },
            anchor: LogicalPosition { line: 4, col: 2 },
            ..Default::default()
        }),
    })));
    assert_eq!(
        s.view.buffer.cursor.jumplist_position, None,
        "the status counter goes with the list"
    );
    assert_eq!(
        s.view.buffer.cursor.position,
        LogicalPosition { line: 4, col: 9 },
        "…and the cursor itself is where it was"
    );
    assert_eq!(
        first_toast(&fx),
        Some((
            "Cleared 17 results from the jumplist".into(),
            Some("jumplist".into())
        )),
    );

    // Singular, and the already-empty case — same key, so mashing the chord coalesces.
    let fx = s.on_event(Event::JumplistCleared(Ok(JumplistClearResult {
        cleared: 1,
        cursor: None,
    })));
    assert_eq!(
        first_toast(&fx),
        Some((
            "Cleared 1 result from the jumplist".into(),
            Some("jumplist".into())
        )),
    );
    let fx = s.on_event(Event::JumplistCleared(Ok(JumplistClearResult {
        cleared: 0,
        cursor: None,
    })));
    assert_eq!(
        first_toast(&fx),
        Some(("Jumplist is already empty".into(), Some("jumplist".into()))),
    );
}

#[test]
fn jumplist_step_adopts_the_opened_entry() {
    // A `jumplist/step` composite (`]`/`[`) returns the target entry already opened; the client
    // adopts it exactly like a picker selection — cross-buffer targets switch the session's
    // buffer, and the status counter rides the opened cursor's `jumplist_position` stamp rather
    // than any client-held state.
    use aether_client::update::Event;
    use aether_protocol::cursor::{Direction, JumplistPosition};
    use aether_protocol::jumplist::{JumplistStepResult, JumplistStepTarget};
    use aether_protocol::view::{BufferDescription, ViewOpenResult};
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
    let open = ViewOpenResult {
        view_id: aether_protocol::ViewId(7),
        scroll: None,
        transient: true,
        read: false,
        buffer: BufferDescription {
            buffer_id: 7,
            language: None,
            line_count: 20,
            byte_count: 100,
            revision: 0,
            saved_revision: 0,
            path: Some("/proj/b.rs".into()),
            scratch_number: None,
            cursor,
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
    };
    let _ = s.on_event(Event::JumplistStepped(
        Ok(JumplistStepResult::Moved(Box::new(JumplistStepTarget {
            path: Some("/b.rs".into()),
            view_id: None,
            position: Some(LogicalPosition { line: 4, col: 9 }),
            anchor: Some(LogicalPosition { line: 4, col: 2 }),
            index: 3,
            total: 17,
            opened: Some(open),
            seat: None,
            skipped: 0,
        }))),
        Direction::Forward,
        aether_protocol::jumplist::JumplistStepScope::Full,
    ));

    assert_eq!(
        s.view.buffer.buffer_id, 7,
        "the step switched to the entry's buffer"
    );
    assert_eq!(
        s.view.buffer.cursor.jumplist_position,
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
        focus_run: None,
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

        collapsible: false,
        update: Some(update),
        truncated: false,
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
        focus_run: None,
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

        collapsible: false,
        update: Some(update),
        truncated: false,
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

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    // which arrives with the response and reveals *after* this — the order Views and the Explorer
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
    let fx = s.pointer_press(0, press, Granularity::Word, false);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "element/set");
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
    assert_eq!(method, "element/set");
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
    assert_eq!(s.view.buffer.cursor.position.col, 9);
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
    let fx = s.pointer_press(
        0,
        LogicalPosition { line: 5, col: 0 },
        Granularity::Char,
        true,
    );
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
    assert_eq!(s.view.mode, Mode::Insert);
    let _ = s.pointer_press(
        0,
        LogicalPosition { line: 2, col: 3 },
        Granularity::Char,
        false,
    );
    assert_eq!(
        s.view.mode,
        Mode::Insert,
        "single click only repositions the caret"
    );

    // Double click (Word) → immediate selection, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(
        0,
        LogicalPosition { line: 2, col: 3 },
        Granularity::Word,
        false,
    );
    assert_eq!(
        s.view.mode,
        Mode::Normal,
        "double-click selects a word → Normal"
    );

    // Shift-click (extend) → selection from the existing anchor, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(
        0,
        LogicalPosition { line: 2, col: 3 },
        Granularity::Char,
        true,
    );
    assert_eq!(
        s.view.mode,
        Mode::Normal,
        "shift-click extends a selection → Normal"
    );

    // Char drag past the press anchor → selection, drops to Normal.
    let mut s = session();
    let _ = key(&mut s, 'i');
    let _ = s.pointer_press(
        0,
        LogicalPosition { line: 2, col: 3 },
        Granularity::Char,
        false,
    );
    assert_eq!(
        s.view.mode,
        Mode::Insert,
        "the press alone hasn't selected anything yet"
    );
    let _ = s.pointer_drag(LogicalPosition { line: 2, col: 7 });
    assert_eq!(
        s.view.mode,
        Mode::Normal,
        "dragging out a selection → Normal"
    );
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
    let fx = s.on_key(KeyCode::Char('x'), ctrl_alt, None);

    // Cuts via the same RPC as a plain Ctrl-x...
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "buffer/cut");
    assert_eq!(params["scope"], json!("selection"));

    //...but unlike Ctrl-x (which stays in Normal) it leaves us in Insert at the gap.
    assert_eq!(s.view.mode, Mode::Insert);
}

/// Find the first `Effect::Request` whose method matches (the multi-request flows — re-list,
/// create — emit more than one, so `the_request`'s exactly-one assertion doesn't fit).
/// A request that names a buffer the server has already closed is stale by definition — the server
/// owns buffer lifetime, and it has told us (or is about to). Another client rebinding a worktree
/// closes the clean file-backed buffers under the roots that moved — several at once, under a
/// client that may be mid-keystroke — so each in-flight request came back as its own error toast.
#[test]
fn a_request_on_a_buffer_the_server_closed_is_dropped_silently() {
    use aether_client::transport::RpcError;
    use aether_protocol::error::ErrorCode;
    let mut s = session();
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, None);
    let token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, .. } => Some(*token),
                _ => None,
            })
            .expect("an edit went out");

    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "element/text",
            code: ErrorCode::BUFFER_NOT_FOUND.0,
            message: "unknown buffer_id: 7".into(),
        }),
    );
    assert!(
        fx.0.is_empty(),
        "no toast, no follow-up (got {} effects)",
        fx.0.len()
    );

    // Any *other* failure still surfaces — this is a narrow rule about a vanished buffer, not a
    // blanket swallow.
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, None);
    let token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, .. } => Some(*token),
                _ => None,
            })
            .expect("an edit went out");
    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "element/text",
            code: ErrorCode::INVALID_POSITION.0,
            message: "nope".into(),
        }),
    );
    assert!(has_error_toast(&fx), "other errors still show");
}

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
fn explorer_alt_l_enters_the_highlighted_directory_in_one_press() {
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
    // Alt-j moves the highlight onto the second entry — and the ghost follows it, previewing the
    // whole remainder of *that* name rather than the prefix the two entries share.
    let _ = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert_eq!(
        s.picker.as_ref().unwrap().explorer_completion().as_deref(),
        Some("her-tui"),
    );

    // Alt-l takes it: one press descends into the highlighted directory, however much of the name
    // is typed and whatever the other matches happen to share. Descending re-lists from an empty
    // query.
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let view = find_request(&fx, "picker/view").expect("alt-l descends via picker/view");
    assert_eq!(view["directory_path"], json!("/proj/aether-tui"));
    let requery = find_request(&fx, "picker/query").expect("descending re-lists");
    assert_eq!(requery["query"], json!(""));
    assert_eq!(s.picker.as_ref().unwrap().query, "");
}

#[test]
fn explorer_alt_l_opens_a_file() {
    use aether_client::keymap::Mods;
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj".into());
        p.query = "ma".into();
        p.items = vec![PickerItem::DirEntry {
            name: "main.rs".into(),
            is_dir: false,
            match_indices: vec![],
            git_status: None,
        }];
        p.total_matches = 1;
        p.offset = 0;
    }
    // A file has no inside, so Alt-l opens it rather than descending — no re-list, no query
    // rewrite (completing the name in place would only restate the highlight). It gets no ghost
    // either: the ghost is what Alt-l would *descend* into.
    assert_eq!(s.picker.as_ref().unwrap().explorer_completion(), None);
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    assert!(find_request(&fx, "picker/view").is_none());
    assert!(find_request(&fx, "picker/query").is_none());
    let params = find_request(&fx, "picker/select").expect("Alt-l opens the file");
    assert_eq!(params["item"]["name"], json!("main.rs"));
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
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
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
    let _ = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert!(
        s.picker.as_ref().unwrap().chips.is_empty(),
        "with no breadcrumb left, Alt-Backspace removes the chip"
    );
}

/// Alt-Backspace's first rung is word-grained, matching the key's meaning in a buffer and in the
/// path editors: one matcher atom per press, then the rungs below. A single-word query — most of
/// them — still clears in one press, which is why this is a refinement of the old wipe rather than
/// a different gesture.
#[test]
fn picker_alt_backspace_drops_one_query_word_per_press() {
    use aether_client::chips::ChipValue;
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.chips = vec![ChipValue::Changed];
        p.query = "src update".into();
    }

    // One atom, separator kept — the query narrows rather than vanishing.
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert!(find_request(&fx, "picker/query").is_some());
    assert_eq!(s.picker.as_ref().unwrap().query, "src ");
    assert_eq!(
        s.picker.as_ref().unwrap().chips.len(),
        1,
        "the query rung runs to exhaustion before any chip is touched"
    );

    // The next press takes the last atom and its trailing space together.
    let _ = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(s.picker.as_ref().unwrap().query, "");
    assert_eq!(s.picker.as_ref().unwrap().chips.len(), 1);

    // Only now does the ladder move on to the chips (Files has no breadcrumb rung).
    let _ = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert!(s.picker.as_ref().unwrap().chips.is_empty());
}

/// The single-word case, spelled out: unchanged from the wipe it replaces.
#[test]
fn picker_alt_backspace_still_clears_a_one_word_query_in_one_press() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    s.picker.as_mut().unwrap().query = "needle".into();
    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert!(find_request(&fx, "picker/query").is_some());
    assert_eq!(s.picker.as_ref().unwrap().query, "");
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
            json!(s.view.buffer.buffer_id),
            "{kind:?} frames the hunk nearest the live cursor instead"
        );
    }
}

/// `Space g b` opens the branch picker, carrying the active buffer as the repo-resolution hint —
/// the same rule `git/prepare_commit` uses, so the client never needs to know repo ids.
#[test]
fn space_g_b_opens_the_branch_picker() {
    let mut s = session();
    let fx = git_leader(&mut s, 'b');
    let view = find_request(&fx, "picker/view").expect("Space g b opens a picker");
    assert_eq!(view["kind"], json!("git_branches"));
    assert_eq!(
        view["buffer_id"],
        json!(s.view.buffer.buffer_id),
        "the active buffer rides along so the server can resolve the repo"
    );
}

use aether_protocol::picker::BranchCheckout;

/// Open the merged branch picker on a fixed two-branch listing: `main` (current) and `feature`.
///
/// `held` is the tree holding `feature`, if any — which is what turns its row from a branch row
/// into a worktree row.
fn branch_picker_session(held: Option<BranchCheckout>) -> aether_client::session::Session {
    use aether_protocol::picker::{PickerItem, PickerKind};
    let mut s = session();
    s.workspace = "p".into();
    s.workspace_paths = vec!["/p".into()];
    let _ = s.open_picker(PickerKind::GitBranches, None, None, false, None);
    let row = |name: &str, is_head: bool, checkout: Option<BranchCheckout>| PickerItem::GitBranch {
        repo_id: "/p".into(),
        name: name.into(),
        is_head,
        subject: "Add a thing".into(),
        timestamp: 1_700_000_000,
        upstream: None,
        ahead: 0,
        behind: 0,
        checkout,
        detached_at: None,
        match_indices: vec![],
    };
    {
        let p = s.picker.as_mut().expect("picker open");
        p.items = vec![
            // `main` carries the checkout holding it, as the server sends it: `checkouts_by_branch`
            // records the tree you are standing in like any other.
            row(
                "main",
                true,
                Some(BranchCheckout {
                    path: "/p".into(),
                    is_main: true,
                    worktree: String::new(),
                    is_current: true,
                    locked: false,
                    prunable: false,
                }),
            ),
            row("feature", false, held),
        ];
        p.total_matches = 2;
        p.selected = 1; // "feature"
    }
    s
}

/// A tree holding a branch: the linked-worktree case unless overridden.
fn linked(worktree: &str) -> BranchCheckout {
    BranchCheckout {
        path: format!("/store/{worktree}"),
        is_main: false,
        worktree: worktree.into(),
        is_current: false,
        locked: false,
        prunable: false,
    }
}

fn request_token(fx: &Effects, method: &str) -> Option<u64> {
    fx.0.iter().find_map(|e| match e {
        Effect::Request {
            method: m, token, ..
        } if *m == method => Some(*token),
        _ => None,
    })
}

/// A `BUFFER_NOT_FOUND` error is stripped of its toast but must still run its callback. Dropping
/// the callback too — as this once did — quietly took every state-clearing continuation with it.
#[test]
fn a_stale_buffer_error_still_runs_its_callback() {
    use aether_client::transport::RpcError;
    use aether_protocol::error::ErrorCode;
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Char('o'), Mods::CTRL, None);
    let token = request_token(&fx, "git/worktree_add").expect("Ctrl-o creates");

    let fx = s.on_rpc_result(
        token,
        Err(RpcError {
            method: "git/worktree_add",
            code: ErrorCode::BUFFER_NOT_FOUND.0,
            message: "unknown buffer_id: 7".into(),
        }),
    );
    assert!(
        !fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Error,
                ..
            }
        )),
        "a buffer the server already closed is still not worth a toast"
    );
}

/// `Ctrl-Enter` on a branch a tree holds targets **that tree**, in a new window. This is the verb
/// the whole context keying exists for: two windows, two trees of one repo, at once.
#[test]
fn ctrl_enter_on_a_held_branch_targets_its_tree() {
    let mut s = branch_picker_session(Some(linked("feature-auth")));
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None);
    let target =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::ShellAction(aether_client::effect::ShellAction::NewWindow(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .expect("Ctrl-Enter asks the shell for a new window");
    assert_eq!(target.workspace.as_deref(), Some("p"));
    assert_eq!(
        target.worktrees,
        vec![("/p".to_string(), "feature-auth".to_string())],
        "the target names the tree to open, not the internal context id"
    );
}

/// A branch **no** tree holds can't be opened in a second window — git permits one checkout per
/// branch. It refuses with the way forward rather than falling through to an ordinary accept, which
/// would check the branch out *here*: something else entirely from what was asked for, and silently.
#[test]
fn ctrl_enter_on_a_treeless_branch_refuses_with_guidance() {
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None);
    assert!(
        find_request(&fx, "git/checkout").is_none(),
        "it must not quietly do what plain Enter does"
    );
    let toast = toast_messages(&fx).join(" ");
    assert!(
        toast.contains("feature") && toast.contains("no worktree"),
        "names the branch and the missing thing: {toast}"
    );
}

/// `Ctrl-o` creates a worktree for the highlighted branch and **stays put**. Its own key rather
/// than a side effect of Enter: creation is a long, cancellable checkout, and bundling it into a
/// navigation key leaves "where am I?" unanswerable when it's cancelled mid-flight.
#[test]
fn ctrl_o_creates_a_worktree_without_moving() {
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Char('o'), Mods::CTRL, None);

    let req = find_request(&fx, "git/worktree_add").expect("Ctrl-o creates");
    assert_eq!(req["branch"], json!("feature"));
    assert_eq!(
        req["repo_id"],
        json!("/p"),
        "the row's repo, not a re-resolve"
    );
    assert!(
        req.get("create_branch").is_none_or(|c| c == &json!(false)),
        "the branch already exists — only the tree is new"
    );
    assert!(
        find_request(&fx, "workspace/bind_worktree").is_none(),
        "creating never moves you: that is the whole point of the split from Enter"
    );
    assert!(s.picker.is_some(), "and the picker stays open on the row");
}

/// A branch that already has a tree has nothing to create. It says which tree, because admin names
/// drift from branch names — "feature already has a worktree" would be the wrong sentence for a
/// row whose tree is called something else.
#[test]
fn ctrl_o_refuses_a_branch_that_already_has_a_tree() {
    let mut s = branch_picker_session(Some(linked("feature-auth")));
    let fx = s.on_key(KeyCode::Char('o'), Mods::CTRL, None);
    assert!(find_request(&fx, "git/worktree_add").is_none());
    let toast = toast_messages(&fx).join(" ");
    assert!(toast.contains("feature-auth"), "names the tree: {toast}");
}

/// `Ctrl-d` takes the **outermost** thing off: a row with a tree loses the tree, a row without one
/// loses the branch. The exact inverse of `Ctrl-o` building the tree onto the branch.
#[test]
fn ctrl_d_removes_the_tree_when_there_is_one() {
    let mut s = branch_picker_session(Some(linked("feature-auth")));
    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);

    let req = find_request(&fx, "git/worktree_remove").expect("Ctrl-d removes the tree");
    assert_eq!(
        req["name"],
        json!("feature-auth"),
        "keyed on the admin name, not the branch"
    );
    assert!(
        req.get("force").is_none_or(|f| f == &json!(false)),
        "the first press is never forced — its refusal is what itemises the risk"
    );
    assert!(
        s.prompt.is_none(),
        "no modal: a confirm carrying no facts would only train people to confirm"
    );
    assert!(
        find_request(&fx, "git/delete_branch").is_none(),
        "the branch survives its worktree"
    );
}

/// The same key on a row with no tree falls through to deleting the branch — behind a confirm,
/// because unlike a worktree removal there is no refusal to read first.
#[test]
fn ctrl_d_deletes_the_branch_when_there_is_no_tree() {
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);
    assert!(find_request(&fx, "git/worktree_remove").is_none());
    assert!(s.prompt.is_some(), "branch deletion stages a confirm");
}

/// `Ctrl-Alt-d` is the escalation from having read the removal's refusal. Alt rather than Shift,
/// because a terminal reports Ctrl-Shift-D indistinguishably from Ctrl-d.
#[test]
fn ctrl_alt_d_forces_the_removal() {
    let mut s = branch_picker_session(Some(linked("feature-auth")));
    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL_ALT, None);
    let req = find_request(&fx, "git/worktree_remove").expect("Ctrl-Alt-d removes");
    assert_eq!(req["force"], json!(true));
}

/// The main checkout is the repository, and the tree you are standing in can't be pulled out from
/// under you. Both say so client-side rather than failing at the server.
#[test]
fn ctrl_d_refuses_the_trees_that_cannot_be_removed() {
    for (label, checkout) in [
        (
            "main checkout",
            BranchCheckout {
                is_main: true,
                worktree: String::new(),
                ..linked("")
            },
        ),
        (
            "the tree you're in",
            BranchCheckout {
                is_current: true,
                ..linked("feature-auth")
            },
        ),
    ] {
        let mut s = branch_picker_session(Some(checkout));
        let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);
        assert!(
            find_request(&fx, "git/worktree_remove").is_none(),
            "{label} cannot be removed"
        );
        assert!(
            s.prompt.is_none(),
            "{label} does not fall through to a branch delete"
        );
    }
}

/// Enter on a branch row checks it out, naming the repo the *row* carried rather than
/// re-resolving — the picker may have been opened over a different repo than the active buffer's.
#[test]
fn branch_picker_enter_checks_out_the_highlighted_branch() {
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);

    let req = find_request(&fx, "git/checkout").expect("Enter checks out");
    assert_eq!(req["branch"], json!("feature"));
    assert_eq!(
        req["repo_id"],
        json!("/p"),
        "the row's repo, not a re-resolve"
    );
    assert!(
        req.get("create").is_none_or(|c| c == &json!(false)),
        "an existing branch is switched to, not created"
    );
    assert!(
        s.picker.is_none(),
        "a checkout is terminal — the list closes"
    );
}

/// `Alt-l` is "one level deeper" everywhere; a flat picker has no deeper, so it means open.
#[test]
fn branch_picker_alt_l_checks_out_like_enter() {
    let mut s = branch_picker_session(None);
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let req = find_request(&fx, "git/checkout").expect("Alt-l checks out");
    assert_eq!(req["branch"], json!("feature"));
}

/// **The merge, in one test.** A branch another tree holds used to dead-end here: git refuses a
/// second checkout, so the branch picker could only toast "checked out in another worktree" and
/// stop — with the way forward under a different key, in a list you were not looking at.
///
/// Now the row *is* the worktree row, and Enter goes there. One intent — get me to this branch —
/// with git's state picking the mechanism.
#[test]
fn enter_on_a_held_branch_opens_its_worktree() {
    let mut s = branch_picker_session(Some(linked("feature-auth")));
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);

    assert!(
        find_request(&fx, "git/checkout").is_none(),
        "git can't check out a branch another tree holds"
    );
    let req = find_request(&fx, "workspace/bind_worktree").expect("Enter goes to the tree");
    assert_eq!(
        req["worktree"],
        json!("feature-auth"),
        "bound by admin name, which drifts from the branch"
    );
    assert_eq!(
        req["repo_id"],
        json!("/p"),
        "the row's repo, not a re-resolve"
    );
    let toast = toast_messages(&fx).join(" ");
    assert!(
        !toast.contains("checked out in"),
        "the refusal this replaced is gone: {toast}"
    );
}

#[test]
fn branch_picker_enter_on_the_current_branch_is_a_no_op() {
    let mut s = branch_picker_session(None);
    s.picker.as_mut().unwrap().selected = 0; // "main", the HEAD row
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        find_request(&fx, "git/checkout").is_none(),
        "already on it — nothing to run"
    );
}

/// Typing a novel name offers "+ Create", which creates *and* switches (`git checkout -b`) —
/// matching the Explorer/Workspaces create rows, which open and activate what they made.
#[test]
fn branch_picker_create_row_creates_and_switches() {
    let mut s = branch_picker_session(None);
    s.picker_set_query("new-thing".into());
    {
        let p = s.picker.as_ref().unwrap();
        assert!(
            p.pending_create().is_some(),
            "a name no branch carries offers the create row"
        );
    }
    s.picker.as_mut().unwrap().selected = s.picker.as_ref().unwrap().create_row_index().unwrap();

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let req = find_request(&fx, "git/checkout").expect("the create row checks out");
    assert_eq!(req["branch"], json!("new-thing"));
    assert_eq!(req["create"], json!(true));
}

#[test]
fn branch_picker_create_row_hides_once_the_name_matches_a_branch() {
    let mut s = branch_picker_session(None);
    s.picker_set_query("feature".into());
    assert!(
        s.picker.as_ref().unwrap().pending_create().is_none(),
        "an exact existing name would be checked out by Enter, so there's nothing to create"
    );
}

/// `Ctrl-d` deletes behind a confirm — the same gesture Explorer/Files/Workspaces and Views use.
#[test]
fn branch_picker_ctrl_d_confirms_then_deletes() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_client::update::Event;
    let mut s = branch_picker_session(None);

    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);
    assert!(
        find_request(&fx, "git/delete_branch").is_none(),
        "Ctrl-d stages a confirm; it doesn't delete outright"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::DeleteBranch { name },
            ..
        }) => assert_eq!(name, "feature"),
        other => panic!("expected a delete-branch confirm, got {other:?}"),
    }

    let fx = s.on_event(Event::PromptAccept);
    let req = find_request(&fx, "git/delete_branch").expect("accepting deletes");
    assert_eq!(req["branch"], json!("feature"));
    assert!(
        req.get("force").is_none_or(|f| f == &json!(false)),
        "the first attempt is never forced"
    );
}

/// A branch the *main* checkout holds is reached by **unbinding** — sending this repo back to its
/// main tree. An empty admin name is exactly what `workspace/bind_worktree` reads as that, so the
/// main row needs no case of its own.
#[test]
fn a_branch_held_by_the_main_checkout_unbinds() {
    let mut s = branch_picker_session(Some(BranchCheckout {
        path: "/p".into(),
        is_main: true,
        worktree: String::new(),
        is_current: false,
        locked: false,
        prunable: false,
    }));
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);

    assert!(
        find_request(&fx, "git/checkout").is_none(),
        "git can't check out a branch another tree holds — you go there instead"
    );
    let req = find_request(&fx, "workspace/bind_worktree").expect("Enter goes to the holder");
    assert!(
        req.get("worktree").is_none_or(|w| w == &json!("")),
        "an empty admin name is the unbind"
    );
}

/// Git refuses to delete the branch you are on, so don't stage a doomed confirm — and don't try to
/// remove the tree you are standing in either.
#[test]
fn branch_picker_ctrl_d_refuses_the_current_branch() {
    let mut s = branch_picker_session(None);
    s.picker.as_mut().unwrap().selected = 0; // "main", the row you are standing on
    let fx = s.on_key(KeyCode::Char('d'), Mods::CTRL, None);
    assert!(s.prompt.is_none(), "no confirm for a doomed delete");
    assert!(find_request(&fx, "git/delete_branch").is_none());
    assert!(find_request(&fx, "git/worktree_remove").is_none());
    let toast = toast_messages(&fx).join(" ");
    assert!(
        toast.contains("You're on main"),
        "says you are standing on it, not that it is \"the repository\": {toast}"
    );
}

/// A `NotMerged` refusal escalates into a second confirm rather than dead-ending, and accepting
/// re-sends with `force`. The status comes from a real merge-base check server-side, which is what
/// lets the client offer this instead of pattern-matching git's wording.
#[test]
fn not_merged_delete_escalates_to_a_force_confirm() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_client::update::Event;
    let mut s = branch_picker_session(None);

    let fx = s.on_event(Event::BranchDeleted {
        branch: "feature".into(),
        forced: false,
        result: Ok(serde_json::from_value(json!({ "status": "not_merged" })).unwrap()),
    });
    assert!(fx.0.is_empty(), "no toast — a confirm is the response");
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::DeleteUnmergedBranch { name },
            ..
        }) => assert_eq!(name, "feature"),
        other => panic!("expected the unmerged escalation, got {other:?}"),
    }

    let fx = s.on_event(Event::PromptAccept);
    let req = find_request(&fx, "git/delete_branch").expect("accepting force-deletes");
    assert_eq!(req["force"], json!(true));
}

#[test]
fn a_forced_delete_that_still_reports_not_merged_does_not_loop() {
    use aether_client::update::Event;
    // Defensive: escalating again would be an infinite confirm. Reachable only if the server's
    // pre-flight and git ever disagree, which is exactly when a loop would be worst.
    let mut s = branch_picker_session(None);
    let fx = s.on_event(Event::BranchDeleted {
        branch: "feature".into(),
        forced: true,
        result: Ok(serde_json::from_value(json!({ "status": "not_merged" })).unwrap()),
    });
    assert!(s.prompt.is_none(), "no second escalation");
    assert!(
        !fx.0.is_empty(),
        "the user is still told something happened"
    );
}

/// A blocked checkout names how many buffers to save — the one refusal with a concrete next step.
#[test]
fn blocked_checkout_tells_the_user_to_save() {
    use aether_client::update::Event;
    let mut s = branch_picker_session(None);
    let fx = s.on_event(Event::CheckedOut {
        branch: "feature".into(),
        result: Ok(serde_json::from_value(json!({
            "status": "blocked_by_dirty_buffers",
            "blocked": [7, 9],
        }))
        .unwrap()),
    });
    let toasted = toast_messages(&fx).join(" | ");
    assert!(
        toasted.contains("2 unsaved") && toasted.to_lowercase().contains("save first"),
        "names the count and the way out, got: {toasted}"
    );
    assert!(
        !toasted.contains("Space"),
        "…as an act, not a chord that goes stale when the keymap moves: {toasted}"
    );
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
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
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
                unsaved: 0,
                match_indices: vec![],
            },
            PickerItem::Workspace {
                name: "other".into(),
                unsaved: 0,
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
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
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
    // Titled by the act that failed, with the tailored sentence as the detail line under it.
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("delete") && msg.contains("another window"),
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    assert_eq!(s.view.mode, aether_client::session::Mode::Search);
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
    let fx = s.on_key(KeyCode::Char(chord), Mods::ALT, None);
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
            view_id: aether_protocol::ViewId(buffer_id),
            display: display.into(),
            commit: None,
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
    let fx = s.picker_close_view();
    assert!(s.prompt.is_none(), "clean close needs no confirm");
    let close = find_request(&fx, "view/close").expect("view/close fired");
    assert_eq!(close["view_id"], json!(7), "the row's view");
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
    let fx = s.picker_close_view();
    assert!(s.prompt.is_none());
    let close = find_request(&fx, "view/close").expect("view/close fired");
    assert_eq!(close["view_id"], json!(0));
    assert_eq!(
        close["open_next"],
        json!(true),
        "closing the active buffer opens its MRU successor"
    );

    // A dirty buffer: closing it stages a discard confirm and sends nothing yet.
    s.picker.as_mut().unwrap().selected = 2;
    let fx = s.picker_close_view();
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
    // `y` accepts → view/close { buffer_id: 9, open_next: false } (id 9 isn't the active buffer).
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    let close = find_request(&fx, "view/close").expect("view/close fired on confirm");
    assert_eq!(close["view_id"], json!(9));
    assert_eq!(close["open_next"], json!(false));
}

/// The view-picker close chord is `Ctrl-d` (the delete-file gesture in the other pickers, free
/// here because the guards are keyed by picker kind). It is deliberately NOT `Ctrl-x`: every GUI
/// shell's focused query input claims Ctrl-x as its native Cut and swallows it before the core sees
/// it, so Ctrl-x would only ever work in the TUI. Closing the *active* buffer switches the editor to
/// a successor but keeps the picker open — the user is still working the list.
#[test]
fn buffers_picker_ctrl_d_closes_active_buffer_and_keeps_picker_open() {
    use aether_client::update::Event;
    use aether_protocol::picker::{BufferDirtyState, PickerItem, PickerKind};
    use aether_protocol::view::{BufferDescription, ViewOpenResult};

    fn buf(buffer_id: u64, display: &str) -> PickerItem {
        PickerItem::Buffer {
            buffer_id,
            view_id: aether_protocol::ViewId(buffer_id),
            display: display.into(),
            commit: None,
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
        find_request(&fx, "view/close").is_none(),
        "Ctrl-x must not close a buffer in the view picker"
    );
    assert!(
        s.picker.is_some(),
        "an unhandled chord leaves the picker open"
    );

    // Ctrl-d closes the highlighted (active) buffer, attaching its MRU successor via open_next.
    let fx = ctrl(&mut s, 'd');
    let close = find_request(&fx, "view/close").expect("Ctrl-d fires view/close");
    assert_eq!(close["view_id"], json!(0));
    assert_eq!(close["open_next"], json!(true));

    // When the successor switch resolves, the editor rebinds to it *and the picker stays open* — a
    // switch no longer tears the picker down (see `adopt_switch`); the pick path owns that.
    let successor = ViewOpenResult {
        view_id: aether_protocol::ViewId(7),
        scroll: None,
        transient: false,
        read: false,
        buffer: BufferDescription {
            buffer_id: 7,
            language: None,
            line_count: 1,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: Some("/proj/other.rs".into()),
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
    };
    let _ = s.on_event(Event::Switched(Ok(successor)));
    assert_eq!(
        s.view.buffer.buffer_id, 7,
        "editor rebinds to the successor buffer"
    );
    assert!(
        s.picker.is_some(),
        "closing the active buffer from the picker keeps the picker open"
    );
}

/// **A view created with no opinion is a preview.** The three client-side opens that used to lean
/// on the old kept-by-default — a fresh scratch, a file created from the explorer, and the
/// `Space Alt-w` open-by-path overlay — must all keep saying *nothing* about keeping, so the server
/// makes each of them a preview. Each is kept the moment the user does something to it.
#[test]
fn opens_with_no_opinion_send_no_keep_flag() {
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    use aether_protocol::picker::PickerKind;

    // A fresh scratch (`Space Alt-b`).
    let mut s = session();
    s.workspace = "proj".into();
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('b'), Mods::ALT, None);
    let open = find_request(&fx, "view/open").expect("a new scratch opens a view");
    assert_eq!(
        open["transient"],
        serde_json::Value::Null,
        "a fresh scratch says nothing, so it is a preview until you type in it: {open}"
    );

    // A file created from the explorer.
    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "new.rs".into();
    }
    let fx = s.explorer_create_from_query();
    let open = find_request(&fx, "view/open").expect("explorer create opens a view");
    assert_eq!(
        open["transient"],
        serde_json::Value::Null,
        "an explorer create says nothing; the save that writes it keeps it: {open}"
    );

    // `Space Alt-w`, the open-from-path overlay.
    let mut s = session();
    s.workspace = "proj".into();
    s.prompt = Some(Prompt::OpenPath(Box::new(PathEditor::absolute(
        String::new(),
        true,
    ))));
    let _ = s.open_path_set_input("/etc/hosts".into());
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    let open = find_request(&fx, "workspace/open_path").expect("the overlay opens by path");
    assert_eq!(
        open["transient"],
        serde_json::Value::Null,
        "naming a file is going to look at one, not keeping it: {open}"
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
    let open = find_request(&fx, "view/open").expect("view/open fired");
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
        find_request(&fx, "view/open").is_none(),
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
            focus_run: None,
            center_on: None,
            explorer_peek_missing: false,
        });
        assert_eq!(p.create_row_index(), Some(1));
    }
    // Click the create row (absolute index 1) → highlight it and accept.
    let fx = s.on_event(Event::PickerClicked(1));
    let open = find_request(&fx, "view/open").expect("view/open fired");
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
    let fx = s.on_key(KeyCode::Char('%'), shifted, Some("%".to_string()));
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/select_all");
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
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);

    // Tab still indents in Insert: only Normal and Read vacated it for element focus, so a daily-use
    // key was not spent on a mode where you would press Esc before moving between editors anyway.
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/tab");
    // No text on the wire: the payload is just the buffer.
    assert_eq!(params.get("text"), None);
}

/// Insert mode's Alt tier goes out as word-grain RPCs — the boundary rules live server-side, so
/// the client only names the direction.
#[test]
fn insert_alt_tier_sends_word_grain_requests() {
    use aether_client::keymap::Mods;
    let mut s = session();
    key(&mut s, 'i');
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);

    let fx = s.on_key(KeyCode::Backspace, Mods::ALT, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/delete_word");
    assert_eq!(params["direction"], json!("backward"));
    assert_eq!(params["boundary"], json!("word"));

    let fx = s.on_key(KeyCode::Delete, Mods::ALT, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/delete_word");
    assert_eq!(params["direction"], json!("forward"));

    let fx = s.on_key(KeyCode::Left, Mods::ALT, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(
        params["motion"],
        json!({"kind": "word", "direction": "backward", "count": 1, "boundary": "word"})
    );

    let fx = s.on_key(KeyCode::Right, Mods::ALT, None);
    let (_t, _method, params) = the_request(&fx);
    assert_eq!(params["motion"]["direction"], json!("forward"));

    // Unmodified, the same keys stay char-grain.
    let fx = s.on_key(KeyCode::Backspace, Mods::NONE, None);
    let (_t, method, _params) = the_request(&fx);
    assert_eq!(method, "element/backspace");
}

/// Home / End are bound in Insert as well as Normal — Insert has no fallthrough to Normal's table,
/// so before this they did nothing at all while typing.
#[test]
fn insert_home_end_move_to_the_line_ends() {
    let mut s = session();
    key(&mut s, 'i');

    let fx = s.on_key(KeyCode::Home, Mods::NONE, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"], json!({"kind": "line_start"}));

    let fx = s.on_key(KeyCode::End, Mods::NONE, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"], json!({"kind": "line_end"}));
}

#[test]
fn space_n_triggers_hover() {
    let mut s = session();
    // The hover reveal is a leader chord; it moved from `t` to `n` when the shells picker took `t`.
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let fx = s.on_key(KeyCode::Char('n'), Mods::NONE, None);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "lsp/hover");
}

/// The single Info-toast message in `fx`, if any.
fn info_toast(fx: &Effects) -> Option<String> {
    fx.0.iter().find_map(|e| match e {
        Effect::Toast {
            title: m,
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
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let token = the_request(&s.on_key(KeyCode::Char('n'), Mods::NONE, None)).0;
    let fx = s.on_rpc_result(token, Ok(json!({ "contents": null, "readiness": "ready" })));
    assert_eq!(info_toast(&fx).as_deref(), Some("No hover info"));

    // A server still starting → say so, not "No hover info".
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let token = the_request(&s.on_key(KeyCode::Char('n'), Mods::NONE, None)).0;
    let fx = s.on_rpc_result(
        token,
        Ok(json!({ "contents": null, "readiness": "starting" })),
    );
    assert_eq!(
        info_toast(&fx).as_deref(),
        Some("Language server still starting")
    );

    // A crashed/stopped server → "unavailable".
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let token = the_request(&s.on_key(KeyCode::Char('n'), Mods::NONE, None)).0;
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
fn space_alt_n_shows_diagnostic_at_cursor() {
    // Space Alt-n → diagnostic at cursor, paired with `Space n` (hover). With no diagnostics loaded
    // it reports "none" via a toast (resolved locally — no RPC), which still proves the chord
    // reaches `show_diagnostic`.
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('n'), Mods::ALT, Some("n".to_string()));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast {
                kind: ToastKind::Info,
                ..
            }
        )),
        "Space Alt-n with no diagnostics toasts an info message"
    );
}

#[test]
fn space_m_shows_blame_commit() {
    // Space m → blame the cursor line (round-trip resolves the commit's details).
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('m'), Mods::NONE, Some("m".to_string()));
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "git/blame_line");
}

/// The `Space g` sub-leader as a state machine: `g` arms it (no effects, and the shells read
/// `Pending::LeaderGit` to draw the awaiting-key cursor), the next key runs the git action, and an
/// unbound key cancels instead of leaking through to Normal mode.
#[test]
fn space_g_arms_the_git_sub_leader_and_the_next_key_completes_it() {
    use aether_client::session::Pending;

    let mut s = session();
    s.view.viewport_id = Some(1); // the diff toggle addresses a viewport
    let fx = key(&mut s, ' ');
    assert!(matches!(s.view.pending, Pending::Leader));
    assert!(fx.0.is_empty(), "the leader alone does nothing");

    let fx = key(&mut s, 'g');
    assert!(
        matches!(s.view.pending, Pending::LeaderGit),
        "Space g waits for one more key rather than opening grep"
    );
    assert!(fx.0.is_empty(), "the prefix alone does nothing");

    let fx = s.on_key(KeyCode::Char('f'), Mods::NONE, Some("f".into()));
    assert!(
        find_request(&fx, "git/fetch").is_some(),
        "Space g f fetches"
    );
    assert!(
        matches!(s.view.pending, Pending::None),
        "the chord is spent"
    );

    // The inline diff left the sub-leader for `Space i` — one key, no prefix.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('i'), Mods::NONE, Some("i".into()));
    assert!(
        find_request(&fx, "git/set_diff_view").is_some(),
        "Space i toggles the inline diff"
    );

    // An unbound second key cancels: no request, and `j` must not move the cursor either.
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = s.on_key(KeyCode::Char('j'), Mods::NONE, Some("j".into()));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "an unbound git chord is silently dropped"
    );
    assert!(matches!(s.view.pending, Pending::None));
}

/// The index verbs are one key each, in one direction each: `s` stages, `u` unstages, `r` reverts,
/// and Alt widens the same verb to the whole file. Nothing here resolves a direction from state —
/// pressing `s` twice must stage twice, not stage and then quietly undo it.
#[test]
fn space_g_stages_unstages_and_reverts_at_two_scopes() {
    let mut s = session();
    let hunk = |s: &mut Session, ch: char, mods: Mods| {
        let _ = key(s, ' ');
        let _ = key(s, 'g');
        let text = (mods == Mods::NONE).then(|| ch.to_string());
        let fx = s.on_key(KeyCode::Char(ch), mods, text);
        let params = find_request(&fx, "git/apply_hunk").expect("git/apply_hunk fired");
        (
            params["action"].as_str().unwrap().to_string(),
            params
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("cursor")
                .to_string(),
        )
    };

    assert_eq!(
        hunk(&mut s, 's', Mods::NONE),
        ("stage".into(), "cursor".into())
    );
    assert_eq!(
        hunk(&mut s, 's', Mods::ALT),
        ("stage".into(), "file".into())
    );
    assert_eq!(
        hunk(&mut s, 'u', Mods::NONE),
        ("unstage".into(), "cursor".into())
    );
    assert_eq!(
        hunk(&mut s, 'u', Mods::ALT),
        ("unstage".into(), "file".into())
    );
    assert_eq!(
        hunk(&mut s, 'r', Mods::NONE),
        ("revert".into(), "cursor".into())
    );
    assert_eq!(
        hunk(&mut s, 'r', Mods::ALT),
        ("revert".into(), "file".into())
    );

    // Twice in a row is twice the same request — the property a toggle could not have.
    assert_eq!(hunk(&mut s, 's', Mods::NONE).0, "stage");
    assert_eq!(hunk(&mut s, 's', Mods::NONE).0, "stage");
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
        editor_font_size: 16,
        ui_font_size: 12,
        ..AppSettings::default()
    })));
    assert_eq!(s.editor_font_size, 16, "persisted buffer size is adopted");
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
    let buffer_row = row_index(&s, AppSettingId::EditorFontSize);
    let fx = s.app_settings_toggle(buffer_row);
    assert_eq!(s.editor_font_size, 18, "16 → next preset 18");
    assert_eq!(s.ui_font_size, 12, "the UI size is untouched");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["editor_font_size"], json!(18));
    assert_eq!(params["ui_font_size"], json!(12));

    // Left steps down to the previous preset (no wrap), also persisting.
    let fx = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.editor_font_size, 16, "Left steps down a preset");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["editor_font_size"], json!(16));

    // The UI row is its own stepper over the same presets, and moves only the UI size.
    let ui_row = row_index(&s, AppSettingId::UiFontSize);
    let fx = s.app_settings_toggle(ui_row);
    assert_eq!(s.ui_font_size, 13, "12 → next preset 13");
    assert_eq!(s.editor_font_size, 16, "the buffer size is untouched");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["ui_font_size"], json!(13));

    let fx = s.on_key(KeyCode::Right, Mods::NONE, None);
    assert_eq!(s.ui_font_size, 14, "Right steps up a preset");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["ui_font_size"], json!(14));
    assert_eq!(params["editor_font_size"], json!(16));
}

/// The background-fetch setting is off until asked for, and toggling it persists through the same
/// `settings/set` every other row uses. Off-by-default is the load-bearing half: this is the only
/// setting that makes the editor talk to the network unprompted.
#[test]
fn background_fetch_setting_is_off_by_default_and_persists() {
    use aether_client::session::AppSettingId;
    let mut s = session();
    assert!(!s.git_auto_fetch, "unattended network access is opt-in");

    s.open_app_settings();
    let row = s
        .app_setting_rows()
        .iter()
        .position(|r| r.id == AppSettingId::GitAutoFetch)
        .expect("a background-fetch row");
    let fx = s.app_settings_toggle(row);
    assert!(s.git_auto_fetch);
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["git_auto_fetch"], json!(true));
    // Turning it on has no visible effect until the server's next tick, so the toast is the only
    // confirmation that anything happened.
    assert!(
        toast_messages(&fx).iter().any(|m| m.contains("enabled")),
        "expected an enabled toast, got {:?}",
        toast_messages(&fx)
    );

    let fx = s.app_settings_toggle(row);
    assert!(!s.git_auto_fetch);
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["git_auto_fetch"], json!(false));
}

/// `Space g f` fetches the repo of the buffer we're on, letting the server resolve it — the client
/// never needs to know a repo id.
#[test]
fn space_g_f_fetches() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = key(&mut s, 'f');
    let req = find_request(&fx, "git/fetch").expect("git/fetch fired");
    assert!(req.get("repo_id").is_none_or(|v| v.is_null()));
    assert!(req.get("buffer_id").is_some());
}

/// A completed fetch reports the *divergence*, not the transfer: "fetched" on its own leaves the
/// user hunting for what changed, and the counts are the reason to fetch at all. The three cases
/// read differently on purpose — no upstream, level, diverged.
#[test]
fn a_finished_fetch_reports_what_it_found() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitFetchResult, GitFetchStatus, GitUpstreamStatus};

    let mut s = session();
    let fetched = |upstream| {
        Event::FetchDone(Ok(GitFetchResult {
            status: GitFetchStatus::Fetched,
            message: String::new(),
            upstream,
        }))
    };

    let fx = s.on_event(fetched(Some(GitUpstreamStatus {
        name: "origin/main".into(),
        ahead: 2,
        behind: 5,
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("2 ahead") && msg.contains("5 behind") && msg.contains("origin/main"),
        "diverged fetch should name both counts and the upstream, got {msg:?}"
    );

    let fx = s.on_event(fetched(Some(GitUpstreamStatus {
        name: "origin/main".into(),
        ahead: 0,
        behind: 0,
    })));
    assert!(
        toast_messages(&fx)
            .join(" ")
            .to_lowercase()
            .contains("up to date"),
        "level with upstream is its own message"
    );

    // No upstream is not "in sync with nothing" — there is simply nothing to report.
    let fx = s.on_event(fetched(None));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        !msg.contains("up to date") && !msg.contains("behind"),
        "a branch with no upstream claims no divergence, got {msg:?}"
    );

    // A refusal surfaces git's own words rather than a paraphrase.
    let fx = s.on_event(Event::FetchDone(Ok(GitFetchResult {
        status: GitFetchStatus::Refused,
        message: "fatal: could not read Username".into(),
        upstream: None,
    })));
    assert!(has_error_toast(&fx));
    assert!(toast_messages(&fx)
        .join(" ")
        .contains("could not read Username"));
}

#[test]
fn space_g_alt_p_pushes() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = s.on_key(KeyCode::Char('p'), Mods::ALT, None);
    let req = find_request(&fx, "git/push").expect("git/push fired");
    assert!(req.get("repo_id").is_none_or(|v| v.is_null()));
    assert!(req.get("buffer_id").is_some());
}

/// `Space g d` is the one git verb that asks first, and it asks *because of what it reaches*: the
/// abort resets the working tree from disk, so conflict resolutions in it are past the undo stack.
/// The question names the operation, which is also the only way the user can tell which of a merge
/// and a rebase they are about to throw away.
#[test]
fn abandoning_a_stopped_operation_confirms_and_names_it() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_client::update::Event;
    use aether_protocol::git::{GitBufferStatus, GitRepoOperation};
    use aether_protocol::viewport::Window;

    let window = |operation| Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: Some(GitBufferStatus {
            operation,
            ..Default::default()
        }),
        root: aether_protocol::viewport::Element::Editor {
            collapsed: false,
            element: 0,
            buffer: 0,
            rows: 0,
            first_row: aether_protocol::coords::ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            first_buffer_line: 0,
            lines: vec![],
        },
    };

    let mut s = session();
    s.view.window = Some(window(Some(GitRepoOperation::Rebase)));
    let fx = git_leader(&mut s, 'd');
    assert!(
        find_request(&fx, "git/abort_operation").is_none(),
        "nothing is abandoned before the question is answered"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::AbandonOperation { operation },
            ..
        }) => assert_eq!(*operation, GitRepoOperation::Rebase),
        other => panic!("expected an abandon confirm, got {other:?}"),
    }

    // Accepting sends it, resolved from the buffer like every other git verb.
    let fx = s.on_event(Event::PromptAccept);
    let params = find_request(&fx, "git/abort_operation").expect("the confirmed abort runs");
    assert_eq!(params["buffer_id"], json!(s.view.buffer.buffer_id));
    assert!(params.get("repo_id").is_none_or(|v| v.is_null()));

    // Declining leaves the repo exactly as it was.
    s.view.window = Some(window(Some(GitRepoOperation::Merge)));
    let _ = git_leader(&mut s, 'd');
    let fx = s.on_event(Event::PromptCancel);
    assert!(find_request(&fx, "git/abort_operation").is_none());
    assert!(s.prompt.is_none(), "declining closes the prompt");

    // With nothing stopped there is nothing to lose, so the key goes straight through and lets the
    // server answer "nothing in progress".
    s.view.window = Some(window(None));
    let fx = git_leader(&mut s, 'd');
    assert!(s.prompt.is_none(), "no operation, no question");
    assert!(find_request(&fx, "git/abort_operation").is_some());
}

/// The stash pair: `t` takes the working tree, `Alt-t` only what's staged. The flag has to reach
/// the wire *and* the wording, because "stashed the working tree" after a `--staged` push would
/// claim the unstaged work went too — when it is still sitting in the buffer.
#[test]
fn space_g_t_stashes_the_tree_and_alt_t_only_the_index() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitStashResult, GitStashStatus};

    let mut s = session();
    let fx = git_leader(&mut s, 't');
    let params = find_request(&fx, "git/stash_push").expect("Space g t stashes");
    assert!(
        params.get("staged").is_none_or(|v| v == &json!(false)),
        "the plain stash takes the whole tree"
    );

    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = s.on_key(KeyCode::Char('t'), Mods::ALT, None);
    let params = find_request(&fx, "git/stash_push").expect("Space g Alt-t stashes the index");
    assert_eq!(params["staged"], json!(true));

    let toast = |s: &mut Session, staged, status| {
        toast_messages(&s.on_event(Event::StashDone {
            staged,
            result: Ok(GitStashResult {
                status,
                ..Default::default()
            }),
        }))
        .join(" ")
    };
    assert!(toast(&mut s, false, GitStashStatus::Pushed).contains("working tree"));
    let msg = toast(&mut s, true, GitStashStatus::Pushed);
    assert!(msg.contains("staged changes"), "got {msg:?}");
    assert!(!msg.contains("working tree"), "got {msg:?}");
    // The narrower emptiness answer, for the tree that has changes but none of them staged.
    let msg = toast(&mut s, true, GitStashStatus::NothingToStash);
    assert!(msg.contains("Nothing staged"), "got {msg:?}");
    // And the git that is too old for the flag names the version, since the fix is outside here.
    let msg = toast(&mut s, true, GitStashStatus::StagedUnsupported);
    assert!(msg.contains("2.35"), "got {msg:?}");
}

/// "No change here" is the wrong sentence once the directions are separate keys: on a hunk that is
/// sitting there staged, `Space g s` has nothing to do and `Space g u` has plenty. The toast is
/// worded from the action that was sent, which is the only thing that knows which was asked.
#[test]
fn nothing_to_do_is_worded_from_the_direction_asked_for() {
    use aether_client::update::Event;
    use aether_protocol::git::{ApplyHunkStatus, ApplyScope, GitApplyHunkResult, HunkAction};

    let mut s = session();
    let toast = |s: &mut Session, action, scope| {
        toast_messages(&s.on_event(Event::HunkApplied {
            action,
            scope,
            result: Ok(GitApplyHunkResult {
                cursor: Default::default(),
                status: ApplyHunkStatus::NoChange,
            }),
        }))
        .join(" ")
    };

    let stage = toast(&mut s, HunkAction::Stage, ApplyScope::Cursor);
    let unstage = toast(&mut s, HunkAction::Unstage, ApplyScope::Cursor);
    assert!(
        stage.contains("stage") && !stage.contains("unstage"),
        "got {stage:?}"
    );
    assert!(unstage.contains("unstage"), "got {unstage:?}");
    assert_ne!(
        stage, unstage,
        "the two directions must not share a sentence"
    );

    // The file scope says so, so "nothing here" can't be read as "nothing at the cursor".
    assert!(toast(&mut s, HunkAction::Unstage, ApplyScope::File).contains("in this file"));
}

/// `Esc` on the git sub-leader backs out — it must never be a verb.
///
/// The sub-leader cancels on any *unbound* second key, so binding `Esc` to something would silently
/// make it the one key on `Space g` that acts instead of escaping. Cancelling an operation lives on
/// `x` for exactly this reason, and this pins both halves.
#[test]
fn esc_cancels_the_git_leader_rather_than_acting() {
    use aether_client::session::Pending;
    let mut s = session();
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    assert!(matches!(s.view.pending, Pending::LeaderGit));

    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert!(
        find_request(&fx, "git/cancel").is_none(),
        "Esc must not dispatch a git verb"
    );
    assert!(
        matches!(s.view.pending, Pending::None),
        "Esc backs out of the sub-leader"
    );

    // And the verb itself is on `x`. With nothing in flight it sends nothing, which is the
    // no-op case rather than an error.
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = key(&mut s, 'x');
    assert!(find_request(&fx, "git/cancel").is_none());
    assert!(
        matches!(s.view.pending, Pending::None),
        "the chord completed"
    );
}

/// The push outcomes that carry a next step say what it is. A `Behind` refusal in particular is
/// the reason that status exists — git's own text here is several lines of hint, and what the user
/// needs is the number and the verb.
#[test]
fn push_outcomes_name_their_next_step() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitPushResult, GitPushStatus, GitUpstreamStatus};

    let mut s = session();
    let origin = || {
        Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 0,
            behind: 0,
        })
    };

    // A first push names the tracking it just established — the moment the arrows start working.
    let fx = s.on_event(Event::PushDone(Ok(GitPushResult {
        status: GitPushStatus::Pushed,
        message: String::new(),
        upstream: origin(),
        set_upstream: true,
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("tracking") && msg.contains("origin/main"),
        "first push should say it set up tracking, got {msg:?}"
    );

    // An ordinary push just says where the commits went.
    let fx = s.on_event(Event::PushDone(Ok(GitPushResult {
        status: GitPushStatus::Pushed,
        message: String::new(),
        upstream: origin(),
        set_upstream: false,
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(msg.contains("origin/main") && !msg.contains("tracking"));

    // Behind: the count and the verb, not git's hint block.
    let fx = s.on_event(Event::PushDone(Ok(GitPushResult {
        status: GitPushStatus::Behind,
        message: "hint: Updates were rejected because...".into(),
        upstream: Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead: 1,
            behind: 3,
        }),
        set_upstream: false,
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains('3') && msg.to_lowercase().contains("fetch"),
        "behind should name the gap and the next step, got {msg:?}"
    );
    assert!(!has_error_toast(&fx), "this is actionable, not an error");
    // …and the split puts the gap in the title with the next step under it, rather than running
    // both together on one line.
    assert_eq!(
        toast_parts(&fx),
        vec![(
            "3 behind origin/main".to_string(),
            Some("Fetch and merge first".to_string())
        )]
    );

    // Anything git refused for a reason we didn't classify keeps its own words — as the detail
    // line, under a title naming what was refused.
    let fx = s.on_event(Event::PushDone(Ok(GitPushResult {
        status: GitPushStatus::Refused,
        message: "remote: protected branch".into(),
        upstream: None,
        set_upstream: false,
    })));
    assert!(has_error_toast(&fx));
    assert_eq!(
        toast_parts(&fx),
        vec![(
            "Push refused".to_string(),
            Some("remote: protected branch".to_string())
        )]
    );
}

/// `Alt-f` is pull, and it must not collide with plain `f` (fetch) — the two differ only by the
/// modifier, and the one that moves the working tree is the one it would be worst to fire by
/// accident.
#[test]
fn space_g_p_pulls_and_f_still_fetches() {
    let mut s = session();
    let fx = git_leader(&mut s, 'p');
    let req = find_request(&fx, "git/pull").expect("git/pull fired");
    assert!(req.get("repo_id").is_none_or(|v| v.is_null()));
    assert!(req.get("buffer_id").is_some());
    assert!(
        find_request(&fx, "git/fetch").is_none(),
        "pull must not also fetch"
    );

    // Fetch keeps its own key: it's the one remote verb that moves nothing.
    let fx = git_leader(&mut s, 'f');
    assert!(find_request(&fx, "git/fetch").is_some());
    assert!(find_request(&fx, "git/pull").is_none());
}

/// What a pull did to local history is the thing the user can't see for themselves, so each outcome
/// gets its own sentence. The three moves in particular are three different things to have happened
/// to their commits.
#[test]
fn pull_outcomes_name_what_happened_to_local_history() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitPullResult, GitPullStatus, GitRefreshResult, GitUpstreamStatus};

    let mut s = session();
    let origin = |ahead, behind| {
        Some(GitUpstreamStatus {
            name: "origin/main".into(),
            ahead,
            behind,
        })
    };
    let done = |status, upstream, refreshed| {
        Event::PullDone(Ok(GitPullResult {
            status,
            upstream,
            refreshed,
            ..Default::default()
        }))
    };

    // A fast-forward names the upstream and the buffers it disturbed — the count is the only
    // warning that content changed underneath open windows.
    let fx = s.on_event(done(
        GitPullStatus::FastForwarded,
        origin(0, 0),
        GitRefreshResult {
            reloaded: vec![1, 2],
            ..Default::default()
        },
    ));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("Fast-forwarded") && msg.contains("origin/main") && msg.contains('2'),
        "got {msg:?}"
    );

    // Merge and rebase are distinguishable in the wording, because they are distinguishable in
    // what happened to the user's commits.
    let fx = s.on_event(done(
        GitPullStatus::Merged,
        origin(1, 0),
        GitRefreshResult::default(),
    ));
    assert!(toast_messages(&fx).join(" ").contains("Merged"));
    let fx = s.on_event(done(
        GitPullStatus::Rebased,
        origin(1, 0),
        GitRefreshResult::default(),
    ));
    assert!(toast_messages(&fx).join(" ").contains("Rebased"));

    // Up to date is information, not a success worth celebrating.
    let fx = s.on_event(done(
        GitPullStatus::UpToDate,
        origin(0, 0),
        GitRefreshResult::default(),
    ));
    assert!(toast_messages(&fx).join(" ").contains("up to date"));

    // Conflicts name the files, because the next step is to open one. Not an error toast: the
    // pull did something, and the user has work to do rather than a fault to report.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Conflicted,
        message: "CONFLICT (content): Merge conflict in a.rs".into(),
        conflicts: vec!["a.rs".into(), "b.rs".into()],
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("a.rs") && msg.contains("b.rs") && msg.contains("Conflict"),
        "got {msg:?}"
    );
    assert!(!has_error_toast(&fx));

    // Diverged is push's `Behind` seen from the other side: the counts and the verb, not git's
    // several lines of hint text.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Diverged,
        message: "hint: You have divergent branches...".into(),
        upstream: origin(2, 3),
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("Diverged") && msg.contains('2') && msg.contains('3'),
        "got {msg:?}"
    );
    assert!(!has_error_toast(&fx));

    // The refusal whose fix is another action on this same sub-leader. Named as an *act*, not a
    // chord: a toast that spells a keybinding goes stale the moment the keymap moves, and key
    // discovery is the hint system's job.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::NoUpstream,
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.to_lowercase().contains("push"),
        "points at the fix: {msg}"
    );
    assert!(!msg.contains("Space"), "without naming a key: {msg}");

    // The pre-flight refusal points at saving, exactly as checkout's does.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::BlockedByDirtyBuffers,
        blocked: vec![4],
        ..Default::default()
    })));
    assert!(toast_messages(&fx)
        .join(" ")
        .to_lowercase()
        .contains("save first"));

    // Anything we didn't classify keeps git's own words.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Refused,
        message: "error: Your local changes would be overwritten".into(),
        ..Default::default()
    })));
    assert!(has_error_toast(&fx));
    assert!(toast_messages(&fx)
        .join(" ")
        .contains("would be overwritten"));
}

/// The states a repo can be *left* in, as opposed to the pull that left it there. Each names the
/// operation, because "resolve, then commit the merge" and "resolve, then continue the rebase" are
/// different instructions and guessing costs the user a wrong command.
#[test]
fn pull_reports_a_repo_left_mid_operation() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitPullResult, GitPullStatus, GitRepoOperation};

    let mut s = session();

    // A rebase that stopped: the follow-up is `--continue`, not a commit.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Conflicted,
        conflicts: vec!["a.rs".into()],
        operation: Some(GitRepoOperation::Rebase),
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("continue the rebase") && msg.contains("a.rs"),
        "got {msg:?}"
    );

    // The same conflict from a merge gets the other instruction.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Conflicted,
        conflicts: vec!["a.rs".into()],
        operation: Some(GitRepoOperation::Merge),
        ..Default::default()
    })));
    assert!(toast_messages(&fx).join(" ").contains("commit the merge"));

    // Pulling again while still stopped names the operation and what's left to resolve — the
    // state the user has forgotten they're in, which is why the pull made no sense.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::OperationInProgress,
        operation: Some(GitRepoOperation::Rebase),
        conflicts: vec!["a.rs".into()],
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("rebasing") && msg.contains("a.rs"),
        "got {msg:?}"
    );

    // Stopped with nothing left conflicted — resolved and staged but never committed. Nothing to
    // point at, so it says what to do instead.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::OperationInProgress,
        operation: Some(GitRepoOperation::Merge),
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("merging") && msg.to_lowercase().contains("abandon"),
        "got {msg:?}"
    );

    // A cancel that stranded the index lock is a warning naming the lock, not a bare
    // acknowledgement — until it's gone, every git operation in the repo fails.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Cancelled,
        index_locked: true,
        ..Default::default()
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(msg.contains("index.lock"), "got {msg:?}");
    //...and an ordinary cancel still says nothing alarming.
    let fx = s.on_event(Event::PullDone(Ok(GitPullResult {
        status: GitPullStatus::Cancelled,
        ..Default::default()
    })));
    assert!(!toast_messages(&fx).join(" ").contains("index.lock"));
}

/// Staging a *hunk* inside a conflicted file is refused with the reason, because the silent
/// alternative was `git add`'s "mark resolved" — a different operation from the one the key names.
/// The refusal points at the keys that do mean something here, so it's a signpost, not a wall.
#[test]
fn staging_a_conflicted_file_explains_the_refusal() {
    use aether_client::update::Event;
    use aether_protocol::git::{ApplyHunkStatus, ApplyScope, GitApplyHunkResult, HunkAction};

    let mut s = session();
    let toast = |s: &mut Session, status| {
        toast_messages(&s.on_event(Event::HunkApplied {
            action: HunkAction::Stage,
            scope: ApplyScope::File,
            result: Ok(GitApplyHunkResult {
                cursor: Default::default(),
                status,
            }),
        }))
        .join(" ")
    };

    let msg = toast(&mut s, ApplyHunkStatus::Conflicted);
    assert!(
        msg.contains("Conflicted") && msg.to_lowercase().contains("resolve"),
        "got {msg:?}"
    );
    // The two outcomes of the whole-file key on a conflicted path.
    assert!(toast(&mut s, ApplyHunkStatus::Resolved).contains("Marked resolved"));
    let msg = toast(&mut s, ApplyHunkStatus::MarkersRemain);
    assert!(msg.contains("markers"), "got {msg:?}");
}

/// Taking a side names the side and what's left, because the buffer just changed under the user and
/// the count decides what they do next: visit the next block, or mark the file resolved.
#[test]
fn resolving_a_conflict_names_the_side_and_what_remains() {
    use aether_client::update::Event;
    use aether_protocol::git::{ConflictSide, GitResolveConflictResult, ResolveConflictStatus};

    let result = |resolved, remaining| {
        Ok(GitResolveConflictResult {
            cursor: Default::default(),
            status: ResolveConflictStatus::Resolved,
            resolved,
            remaining,
        })
    };

    let mut s = session();
    let msg = toast_messages(&s.on_event(Event::ConflictResolved {
        side: ConflictSide::Theirs,
        result: result(1, 2),
    }))
    .join(" ");
    // Positional wording, matching the keys: "theirs" is exactly the word that would be wrong
    // mid-rebase, where the bottom section holds the user's own commit.
    assert!(
        msg.contains("bottom section") && msg.contains("2 conflicts left"),
        "got {msg:?}"
    );
    assert!(
        !msg.contains("theirs"),
        "ours/theirs must not resurface: {msg:?}"
    );

    // Reaching zero is the signal the file is done, and a multi-block take says how many it took.
    let msg = toast_messages(&s.on_event(Event::ConflictResolved {
        side: ConflictSide::Both,
        result: result(2, 0),
    }))
    .join(" ");
    assert!(
        msg.contains("both sections")
            && msg.contains("2 conflicts")
            && msg.contains("No conflicts left"),
        "got {msg:?}"
    );

    // Nowhere near a block: says so rather than reporting a resolution that didn't happen.
    let msg = toast_messages(&s.on_event(Event::ConflictResolved {
        side: ConflictSide::Ours,
        result: Ok(GitResolveConflictResult::default()),
    }))
    .join(" ");
    assert!(msg.contains("No conflict here"), "got {msg:?}");
}

/// The two ends of concluding an operation: `Space g d` abandons it, and a commit that resumed
/// one says so — including when the resumed operation stopped again, which is a success and a
/// to-do list at the same time.
#[test]
fn finishing_an_operation_reports_what_happened_to_it() {
    use aether_client::update::Event;
    use aether_protocol::git::{
        CommitInfo, GitAbortOperationResult, GitAbortStatus, GitCommitResult, GitRepoOperation,
    };

    let mut s = session();
    let msg = toast_messages(
        &s.on_event(Event::OperationAborted(Ok(GitAbortOperationResult {
            status: GitAbortStatus::Aborted,
            operation: Some(GitRepoOperation::Rebase),
            ..Default::default()
        }))),
    )
    .join(" ");
    assert!(msg.contains("Abandoned the rebase"), "got {msg:?}");

    // On a clean repo the way-out key says so rather than failing.
    let msg = toast_messages(&s.on_event(Event::OperationAborted(Ok(
        GitAbortOperationResult::default(),
    ))))
    .join(" ");
    assert!(msg.contains("Nothing in progress"), "got {msg:?}");

    // A rebase that hit the next conflict: the commit worked, and there is more to do.
    let commit = CommitInfo {
        commit: "abcdef1234".into(),
        author: "Test".into(),
        email: "t@e.com".into(),
        date: "2026-08-19 12:00:00 +0100".into(),
        message: "my edit".into(),
    };
    let msg = toast_messages(&s.on_event(Event::Committed(Ok(GitCommitResult {
        commit: Some(commit),
        operation: Some(GitRepoOperation::Rebase),
        conflicts: vec!["a.rs".into()],
        ..Default::default()
    }))))
    .join(" ");
    assert!(
        msg.contains("Committed") && msg.contains("rebase stopped at") && msg.contains("a.rs"),
        "got {msg:?}"
    );
}

/// Only one announced git operation at a time: the indicator and `Space g x` are both single-slot,
/// so a second would leave no way to say which one to stop.
#[test]
fn a_second_git_operation_is_refused_while_one_is_running() {
    use aether_protocol::git::{GitOperation, GitOperationKind};

    let mut s = session();
    s.git_operation = Some((
        "/repo".to_string(),
        GitOperation {
            kind: GitOperationKind::Pull,
            detail: String::new(),
        },
    ));

    for (ch, mods, method) in [
        ('f', Mods::NONE, "git/fetch"),
        ('p', Mods::NONE, "git/pull"),
        ('p', Mods::ALT, "git/push"),
    ] {
        let _ = key(&mut s, ' ');
        let _ = key(&mut s, 'g');
        let text = (mods == Mods::NONE).then(|| ch.to_string());
        let fx = s.on_key(KeyCode::Char(ch), mods, text);
        assert!(
            find_request(&fx, method).is_none(),
            "{method} must not start while another operation runs"
        );
        assert!(toast_messages(&fx).join(" ").contains("already running"));
    }

    // Cleared, it works again.
    s.git_operation = None;
    let fx = git_leader(&mut s, 'p');
    assert!(find_request(&fx, "git/pull").is_some());
}

#[test]
fn space_k_toggles_keep_and_guards_unsaved() {
    let mut s = session();

    // Clean transient view: Space k pins it permanent (transient: false).
    s.view.view_transient = true;
    s.view.buffer.revision = 3;
    s.view.buffer.saved_revision = 3;
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let params = find_request(&fx, "view/set_transient").expect("Space k toggles transient");
    assert_eq!(params["view_id"], json!(s.view.view_id));
    assert_eq!(
        params["transient"],
        json!(false),
        "pins the transient buffer permanent"
    );

    // Clean permanent view: Space k releases it back to transient.
    s.view.view_transient = false;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let params = find_request(&fx, "view/set_transient").expect("toggles the other way");
    assert_eq!(params["transient"], json!(true));

    // Dirty permanent view: Space k refuses to make it transient — silent no-op, no RPC.
    s.view.view_transient = false;
    s.view.buffer.revision = 5;
    s.view.buffer.saved_revision = 3;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    assert!(
        find_request(&fx, "view/set_transient").is_none(),
        "an unsaved buffer can't be made transient"
    );
    assert!(fx.0.is_empty(), "the refusal is a silent no-op");

    // A dirty *transient* view can still be pinned permanent — that's safe (stops it auto-closing
    // with the unsaved edits), so the guard only blocks the make-transient direction.
    s.view.view_transient = true;
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let params = find_request(&fx, "view/set_transient").expect("dirty transient can be pinned");
    assert_eq!(params["transient"], json!(false));
}

/// Keep is per **view**, and `Space u` changes how the view is seen, not the view: a kept file
/// read is still kept, and `Space k` addresses the same view before and after the flip.
#[test]
fn space_u_leaves_the_views_keep_state_alone() {
    let mut s = md_session();
    s.view.view_transient = true; // a preview
    let view = s.view.view_id;
    let fx = leader(&mut s, 'u');
    let token = the_read_request(&s, &fx, true);
    let _ = s.on_rpc_result(token, Ok(read_set(true)));
    assert_eq!(s.view.view_id, view, "the same view");
    assert!(
        s.view.view_transient,
        "still a preview: reading is not a keep"
    );

    // `Space k` keeps it — the view, whichever way it is being seen — and says so.
    let fx = leader(&mut s, 'k');
    let params = find_request(&fx, "view/set_transient").expect("Space k toggles the view");
    assert_eq!(params["view_id"], json!(view.get()));
    assert_eq!(params["transient"], json!(false), "kept, not released");
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(json!({ "transient": false })));
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast { title, .. } if title == "View kept")),
        "the toast agrees with the state"
    );
    assert!(!s.view.view_transient);
}

/// `Space k` inside a **review** keeps the document under the cursor, and never toggles.
///
/// A commit's patch and the working changes are composed views over real files, each windowed by
/// a preview of its own. The review keeps the flag it was created with, so addressing it does
/// nothing a user wants; what is worth outliving it is the file being read. So the request is a
/// keep even though the view is a preview, the answer names the file, and the **review's** flag
/// is left exactly where it was — it is still the preview that closes when you leave it.
#[test]
fn space_k_in_a_review_keeps_the_document_under_the_cursor() {
    let mut s = session();
    // A composed view: its identity is the patch, the cursor is in one of the files it windows.
    s.view.view_id = ViewId(9);
    s.view.view_buffer = 9;
    s.view.view_transient = true;
    s.view.buffer.buffer_id = 42;
    s.view.buffer.label = "a.rs".into();
    s.view.buffer.revision = 1;
    s.view.buffer.saved_revision = 1;

    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/set_transient");
    assert_eq!(
        params["view_id"],
        json!(9),
        "the view id is the handle; the server resolves the focused document behind it"
    );
    assert_eq!(
        params["transient"],
        json!(false),
        "a keep, though the view itself is transient — never a toggle"
    );

    // The server kept the file and answers its flag.
    let fx = s.on_rpc_result(token, Ok(json!({ "transient": false })));
    let toasts = toast_messages(&fx);
    assert!(
        toasts.iter().any(|t| t.starts_with("Kept a.rs")),
        "the toast names the document that was kept: {toasts:?}"
    );
    assert!(
        s.view.view_transient,
        "the review is as transient as it was: the reply said nothing about it"
    );
}

/// A shell and a conversation *are* the view — the client can tell (their window has an input
/// element), so `Space k` says so on the spot rather than paying a round trip to be told the
/// same. The server refuses it too; this is the reply, not the rule.
#[test]
fn space_k_in_a_shell_refuses_without_asking() {
    for focused in [0, 1] {
        let mut s = shell_session(focused);
        let fx = leader(&mut s, 'k');
        assert!(
            no_request(&fx),
            "focused element {focused}: nothing is sent"
        );
        let toasts = toast_messages(&fx);
        assert!(
            toasts
                .iter()
                .any(|t| t.starts_with("Only a document can be kept")),
            "focused element {focused}: it says why: {toasts:?}"
        );
    }
}

/// Only a document can be kept. A composed view — a patch, a shell, a conversation — keeps the
/// flag it was created with, and the server says so by answering the **actual** flag rather than
/// failing. The echo differing from what was asked is the refusal: the client says why, and the
/// keep state stays where the server left it.
#[test]
fn space_k_toasts_when_the_server_keeps_the_flag() {
    let mut s = session();
    s.view.view_id = ViewId(9);
    s.view.view_buffer = 9;
    // A patch: a preview, and staying one.
    s.view.view_transient = true;
    // The cursor is in the patch's own generated text — its metadata block — so there is no
    // windowed document to redirect to and the request is about the view.
    s.view.buffer.buffer_id = 9;
    s.view.buffer.revision = 1;
    s.view.buffer.saved_revision = 1;

    let fx = leader(&mut s, 'k');
    let (token, _, params) = the_request(&fx);
    assert_eq!(params["transient"], json!(false), "it still asks");

    // The server answers with the flag unchanged.
    let fx = s.on_rpc_result(token, Ok(json!({ "transient": true })));
    assert!(
        s.view.view_transient,
        "the keep state is the server's answer, not the request"
    );
    let toasts = toast_messages(&fx);
    assert!(
        toasts
            .iter()
            .any(|t| t.starts_with("Only a document can be kept")),
        "the refusal is explained: {toasts:?}"
    );
    assert!(
        !toasts.iter().any(|t| t.contains("View kept")),
        "and never claims it worked: {toasts:?}"
    );
}

/// The unsaved guard is **view-wide**: closing a view drops every document it windows, so a dirty
/// element elsewhere blocks making it transient just as the focused one does.
#[test]
fn space_k_refuses_a_view_with_another_element_dirty() {
    let mut s = session();
    s.view.view_transient = false;
    // The focused element is clean...
    s.view.buffer.revision = 3;
    s.view.buffer.saved_revision = 3;
    // ...but the view knows something else in it is not.
    s.view.window = Some(aether_protocol::viewport::Window {
        other_elements_dirty: true,
        max_line_width: 0,
        git_status: None,
        root: aether_protocol::viewport::Element::Editor {
            collapsed: false,
            element: 0,
            buffer: 0,
            rows: 0,
            first_row: aether_protocol::coords::ElementRow(0),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            first_buffer_line: 0,
            lines: vec![],
        },
    });

    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    assert!(
        find_request(&fx, "view/set_transient").is_none(),
        "a view with unsaved work anywhere in it can't be made transient"
    );
    assert!(fx.0.is_empty(), "the refusal is a silent no-op");
}

#[test]
fn reload_moved_to_space_alt_k() {
    let mut s = session();
    s.view.buffer.path = Some("/p/file.rs".into()); // reload needs a file-backed buffer

    // Reload now lives on Space Alt-k.
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    assert!(
        find_request(&fx, "buffer/reload").is_some(),
        "Space Alt-k reloads"
    );

    //...and its old home, Space a, no longer reloads. (It's unbound outright now stage-hunk
    // has moved to the git sub-leader, so this doubles as the ignore-an-unbound-key path.)
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('a'), Mods::NONE, Some("a".into()));
    assert!(
        find_request(&fx, "buffer/reload").is_none(),
        "Space a is no longer bound to reload"
    );
}

#[test]
fn space_p_copies_relative_and_absolute_paths() {
    let mut s = session();
    s.workspace_paths = vec!["/proj".into()];
    s.view.buffer.path = Some("/proj/src/main.rs".into());

    // Space p → workspace-relative path.
    let _ = key(&mut s, ' '); // leader
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()));
    assert_eq!(written_clipboard(&fx).as_deref(), Some("src/main.rs"));

    // Space Alt-p → absolute path.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::ALT, None);
    assert_eq!(written_clipboard(&fx).as_deref(), Some("/proj/src/main.rs"));
}

#[test]
fn space_p_multi_root_copies_bare_relative_path() {
    let mut s = session();
    s.workspace_paths = vec!["/proj/alpha".into(), "/proj/beta".into()];
    s.view.buffer.path = Some("/proj/beta/src/main.rs".into());

    // Unlike the status-bar label, the copied path carries no `root:` prefix.
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()));
    assert_eq!(written_clipboard(&fx).as_deref(), Some("src/main.rs"));
}

#[test]
fn copy_path_warns_for_scratch_buffer() {
    let mut s = session();
    s.view.buffer.path = None; // a scratch buffer
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('p'), Mods::NONE, Some("p".into()));
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

// ---- application settings (Space,) -----------------------------------------------------------

#[test]
fn app_settings_overlay_opens_via_leader_comma() {
    let mut s = session();
    let _ = key(&mut s, ' '); // leader
    s.on_key(KeyCode::Char(','), Mods::NONE, Some(','.to_string()));
    assert!(
        s.app_settings.is_some(),
        "Space , opens the app-settings overlay"
    );
    // Its neighbour on `.` is the workspace-scoped overlay — a distinct chord.
    assert!(s.workspace_settings.is_none());

    let mut s = session();
    let _ = key(&mut s, ' ');
    s.on_key(KeyCode::Char('.'), Mods::NONE, Some('.'.to_string()));
    assert!(
        s.workspace_settings.is_some(),
        "Space . opens the workspace-settings overlay"
    );
    assert!(s.app_settings.is_none());
}

#[test]
fn app_settings_esc_closes_the_overlay() {
    let mut s = session();
    s.open_app_settings();
    assert!(s.app_settings.is_some());
    s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert!(s.app_settings.is_none());
}

#[test]
fn app_settings_toggle_persists_and_reflows() {
    use aether_protocol::viewport::WrapMode;

    let mut s = session();
    assert_eq!(s.wrap, WrapMode::Soft);
    s.open_app_settings();
    // Enter on the (single) soft-wrap row.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);

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

#[test]
fn app_settings_apply_and_toggle_theme() {
    use aether_client::update::Event;
    use aether_protocol::settings::{AppSettings, ThemeMode};

    // Theme defaults dark; a persisted `light` is adopted with no shell effect (render-only —
    // the shells re-resolve their role table on the next frame).
    let mut s = session();
    assert_eq!(s.theme, ThemeMode::Dark);
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        theme: ThemeMode::Light,
        ..AppSettings::default()
    })));
    assert_eq!(s.theme, ThemeMode::Light, "persisted theme is adopted");
    assert!(fx.0.is_empty(), "theme is render-only — no shell action");

    // The row renders as a "Light theme" toggle whose checked state tracks the mode, and
    // toggling flips the mode back to dark + persists via settings/set.
    s.open_app_settings();
    let rows = s.app_setting_rows();
    let idx = rows
        .iter()
        .position(|r| matches!(r.id, aether_client::session::AppSettingId::Theme))
        .expect("a Theme row");
    assert_eq!(
        rows[idx].control,
        aether_client::session::AppSettingControl::Toggle(true),
        "checked while light"
    );
    let fx = s.app_settings_toggle(idx);
    assert_eq!(s.theme, ThemeMode::Dark, "toggle flips back to dark");
    let params = find_request(&fx, "settings/set").expect("settings/set fired");
    assert_eq!(params["theme"], json!("dark"));
}

/// The reading-view width setting: a three-way choice, so the row cycles on activate and steps
/// (clamping) on Left/Right — the same two gestures the font-size rows use, and the reason the
/// stepper isn't a toggle.
#[test]
fn app_settings_apply_and_cycle_markdown_width() {
    use aether_client::keymap::{KeyCode, Mods};
    use aether_client::session::{AppSettingControl, AppSettingId};
    use aether_client::update::Event;
    use aether_protocol::settings::{AppSettings, MarkdownWidth};

    // Narrow by default; a persisted width is adopted with no shell effect — every shell resolves
    // the measure on its next frame, so there's nothing to reflow.
    let mut s = session();
    assert_eq!(s.markdown_width, MarkdownWidth::Narrow);
    let fx = s.on_event(Event::AppSettingsLoaded(Ok(AppSettings {
        markdown_width: MarkdownWidth::Wide,
        ..AppSettings::default()
    })));
    assert_eq!(
        s.markdown_width,
        MarkdownWidth::Wide,
        "persisted width adopted"
    );
    assert!(fx.0.is_empty(), "reading width is render-only");

    s.open_app_settings();
    let idx = s
        .app_setting_rows()
        .iter()
        .position(|r| r.id == AppSettingId::MarkdownWidth)
        .expect("a MarkdownWidth row");
    assert_eq!(
        s.app_setting_rows()[idx].control,
        AppSettingControl::Choice("Wide"),
        "the row shows where the setting stands"
    );

    // Activating cycles wide → full → narrow → wide, persisting each step: pressing Enter
    // repeatedly must reach every option and come back, or an option would be unreachable.
    for want in [
        MarkdownWidth::Full,
        MarkdownWidth::Narrow,
        MarkdownWidth::Wide,
    ] {
        let fx = s.app_settings_toggle(idx);
        assert_eq!(s.markdown_width, want, "activate cycles to {want:?}");
        let params = find_request(&fx, "settings/set").expect("settings/set fired");
        assert_eq!(params["markdown_width"], json!(to_tag(want)));
    }

    // Left/Right step without wrapping, and clamp at the ends — a stepper, not a cycle.
    let fx = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.markdown_width, MarkdownWidth::Narrow, "Left narrows");
    assert!(find_request(&fx, "settings/set").is_some());
    let fx = s.on_key(KeyCode::Left, Mods::NONE, None);
    assert_eq!(s.markdown_width, MarkdownWidth::Narrow, "clamped at narrow");
    assert!(
        find_request(&fx, "settings/set").is_none(),
        "an unchanged width doesn't write settings.toml"
    );
    for want in [
        MarkdownWidth::Wide,
        MarkdownWidth::Full,
        MarkdownWidth::Full,
    ] {
        let _ = s.on_key(KeyCode::Right, Mods::NONE, None);
        assert_eq!(s.markdown_width, want, "Right widens then clamps at full");
    }
}

/// The wire tag for a reading width, for asserting what `settings/set` carried.
fn to_tag(width: aether_protocol::settings::MarkdownWidth) -> &'static str {
    use aether_protocol::settings::MarkdownWidth as W;
    match width {
        W::Narrow => "narrow",
        W::Wide => "wide",
        W::Full => "full",
    }
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
            unsaved: 0,
            match_indices: vec![],
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
                unsaved: 0,
                match_indices: vec![],
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

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
                Effect::Request { token, method, .. } if *method == "view/open" => Some(*token),
                _ => None,
            })
            .expect("scratch view/open fired");

    // Tick between the create result and the scratch landing: workspace set, buffer still 0.
    let fx = s.on_hint_tick(1_000_000_006_000);
    no_bounce(&fx, "while scratch open in flight");

    let fx = s.on_rpc_result(
        open_token,
        Ok(json!({
            "buffer": 0,
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
            worktrees: Vec::new(),
            name: "fresh".into(),
            paths: vec![],
            projects: Vec::new(),
        },
        last_view_id: None,
        opened: None,
        server_started_at: 0,
    })));
    assert_eq!(s.workspace, "fresh");
    // Rather than leave the previous workspace's buffer behind, a scratch is opened (a `view/open`
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
        vec!["history/state", "view/open", "directory/list"],
        "opens a fresh scratch in the new workspace, and the auto-opened settings overlay asks for \
         its add-root completions up front"
    );
    // That listing is the *unrestricted* kind — this workspace has no roots at all, so a bounded
    // one would have nothing to complete against, which is precisely the case this flow lands in.
    let list = find_request(&fx, "directory/list").expect("the add-root row lists its seed");
    assert_eq!(list["path"], json!("~/"));
    assert_eq!(list["unrestricted"], json!(true));
    // The settings overlay auto-opens, focused on the add-root input (index = roots.len + 1 = 1).
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
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert!(s.workspace_settings.as_ref().unwrap().on_input());
    // The shell's input owns text entry and syncs the whole value; the core no longer key-edits.
    let _ = s.workspace_settings_set_add("/b".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let add = find_request(&fx, "workspace/add_root").expect("workspace/add_root fired");
    assert_eq!(add["workspace"], json!("aether"));
    assert_eq!(add["path"], json!("/b"));
    // The result updates the session roots + the overlay's roots and clears the input.
    let _ = s.on_event(Event::WorkspaceRootAdded(Ok(WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether".into(),
        paths: vec!["/a".into(), "/b".into()],
        projects: Vec::new(),
    })));
    assert_eq!(s.workspace_paths, vec!["/a".to_string(), "/b".to_string()]);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.roots.len(), 2);
    assert_eq!(
        ps.add.input.text, "~/",
        "the input resets to its seed after a successful add, so the next one starts where the \
         last did"
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

    // With one declared, a fresh open shows just the input — the query *is* the search
    // (like Grep), and nothing has been searched yet, so a note would read as a failed one.
    s.workspace_projects = vec![WorkspaceProject {
        path_index: 0,
        relative_path: "crates/core".into(),
        language: "rust".into(),
        error: None,
    }];
    let _ = s.open_picker(PickerKind::WorkspaceSymbols, None, None, false, None);
    s.picker.as_mut().unwrap().ticking = false;
    let note = s.picker.as_ref().unwrap().empty_note();
    assert!(
        note.is_none(),
        "an unqueried picker hasn't searched — expected no note, got {note:?}",
    );

    // Only a *typed* query that settled empty really is "no matches".
    {
        let p = s.picker.as_mut().unwrap();
        p.query = "zzz".into();
        p.ticking = false;
    }
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
    use aether_protocol::workspace::{WorkspaceInfo, WorkspaceProject};

    let mut s = Session::new(
        WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec!["/a".into()],
            projects: vec![WorkspaceProject {
                path_index: 0,
                relative_path: "crates/core".into(),
                language: "rust".into(),
                error: None,
            }],
        },
        aether_protocol::view::ViewOpenResult {
            view_id: aether_protocol::ViewId(1),
            scroll: None,
            transient: false,
            read: false,
            buffer: aether_protocol::view::BufferDescription {
                buffer_id: 1,
                language: None,
                line_count: 1,
                byte_count: 0,
                revision: 0,
                saved_revision: 0,
                path: Some("/a/a.rs".into()),
                scratch_number: None,
                cursor: aether_protocol::cursor::CursorState::default(),
                lsp_server: None,
                title: None,
                commit: None,
                read_only: false,
                is_patch: false,
            },
        },
    );
    assert_eq!(s.workspace_projects.len(), 1);

    //...and the settings overlay shows them without needing a workspace event first.
    s.open_workspace_settings();
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.projects.len(), 1);
    assert_eq!(ps.projects[0].relative_path, "crates/core");
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
        assert_eq!(s.workspace_settings.as_ref().unwrap().row(), expected);
    }

    // Inside the editor, Tab steps root → path rather than leaving the row — and *without*
    // adopting the root ghost, which is Alt-l's job. A partly-typed filter stays as typed.
    let _ = s.workspace_settings_set_add_project_root("be".into());
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.field, ChipEditorField::Root);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert_eq!(ps.add_project.field, ChipEditorField::Path);
    assert_eq!(
        ps.add_project.root_filter.text, "be",
        "Tab traverses; it does not complete the root to its full label",
    );

    // Alt-l is what adopts the ghost — same traversal, but the filter becomes the full label.
    s.on_key(KeyCode::BackTab, Mods::NONE, None);
    s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.root_filter.text, "beta");
    assert_eq!(ps.add_project.field, ChipEditorField::Path);

    //...and Shift-Tab walks back out the same way.
    s.on_key(KeyCode::BackTab, Mods::NONE, None);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.add_project.field, ChipEditorField::Root);
    s.on_key(KeyCode::BackTab, Mods::NONE, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::AddRoot,
    );

    // Wrapping backwards into the row lands on its *last* segment, so reverse traversal retraces
    // the forward path rather than skipping a field. From add-root (index 3) that's four steps:
    // root(1), root(0), name, then round to add-project.
    for _ in 0..4 {
        s.on_key(KeyCode::BackTab, Mods::NONE, None);
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
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
    s.on_key(KeyCode::Char('j'), Mods::ALT, None);
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
    s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Name,
        "Alt-j is the focused field's key, not the dialog's",
    );

    // The arrows are the non-chord alternative to Tab for people who want one.
    s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Root(0),
    );
    s.on_key(KeyCode::Up, Mods::NONE, None);
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
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
    s.on_key(KeyCode::Tab, Mods::NONE, None);
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
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
    s.on_key(KeyCode::Char('l'), Mods::ALT, None);
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
        s.on_key(KeyCode::Tab, Mods::NONE, None); // name → root → add-root → add-project
    }
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
            relative_path: "crates/core".into(),
            language: "rust".into(),
            error: None,
        },
        WorkspaceProject {
            path_index: 1,
            relative_path: "svc".into(),
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
    }
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert!(ps.on_input(), "both add rows count as text inputs");

    // Tab off the path enters the row's trailing language segment, still on the same row...
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    let ps = s.workspace_settings.as_ref().unwrap();
    assert_eq!(ps.row(), SettingsRow::AddProject);
    assert!(ps.on_add_project_language);

    //...and only Tab off *that* cycles round to the first field.
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().row(),
        SettingsRow::Name,
    );
    //...and Shift-Tab off the first wraps back to the last.
    s.on_key(KeyCode::BackTab, Mods::NONE, None);
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
        s.on_key(KeyCode::Tab, Mods::NONE, None);
    }
    let _ = s.workspace_settings_set_add_project("crates/core".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let add = find_request(&fx, "workspace/add_project").expect("workspace/add_project fired");
    assert_eq!(add["workspace"], json!("aether"));
    assert_eq!(add["path_index"], json!(0));
    assert_eq!(add["relative_path"], json!("crates/core"));
    assert!(
        add.get("language").is_none(),
        "no language sent — the server infers it from the marker"
    );

    let _ = s.on_event(Event::WorkspaceProjectAdded(Ok(WorkspaceInfo {
        worktrees: Vec::new(),
        name: "aether".into(),
        paths: vec!["/a".into()],
        projects: vec![WorkspaceProject {
            path_index: 0,
            relative_path: "crates/core".into(),
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
        relative_path: "crates/core".into(),
        language: "rust".into(),
        error: None,
    }];
    s.open_workspace_settings();
    // name → root → add-root → project(0).
    for _ in 0..3 {
        s.on_key(KeyCode::Tab, Mods::NONE, None);
    }
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None);
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

    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, None);
    let req = find_request(&fx, "workspace/remove_project").expect("remove fired on accept");
    assert_eq!(req["path_index"], json!(0));
    assert_eq!(req["relative_path"], json!("crates/core"));
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let rename = find_request(&fx, "workspace/rename").expect("workspace/rename fired");
    assert_eq!(rename["workspace"], json!("old"));
    assert_eq!(rename["new_name"], json!("oldx"));
    // The result reconciles the committed name in both the session and the overlay.
    let _ = s.on_event(Event::WorkspaceRenamed(Ok(WorkspaceInfo {
        worktrees: Vec::new(),
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
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert_eq!(s.workspace_settings.as_ref().unwrap().selected, 1);
    // Delete opens the shared confirm prompt for the highlighted root (no request yet).
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    let remove = find_request(&fx, "workspace/remove_root").expect("workspace/remove_root fired");
    assert_eq!(remove["workspace"], json!("aether"));
    assert_eq!(remove["path"], json!("/a"));
    assert!(s.prompt.is_none(), "the prompt closes on accept");
    // The result refreshes the roots.
    let _ = s.on_event(Event::WorkspaceRootRemoved(Ok(WorkspaceRemoveRootResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "aether".into(),
            paths: vec!["/b".into()],
            projects: Vec::new(),
        },
        closed_buffer_ids: vec![],
        next_view_id: None,
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
    assert_eq!(ps.add.input.text, "/new/root");
    // No-op outside the overlay.
    s.workspace_settings = None;
    let fx = s.workspace_settings_set_name("x".into());
    assert!(fx.0.is_empty());
}

/// Both add rows collapse to their affordance while unfocused and untouched — and "untouched"
/// means *still holding the seed*, not merely empty.
///
/// That distinction is the whole point: seeding add-root with `~/` made an emptiness test stop
/// firing, so the row rendered a bare `~/` where it used to say what it was for. Pinned in the core
/// because all three shells draw this from the one rule.
#[test]
fn add_rows_show_their_affordance_until_something_is_typed() {
    use aether_client::session::SettingsRow;
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    let _ = s.open_workspace_settings();

    // Open focuses the name field, so both add rows are unfocused — and the add-root row is holding
    // its `~/` seed, which must not count as content.
    {
        let ps = s.workspace_settings.as_ref().unwrap();
        assert_eq!(ps.add.input.text, "~/", "the seed is there...");
        assert_eq!(
            ps.add_placeholder(SettingsRow::AddRoot),
            Some("Add root..."),
            "...but an untouched field still says what the row is for"
        );
        assert_eq!(
            ps.add_placeholder(SettingsRow::AddProject),
            Some("Add project...")
        );
    }

    // Focusing the add-root row swaps the seed in — which is also when its completions matter.
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    {
        let ps = s.workspace_settings.as_ref().unwrap();
        assert_eq!(ps.row(), SettingsRow::AddRoot);
        assert_eq!(ps.add_placeholder(SettingsRow::AddRoot), None);
        // The other row is still untouched, so it keeps its label.
        assert_eq!(
            ps.add_placeholder(SettingsRow::AddProject),
            Some("Add project...")
        );
    }

    // Type something and navigate away: now there is content worth showing, so no placeholder.
    let _ = s.workspace_settings_set_add("~/code".into());
    s.on_key(KeyCode::BackTab, Mods::NONE, None);
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_placeholder(SettingsRow::AddRoot),
        None,
        "a typed path outlives the focus that produced it"
    );

    // Delete back down to the seed and the affordance returns — there is nothing invested either
    // way, so the label is the more useful thing to render.
    let _ = s.workspace_settings_set_add("~/".into());
    assert_eq!(
        s.workspace_settings
            .as_ref()
            .unwrap()
            .add_placeholder(SettingsRow::AddRoot),
        Some("Add root...")
    );
}

/// The add-root row completes path segments like every other path field — over **absolute** paths,
/// via the unrestricted listing, since the directory you are adding is by definition one the
/// workspace does not already contain.
#[test]
fn settings_add_root_completes_absolute_paths() {
    use aether_client::update::{Event, PathEditorOwner};
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    let mut s = session();
    s.workspace = "aether".into();
    // Multi-root on purpose: an absolute field must never grow a root segment.
    s.workspace_paths = vec!["/a".into(), "/b".into()];
    let fx = s.open_workspace_settings();

    // Opening asks for the seed's directory, unrestricted.
    let list = find_request(&fx, "directory/list").expect("the seed is listed on open");
    assert_eq!(list["path"], json!("~/"));
    assert_eq!(list["unrestricted"], json!(true));

    let _ = s.on_event(Event::PathEditorListing {
        owner: PathEditorOwner::AddRoot,
        abs: "~/".into(),
        result: Ok(DirectoryListResult {
            path: "/home/me".into(),
            parent: None,
            entries: vec![
                DirectoryEntry {
                    name: "Projects".into(),
                    is_dir: true,
                },
                DirectoryEntry {
                    name: ".bashrc".into(),
                    is_dir: false,
                },
            ],
        }),
    });

    // Tab to the row, then a typed prefix ghosts the directory — and only the directory: a root is
    // a directory, so the file is never offered.
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert!(s.workspace_settings.as_ref().unwrap().on_input());
    let _ = s.workspace_settings_set_add("~/Pro".into());
    {
        let ed = &s.workspace_settings.as_ref().unwrap().add;
        assert_eq!(ed.path_ghost().as_deref(), Some("jects/"));
        assert!(
            !ed.multi_root(&s.workspace_paths),
            "an absolute field has no root segment, whatever the workspace's root count"
        );
    }
    let _ = s.workspace_settings_set_add("~/.bash".into());
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().add.path_ghost(),
        None,
        "files are never offered for a root"
    );

    // Alt-l accepts the directory and re-lists one level down.
    let _ = s.workspace_settings_set_add("~/Pro".into());
    let fx = s.on_key(KeyCode::Char('l'), Mods::ALT, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().add.input.text,
        "~/Projects/"
    );
    let list = find_request(&fx, "directory/list").expect("accepting a dir re-lists");
    assert_eq!(list["path"], json!("~/Projects/"));
    assert_eq!(list["unrestricted"], json!(true));

    // And Enter commits the literal path, tilde intact — the server expands it.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let add = find_request(&fx, "workspace/add_root").expect("workspace/add_root fired");
    assert_eq!(add["path"], json!("~/Projects/"));
}

#[test]
fn settings_esc_closes_the_overlay() {
    let mut s = session();
    s.workspace = "aether".into();
    s.open_workspace_settings();
    assert!(s.workspace_settings.is_some());
    s.on_key(KeyCode::Esc, Mods::NONE, None);
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
            focus_run: None,
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
            focus_run: None,
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
/// leaves the context. A session *launched* for the file (`ae /path`) tethers to it, so the close
/// quits, vim-like.
#[test]
fn ephemeral_last_buffer_close_when_launched_quits() {
    let mut s = session();
    s.workspace = "ephemeral/1".to_string();
    // An ordinary view: its identity and the buffer it edits are the same id. Both are set
    // because closing addresses `view_id` while the tether names the edited buffer.
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);

    let fx = s.close_view();
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
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

    let fx = s.close_view();
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

    let fx = s.close_view();
    let (token, _, _) = the_request(&fx);

    let fx = s.on_rpc_result(token, Ok(json!({ "next_view_id": 5 })));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Exit)),
        "a remaining sibling means we stay, not quit"
    );
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/open");
    assert_eq!(params["view_id"], json!(5), "present the remaining view");
}

/// `Space x` asks for the landing in the same round-trip (`open_next`), and what comes back is
/// adopted as a **navigation**: the client lands on the successor with the cursor the result
/// carries and resubscribes so the shell frames it.
///
/// The server resolves that successor from this client's history now, so the result carries a
/// position and not merely a view to attach to — a close that dropped the cursor on the floor
/// would land you at the top of the file you stepped back to.
#[test]
fn space_x_lands_on_the_successor_the_close_hands_back() {
    use aether_protocol::cursor::CursorState;
    use aether_protocol::view::{BufferDescription, ViewOpenResult};
    use aether_protocol::LogicalPosition;

    let mut s = session();
    s.workspace = "proj".into();
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;

    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
    assert_eq!(params["view_id"], json!(7), "the view being closed");
    assert_eq!(
        params["open_next"],
        json!(true),
        "Space x wants the landing back with the close"
    );

    let cursor = CursorState {
        position: LogicalPosition { line: 12, col: 4 },
        anchor: LogicalPosition { line: 12, col: 4 },
        match_bracket: None,
        jumplist_position: None,
    };
    let landing = ViewOpenResult {
        view_id: ViewId(3),
        scroll: None,
        transient: false,
        read: false,
        buffer: BufferDescription {
            buffer_id: 3,
            language: None,
            line_count: 40,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: Some("/proj/came_from.rs".into()),
            scratch_number: None,
            cursor,
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
    };
    let fx = s.on_rpc_result(
        token,
        Ok(json!({ "next_view_id": 3, "opened": serde_json::to_value(&landing).unwrap() })),
    );
    assert_eq!(s.view.buffer.buffer_id, 3, "landed on the successor");
    assert_eq!(s.view.view_id, ViewId(3));
    assert_eq!(
        s.view.buffer.cursor.position,
        LogicalPosition { line: 12, col: 4 },
        "with the cursor the server restored"
    );
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "and a resubscribe, which is what frames it"
    );
}

// ---- the tether --------------------------------------------------------------

/// Closing a *composed* view that merely happens to be focused on the tethered file closes the
/// view, not the client.
///
/// The tether names a document; close addresses the view. Those coincide for an ordinary editor and
/// diverge the moment focus rebinds `view.buffer` to a file inside a patch — so `Space x` on a
/// working-changes view sitting on the tethered file used to exit, abandoning the file the `$EDITOR`
/// caller was still waiting on.
#[test]
fn closing_a_view_focused_on_the_tethered_file_does_not_exit() {
    let mut s = session();
    s.workspace = "proj".to_string();
    // A patch view (10) whose focused element windows the tethered file (7) — the one shape where
    // the view's identity and the buffer being edited are different ids.
    s.view.view_id = ViewId(10);
    s.view.view_buffer = 10;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);

    assert!(
        s.tethered(),
        "the buffer being edited IS the tether, so the status mark still shows"
    );
    assert!(
        !s.tethered_view(),
        "but the view is the patch, not the file"
    );

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
    assert_eq!(
        params["view_id"],
        json!(10),
        "the view closes, not the file"
    );
    assert_eq!(
        params["open_next"],
        json!(true),
        "an ordinary close, so the server picks a successor"
    );

    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(
        !quits(&fx),
        "the tethered file is still open; closing the patch must not end the session"
    );
}

/// A quick-edit session in a *real* workspace (`ae file`, workspace inferred — the git-commit
/// case): `Space x` on the tethered buffer exits the client instead of switching to a successor.
#[test]
fn closing_the_tether_in_a_workspace_context_exits() {
    let mut s = session();
    s.workspace = "proj".to_string();
    // An ordinary view: its identity and the buffer it edits are the same id. Both are set
    // because closing addresses `view_id` while the tether names the edited buffer.
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
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
    s.view.buffer.buffer_id = 7;

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
    assert_eq!(params["open_next"], json!(true), "adopt the MRU successor");
}

/// Another client closing the tether (the `view/closed` push) exits too — the contract the
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
            method: "view/closed".into(),
            params: json!({ "view_id": 7, "buffer_id": 7 }),
        })
    };

    // Viewing the tether when it closes.
    let mut s = session();
    s.workspace = "proj".to_string();
    // An ordinary view: its identity and the buffer it edits are the same id. Both are set
    // because closing addresses `view_id` while the tether names the edited buffer.
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);
    let fx = s.on_event(push());
    assert!(quits(&fx), "the tether closed out from under us — exit");

    // Browsing another buffer when the tether closes: still exit.
    let mut s = session();
    s.workspace = "proj".to_string();
    s.view.buffer.buffer_id = 9;
    s.tether = Some(7);
    let fx = s.on_event(push());
    assert!(quits(&fx), "exit even while viewing something else");

    // No tether: a push for a background buffer is ignored.
    let mut s = session();
    s.workspace = "proj".to_string();
    s.view.buffer.buffer_id = 9;
    let fx = s.on_event(push());
    assert!(!quits(&fx), "untethered clients ignore background closes");
}

/// `view/closed` with nothing to land on: the placeholder opened is transient, like every other
/// landing with nothing to return to — not a scratch the client now keeps.
#[test]
fn closed_under_us_with_nothing_left_lands_on_a_transient_placeholder() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification};
    let mut s = session();
    s.workspace = "proj".to_string();
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "view/closed".into(),
        params: json!({ "view_id": 7, "buffer_id": 7 }),
    }));
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/open");
    assert!(params.get("view_id").is_none(), "nothing to present");
    assert_eq!(
        params["transient"],
        json!(true),
        "a placeholder, not a kept scratch"
    );

    // Handed a view, it is presented as it is — no opinion on its transience.
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "view/closed".into(),
        params: json!({ "view_id": 7, "buffer_id": 7, "next_view_id": 9 }),
    }));
    let (_, _, params) = the_request(&fx);
    assert_eq!(params["view_id"], json!(9));
    assert!(params.get("transient").is_none());
}

/// Un-keeping the tethered buffer (`Space k`) releases the tether: the buffer demotes to an
/// ordinary transient and closing it no longer exits. One-way — a later re-keep is a plain keep,
/// not a re-arm.
#[test]
fn unkeep_releases_the_tether_one_way() {
    let mut s = session();
    s.workspace = "proj".to_string();
    // An ordinary view: its identity and the buffer it edits are the same id. Both are set
    // because closing addresses `view_id` while the tether names the edited buffer.
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);

    // `Space k` on the (clean) tether: one set_transient request, demoting the buffer.
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/set_transient");
    assert_eq!(params["view_id"], json!(7), "the tethered view");
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
    s.view.view_transient = true;
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/set_transient");
    assert_eq!(params["transient"], json!(false), "plain keep");
    assert_eq!(s.tether, None, "re-keeping does not re-arm the tether");

    // And closing now behaves like any ordinary buffer: successor, no exit.
    s.view.view_transient = false;
    let fx = s.close_view();
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
    s.view.buffer.buffer_id = 7;
    s.view.buffer.revision = 3;
    s.view.buffer.saved_revision = 2;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('k'), Mods::NONE, Some("k".into()));
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
    // An ordinary view: its identity and the buffer it edits are the same id. Both are set
    // because closing addresses `view_id` while the tether names the edited buffer.
    s.view.view_id = ViewId(7);
    s.view.view_buffer = 7;
    s.view.buffer.buffer_id = 7;
    s.tether = Some(7);

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let params = find_request(&fx, "view/save").expect("Space Alt-x saves first");
    assert_eq!(params["overwrite"], json!(false));
    assert!(!quits(&fx), "no exit before the save lands");
    let token = save_token(&fx);

    // Save lands → the close fires (tether style: no successor). Still no exit.
    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    let close_token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, method, .. } if *method == "view/close" => Some(*token),
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
    s.view.buffer.buffer_id = 7;

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let token = save_token(&fx);

    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    let close = fx.0.iter().find_map(|e| match e {
        Effect::Request { method, params, .. } if *method == "view/close" => Some(params.clone()),
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
    use aether_protocol::view::ViewOpenResult;
    use aether_protocol::workspace::WorkspaceInfo;

    let reopen = |path: &str, id: u64| -> ViewOpenResult {
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
        worktrees: Vec::new(),
        name: "proj".into(),
        paths: vec!["/p".into()],
        projects: vec![],
    };

    // Same file after a restart: the tether follows the new id.
    let mut s = session();
    s.view.buffer.buffer_id = 7;
    s.view.buffer.path = Some("/p/f.txt".into());
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
    s.view.buffer.buffer_id = 7;
    s.view.buffer.path = Some("/p/f.txt".into());
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

    let fx = s.close_view();
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/close");
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
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    use aether_protocol::view::{BufferDescription, ViewOpenResult};
    use aether_protocol::workspace::{WorkspaceActivateResult, WorkspaceInfo};

    let mut s = session();
    s.workspace = "proj".into();
    // Opening the overlay (what `A::OpenPath` does).
    s.prompt = Some(Prompt::OpenPath(Box::new(PathEditor::absolute(
        String::new(),
        true,
    ))));

    // The shell syncs typed text into the core.
    let _ = s.open_path_set_input("/etc/hosts".into());

    // Enter submits.
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "workspace/open_path");
    assert_eq!(params["path"], json!("/etc/hosts"));
    assert!(s.prompt.is_none(), "the overlay closes on submit");

    // The result lands like a switch: adopt the (resolved) workspace + opened buffer.
    let opened = ViewOpenResult {
        view_id: aether_protocol::ViewId(9),
        scroll: None,
        transient: false,
        read: false,
        buffer: BufferDescription {
            buffer_id: 9,
            language: None,
            line_count: 1,
            byte_count: 0,
            revision: 0,
            saved_revision: 0,
            path: Some("/etc/hosts".into()),
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
    };
    let result = serde_json::to_value(WorkspaceActivateResult {
        workspace: WorkspaceInfo {
            worktrees: Vec::new(),
            name: "proj".into(),
            paths: vec![],
            projects: Vec::new(),
        },
        last_view_id: None,
        opened: Some(opened),
        server_started_at: 0,
    })
    .unwrap();
    let fx = s.on_rpc_result(token, Ok(result));
    assert!(!has_error_toast(&fx));
    assert_eq!(s.view.buffer.buffer_id, 9, "adopted the opened buffer");
}

/// Esc cancels the open-from-path overlay without opening anything.
#[test]
fn open_path_prompt_esc_cancels() {
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    let mut s = session();
    s.workspace = "proj".into();
    s.prompt = Some(Prompt::OpenPath(Box::new(PathEditor::absolute(
        "/some/path".into(),
        true,
    ))));
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
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    let mut s = session();
    s.workspace = "proj".into();
    s.prompt = Some(Prompt::OpenPath(Box::new(PathEditor::absolute(
        "   ".into(),
        true,
    )))); // whitespace only
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        matches!(s.prompt, Some(Prompt::OpenPath(_))),
        "an empty submit leaves the overlay open"
    );
    assert!(!fx.0.iter().any(|e| matches!(e, Effect::Request { .. })));
}

/// The open-from-path overlay completes absolute paths, files included — it opens a file, so a file
/// is a valid answer here in a way it never is for a root.
///
/// The listing is the unrestricted kind, and this test runs with **no workspace roots at all**: the
/// bounded listing requires an active workspace, so it would refuse outright. That is the case this
/// overlay exists for.
#[test]
fn open_path_prompt_completes_absolute_paths_including_files() {
    use aether_client::session::Prompt;
    use aether_client::update::{Event, PathEditorOwner};
    use aether_protocol::directory::{DirectoryEntry, DirectoryListResult};
    let mut s = session();
    s.workspace_paths = Vec::new();

    // `Space Alt-w` opens it.
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('w'), Mods::ALT, None);
    let list = find_request(&fx, "directory/list").expect("opening lists the seed");
    assert_eq!(list["path"], json!("~/"));
    assert_eq!(list["unrestricted"], json!(true));

    let _ = s.on_event(Event::PathEditorListing {
        owner: PathEditorOwner::OpenPath,
        abs: "~/".into(),
        result: Ok(DirectoryListResult {
            path: "/home/me".into(),
            parent: None,
            entries: vec![
                DirectoryEntry {
                    name: "Projects".into(),
                    is_dir: true,
                },
                DirectoryEntry {
                    name: "notes.md".into(),
                    is_dir: false,
                },
            ],
        }),
    });

    let ghost = |s: &Session| match s.prompt.as_ref() {
        Some(Prompt::OpenPath(ed)) => ed.path_ghost(),
        _ => panic!("the overlay should still be open"),
    };
    // A directory completes with its trailing `/`...
    let _ = s.open_path_set_input("~/Pro".into());
    assert_eq!(ghost(&s).as_deref(), Some("jects/"));
    // ...and so does a file, outright — the difference from the add-root row.
    let _ = s.open_path_set_input("~/not".into());
    assert_eq!(ghost(&s).as_deref(), Some("es.md"));

    // Alt-l absorbs it, and Enter opens the literal path with the tilde intact.
    let _ = s.on_prompt_key(KeyCode::Char('l'), Mods::ALT, None);
    let fx = s.on_prompt_key(KeyCode::Enter, Mods::NONE, None);
    let open = find_request(&fx, "workspace/open_path").expect("workspace/open_path fired");
    assert_eq!(open["path"], json!("~/notes.md"));
}

/// Alt-Backspace in the open-from-path overlay pops one path segment, fish-style — the grain the
/// save-as prompt's path field already uses, because it holds the same kind of value.
#[test]
fn open_path_alt_backspace_pops_a_path_segment() {
    use aether_client::path_editor::PathEditor;
    use aether_client::session::Prompt;
    let mut s = session();
    s.workspace = "proj".into();
    s.prompt = Some(Prompt::OpenPath(Box::new(PathEditor::absolute(
        "/etc/nginx/conf.d".into(),
        true,
    ))));

    let text = |s: &Session| match s.prompt.as_ref() {
        Some(Prompt::OpenPath(ed)) => ed.input.text.clone(),
        _ => panic!("the overlay should still be open"),
    };

    let fx = s.on_prompt_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(text(&s), "/etc/nginx/");
    // The pop now re-lists: the field completes, so the suggestions have to follow it up the tree
    // rather than keep describing the directory you just left. (This assertion was the inverse
    // before the field gained completion — there was nothing to refresh.)
    let list = find_request(&fx, "directory/list").expect("popping a segment re-lists");
    assert_eq!(list["path"], json!("/etc/nginx/"));
    assert_eq!(list["unrestricted"], json!(true));
    let _ = s.on_prompt_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(text(&s), "/etc/");
    // Down to nothing, then a clean no-op — never a close.
    let _ = s.on_prompt_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(text(&s), "/");
    let _ = s.on_prompt_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(text(&s), "");
    let _ = s.on_prompt_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(text(&s), "");
}

/// The workspace-settings overlay's two plain fields take Alt-Backspace at their own grain: a word
/// in the name, a `/` segment in the add-root path.
#[test]
fn settings_alt_backspace_matches_each_fields_grain() {
    let mut s = session();
    s.workspace = "aether".into();
    s.workspace_paths = vec!["/a".into()];
    s.open_workspace_settings();

    // The name field (focused on open).
    assert!(s.workspace_settings.as_ref().unwrap().on_name());
    let _ = s.workspace_settings_set_name("my old project".into());
    s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(s.workspace_settings.as_ref().unwrap().name.text, "my old ");

    // Tab down to the add-root input (past the single root).
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    s.on_key(KeyCode::Tab, Mods::NONE, None);
    assert!(s.workspace_settings.as_ref().unwrap().on_input());
    let _ = s.workspace_settings_set_add("/home/me/code".into());
    s.on_key(KeyCode::Backspace, Mods::ALT, None);
    assert_eq!(
        s.workspace_settings.as_ref().unwrap().add.input.text,
        "/home/me/",
        "a path field pops a segment, not a word"
    );
}

// ---- sneak (s / S word-jump) --------------------------------------------------------------------

/// A session with a viewport, so `sneak/update` has an id to scope to.
fn session_with_viewport() -> Session {
    let mut s = session();
    s.view.viewport_id = Some(7);
    s
}

#[test]
fn sneak_arms_then_first_char_requests_update() {
    let mut s = session_with_viewport();
    // `s` arms the session but issues no traffic yet.
    let fx = key(&mut s, 's');
    assert!(s.view.sneak.is_some(), "sneak armed");
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
    assert_eq!(s.view.sneak.as_ref().unwrap().labels, vec!['a', 'b']);
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
    assert!(s.view.sneak.is_none(), "session ended on label press");
}

#[test]
fn sneak_shift_select_extends() {
    let mut s = session_with_viewport();
    // `S` (Shift) arms the extend variant.
    let _ = s.on_key(KeyCode::Char('s'), Mods::SHIFT, Some("S".into()));
    assert!(s.view.sneak.as_ref().unwrap().extend);
    let fx = s.on_key(KeyCode::Char('g'), Mods::SHIFT, Some("G".into()));
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
    let _ = s.on_key(KeyCode::Char('s'), Mods::ALT, Some("s".into()));
    assert!(s.view.sneak.as_ref().unwrap().big);
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
    let fx = s.on_key(KeyCode::Backspace, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "sneak/update");
    assert_eq!(params["query"], json!(""));
    assert!(s.view.sneak.is_some(), "still armed after backspace");

    // Esc cancels: a sneak/cancel and the session ends.
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "sneak/cancel");
    assert!(s.view.sneak.is_none(), "session ended on Esc");
}

#[test]
fn space_z_asks_the_shell_to_open_a_new_window() {
    let mut s = session();
    // `Space z` — was `Space Alt-x` until that chord became save-and-close.
    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('z'), Mods::NONE, Some("z".into()));
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::NewWindow(_)))),
        "Space z should emit ShellAction::NewWindow"
    );
    // It's a pure shell hand-off — no server traffic, and crucially not a view/close (that's
    // `Space x`).
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
        "opening a window issues no RPC"
    );
}

// ---- hints --------------------------------------------------------

/// A non-placeholder session (hints display nowhere on the boot placeholder).
fn hint_session() -> Session {
    Session::new(
        aether_protocol::workspace::WorkspaceInfo {
            worktrees: Vec::new(),
            name: "w".into(),
            paths: vec!["/tmp/w".into()],
            projects: Vec::new(),
        },
        aether_protocol::view::ViewOpenResult {
            view_id: aether_protocol::ViewId(1),
            scroll: None,
            transient: false,
            read: false,
            buffer: aether_protocol::view::BufferDescription {
                buffer_id: 1,
                language: None,
                line_count: 1,
                byte_count: 0,
                revision: 0,
                saved_revision: 0,
                path: Some("/tmp/w/a.rs".into()),
                scratch_number: None,
                cursor: aether_protocol::cursor::CursorState::default(),
                lsp_server: None,
                title: None,
                commit: None,
                read_only: false,
                is_patch: false,
            },
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

/// Drive a session through the connect sequence with hints on: `startup` → canned settings +
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
    let settings = json!({ "wrap": "soft", "ligatures": true, "editor_font_size": 14, "ui_font_size": 13, "hints": true });
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
    let settings = json!({ "wrap": "soft", "ligatures": true, "editor_font_size": 14, "ui_font_size": 13, "hints": true });
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
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
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
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast { title, .. } if title.contains("Hints disabled"))),
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
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
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
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast { title, .. } if title.contains("Hints disabled"))),
        "turning hints off is confirmed with a toast"
    );

    // And back on.
    key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('h'), Mods::ALT, None);
    assert!(s.hints_enabled);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "settings/set");
    assert_eq!(params["hints"], json!(true));
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::Toast { title, .. } if title.contains("Hints enabled"))),
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
            s.on_key(KeyCode::Char('h'), Mods::ALT, None)
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
            Effect::Toast { title, body: Some(body), kind: ToastKind::Warning, .. }
                if title.contains("Hints unavailable") && body.contains("Restart the Aether server")
        )),
        "a failed hints snapshot fetch must say so"
    );
    assert!(s.hint_view().is_none(), "the engine stays dormant");

    // With hints off the failure is irrelevant — stay quiet. (Drain the one-time decoration
    // subscribe the first step of any file-backed session emits, so the assertion below sees
    // only what this event produced.)
    let mut s = hint_session();
    s.hints_enabled = false;
    let _ = s.on_event(Event::Noop);
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
        unsaved: 0,
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
    // The boot chooser (every shell) is the core Workspaces picker over a placeholder session; its
    // hints run through the ordinary tick/view path — the picker context outranks the placeholder
    // check.
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
        unsaved: 0,
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

// ---- input history --------------------------------------------------

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
    assert_eq!(s.view.mode, Mode::Search);
    let _ = s.search_set_query("draft".into());

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.view.search.query, "newer", "Up recalls the newest entry");
    assert_eq!(
        find_request(&fx, "search/set").map(|p| p["query"].clone()),
        Some(json!("newer")),
        "each recall previews its matches"
    );
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.view.search.query, "older");
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(
        s.view.search.query, "older",
        "the oldest entry doesn't wrap"
    );

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(s.view.search.query, "newer");
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(
        s.view.search.query, "draft",
        "stepping past the newest restores the typed draft"
    );
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(s.view.search.query, "draft", "and stays there");

    // Alt-k/j remain as the unlisted alias.
    let _ = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    assert_eq!(s.view.search.query, "newer");
    let _ = s.on_key(KeyCode::Char('j'), Mods::ALT, None);
    assert_eq!(s.view.search.query, "draft");
}

/// Typing abandons a walk: the next `Up` starts again from the newest entry and stashes the *new*
/// draft, rather than continuing from where the previous walk left off.
#[test]
fn typing_abandons_a_history_walk() {
    let mut s = session();
    adopt_history(&mut s, json!({ "search": ["one", "two"] }));
    let _ = key(&mut s, '/');
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.view.search.query, "one");

    let _ = s.search_set_query("typed".into());
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(
        s.view.search.query, "two",
        "the walk restarts from the newest"
    );
    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(s.view.search.query, "typed", "and restores the newer draft");
}

/// Committing a search records it — once. The record is applied locally *and* sent to the server
/// (which persists it for other windows); a repeat of the newest entry sends nothing.
#[test]
fn committing_a_search_records_it_locally_and_server_side() {
    use aether_protocol::history::HistoryKind;
    let mut s = session();
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("needle".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "search", "value": "needle" }))
    );
    assert_eq!(hist(&s, HistoryKind::Search), ["needle"]);

    // Same query again: already the newest entry, so no list change and no traffic.
    let _ = key(&mut s, '/');
    let _ = s.search_set_query("needle".into());
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.picker.as_ref().unwrap().query, "wrap_state");
    assert_eq!(
        find_request(&fx, "picker/query").map(|p| p["query"].clone()),
        Some(json!("wrap_state")),
        "the recalled query re-runs the search"
    );
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.picker.as_ref().unwrap().query, "fn resolve");

    // Alt-k still moves the highlight rather than the history.
    let _ = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    assert_eq!(s.picker.as_ref().unwrap().query, "fn resolve");

    // Files has no query history: Up is inert there (and mustn't touch the query).
    let mut s = session();
    adopt_history(&mut s, json!({ "grep": ["fn resolve"] }));
    let _ = s.open_picker(PickerKind::Files, None, None, false, None);
    let _ = s.picker_set_query("mai".into());
    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
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
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert_eq!(
        find_request(&fx, "history/record"),
        Some(&json!({ "kind": "grep", "value": "wrap" })),
        "only the query the user settled on is recorded"
    );
    assert_eq!(hist(&s, HistoryKind::Grep), ["wrap"]);

    // A one-character query never ran a search, so it never enters the history.
    let _ = s.open_picker(PickerKind::Grep, None, None, false, None);
    let _ = s.picker_set_query("w".into());
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char('g'), Mods::ALT, None);
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    let ed = s.picker.as_ref().unwrap().chip_editor.as_ref().unwrap();
    assert_eq!(ed.input.text, "*.rs");
    let _ = s.on_key(KeyCode::Up, Mods::NONE, None);
    let ed = s.picker.as_ref().unwrap().chip_editor.as_ref().unwrap();
    assert_eq!(ed.input.text, "*.toml");

    // Enter commits: the chip lands and the field text is recorded.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char('c'), Mods::ALT, None); // smart -> sensitive
    assert_eq!(s.view.search.options.case, CaseMode::Sensitive);

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(s.view.search.query, "f.o");
    assert_eq!(
        s.view.search.options,
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

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(s.view.search.query, "plain");
    assert_eq!(
        s.view.search.options.case,
        CaseMode::Sensitive,
        "Down restores the options that were in effect before the walk"
    );
    assert!(!s.view.search.options.regex);
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

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
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

    let _ = s.on_key(KeyCode::Down, Mods::NONE, None);
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
    let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None); // Alt-e: regex on
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
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
            let _ = s.on_key(KeyCode::Char('e'), Mods::ALT, None);
        }
        let _ = s.on_key(KeyCode::Esc, Mods::NONE, None);
    }
    assert_eq!(hist(&s, HistoryKind::Grep), ["wrap"], "one row, not two");
    assert!(
        s.history.list(HistoryKind::Grep)[0].filters.regex,
        "the newest configuration wins"
    );
}

// ---- markdown reading view ----------------------------------------------

fn md_session() -> Session {
    let mut s = session();
    s.view.buffer.language = Some("markdown".into());
    s
}

fn leader(s: &mut Session, c: char) -> Effects {
    let _ = key(s, ' ');
    key(s, c)
}

/// `Space g …` — the git sub-leader (`KeyContext::LeaderGit`). Two prefix keystrokes, then the
/// operation; the intermediate `g` produces no effects of its own.
fn git_leader(s: &mut Session, c: char) -> Effects {
    let _ = key(s, ' ');
    let _ = key(s, 'g');
    key(s, c)
}

/// The window the server answers a subscribe with when it presents `buffer_id` as the reader:
/// one prose element carrying the parse of `text` and the line table that places it.
fn reader_subscribe(
    buffer_id: u64,
    text: &str,
) -> aether_protocol::viewport::ViewportSubscribeResult {
    use aether_protocol::viewport::{Element, Window};
    aether_protocol::viewport::ViewportSubscribeResult {
        viewport_id: 7,
        buffer_status: Default::default(),
        focus: focus_on(0, buffer_id, 0),
        window: Window {
            other_elements_dirty: false,
            max_line_width: 0,
            git_status: None,
            root: Element::Prose {
                element: 0,
                blocks: aether_client::markdown::parse(text),
                source: aether_protocol::ui::SourceLines::of(text),
            },
        },
    }
}

/// The window for the same file presented as the **editor**: one editor element, no lines
/// loaded. What `Space u` toggles to, and what the reader is recognised as *not* being.
fn editor_subscribe(buffer_id: u64) -> aether_protocol::viewport::ViewportSubscribeResult {
    use aether_protocol::viewport::{Element, Window};
    aether_protocol::viewport::ViewportSubscribeResult {
        viewport_id: 7,
        buffer_status: Default::default(),
        focus: focus_on(0, buffer_id, 0),
        window: Window {
            other_elements_dirty: false,
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                collapsed: false,
                element: 0,
                buffer: buffer_id,
                rows: 1,
                first_row: aether_protocol::coords::ElementRow::ZERO,
                laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                role: aether_protocol::ui::ElementRole::Field,
                first_buffer_line: 0,
                lines: vec![],
            },
        },
    }
}

/// The reader's window lands: adopt it as the shell would, returning the adoption's effects (the
/// fence highlight requests, a followed anchor's cursor move).
fn adopt_reader_window(s: &mut Session, text: &str) -> Effects {
    let id = s.view.buffer.buffer_id;
    s.adopt_subscribe(reader_subscribe(id, text))
}

/// The server's answer to a `view/set_read`: the mode it now has this client in.
fn read_set(read: bool) -> serde_json::Value {
    json!({ "read": read })
}

/// The `view/set_read` a `Space u` (or an edit transition) sends: naming the session's own view,
/// asking to read (`true`) or to edit. Returns its token.
fn the_read_request(s: &Session, fx: &Effects, read: bool) -> u64 {
    let (token, _, params) = all_requests(fx)
        .into_iter()
        .zip(fx.0.iter().filter_map(|e| match e {
            Effect::Request { token, .. } => Some(*token),
            _ => None,
        }))
        .map(|((m, p), t)| (t, m, p))
        .find(|(_, m, _)| *m == "view/set_read")
        .expect("a view/set_read for the session's view");
    assert_eq!(params["view_id"], json!(s.view.view_id.get()));
    assert!(
        params.get("buffer_id").is_none(),
        "the wire names views, never buffers"
    );
    assert_eq!(params["read"], json!(read));
    token
}

/// `Space u` on the session's markdown buffer — which asks the server to flip this client's mode
/// to reading and re-subscribes once it has — and the window the subscribe answers with, over
/// `text`. Returns the window adoption's effects.
fn enter_reader(s: &mut Session, text: &str) -> Effects {
    let view = s.view.view_id;
    let fx = leader(s, 'u');
    let token = the_read_request(s, &fx, true);
    let fx = s.on_rpc_result(token, Ok(read_set(true)));
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "the mode flipped: re-subscribe"
    );
    assert_eq!(s.view.view_id, view, "the same view");
    adopt_reader_window(s, text)
}

/// A canned reading-view setup: `Space u` on a markdown buffer, the reader's window adopted.
/// Layout: heading (line 0), paragraph (line 2), paragraph with a link (line 4).
fn read_session() -> Session {
    let mut s = md_session();
    let _ = enter_reader(
        &mut s,
        "# Title\n\nFirst para.\n\nSee [docs](https://x.y) here.\n",
    );
    s
}

/// A reading view over content the parser yields *no blocks* for — an empty file, or one
/// holding nothing but blank lines.
fn blockless_read_session(text: &str) -> Session {
    let mut s = md_session();
    let _ = enter_reader(&mut s, text);
    assert!(s
        .view
        .read
        .as_ref()
        .expect("reading view")
        .blocks
        .is_empty());
    s
}

#[test]
fn space_u_asks_to_read_and_the_window_delivers_it() {
    use aether_client::session::Mode;
    let mut s = md_session();
    let view = s.view.view_id;
    let fx = leader(&mut s, 'u');
    // The ask: a content anchor so the same lines stay on screen across the re-presentation,
    // then the flip of this client's mode. Nothing else is fetched — the re-subscribe's window
    // carries the document.
    assert!(fx.0.iter().any(|e| matches!(e, Effect::SaveContentAnchor)));
    let token = the_read_request(&s, &fx, true);
    assert_eq!(all_requests(&fx).len(), 1);
    assert_eq!(s.view.mode, Mode::Normal, "until the window says otherwise");
    assert!(s.view.read.is_none());
    let fx = s.on_rpc_result(token, Ok(read_set(true)));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));
    assert_eq!(
        s.view.view_id, view,
        "the same view: reading is a mode, not a view"
    );
    assert_eq!(
        s.view.buffer.buffer_id, s.view.view_buffer,
        "the same buffer"
    );

    let _ = adopt_reader_window(
        &mut s,
        "# Title\n\nFirst para.\n\nSee [docs](https://x.y) here.\n",
    );
    assert_eq!(s.view.mode, Mode::Read);
    let read = s.view.read.as_ref().expect("reading view active");
    assert!(!read.loading);
    // heading + 2 paragraphs + the link, in document order.
    assert_eq!(read.elements.len(), 4);
}

/// `Space u` carries **where you are**: the content anchor the shell captures for the flip is
/// what the re-subscribe frames, and nothing the server answers with moves it — the view, its
/// scroll memory and its cursor are all the same view's.
#[test]
fn a_mode_flip_keeps_your_place() {
    use aether_protocol::coords::VisualRow;
    let mut s = md_session();
    let _ = adopt_reader_window(&mut s, "# Title\n\nFirst para.\n\nSecond para.\n");

    // The flip: the shell captures the anchor for where the reader is, then asks for source.
    let fx = leader(&mut s, 'u');
    assert!(fx.0.iter().any(|e| matches!(e, Effect::SaveContentAnchor)));
    s.capture_scroll_anchor(VisualRow(0), 20, &Default::default());
    let anchored = s
        .relayout_anchor_position()
        .expect("an anchor for the flip");
    let token = the_read_request(&s, &fx, false);
    let fx = s.on_rpc_result(token, Ok(read_set(false)));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));
    assert_eq!(
        s.relayout_anchor_position(),
        Some(anchored),
        "the place the flip was carrying is what the re-subscribe frames"
    );
}

#[test]
fn space_u_on_non_markdown_toasts_and_stays_normal() {
    use aether_client::session::Mode;
    let mut s = session(); // language: None
    let fx = leader(&mut s, 'u');
    assert_eq!(s.view.mode, Mode::Normal);
    assert!(s.view.read.is_none());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
}

#[test]
fn read_j_steps_focus_via_goto_to_next_block() {
    let mut s = read_session();
    // Cursor at 0,0 → focus is the heading; `j` lands on the first paragraph's start (line 2).
    let fx = key(&mut s, 'j');
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["kind"], json!("goto"));
    assert_eq!(params["motion"]["position"], json!({"line": 2, "col": 0}));
    assert_eq!(params["extend_selection"], json!(false));
}

#[test]
fn read_p_and_alt_jk_alias_the_element_step() {
    let mut s = read_session();
    // `p` steps like `j` — the editor's first-non-blank line step collapses into the element
    // step at block grain…
    let fx = key(&mut s, 'p');
    let (t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["position"], json!({"line": 2, "col": 0}));
    let _ = s.on_rpc_result(
        t,
        Ok(json!({
            "position": {"line": 2, "col": 0},
            "anchor": {"line": 2, "col": 0},
        })),
    );
    // …and `Alt-k` steps back like `k` (the visual-row variant, same collapse).
    let fx = s.on_key(KeyCode::Char('k'), Mods::ALT, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["position"], json!({"line": 0, "col": 0}));
}

#[test]
fn read_percent_selects_all_blocks() {
    let mut s = read_session();
    let fx = s.on_key(
        KeyCode::Char('%'),
        Mods {
            shift: true,
            ..Mods::NONE
        },
        Some("%".into()),
    );
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/select_all");
}

#[test]
fn read_comma_collapses_the_block_selection() {
    let mut s = read_session();
    // A point cursor has nothing to collapse: `,` is swallowed.
    assert!(no_request(&key(&mut s, ',')));
    // Build a real block selection: `x` selects the focused heading whole-line…
    let fx = key(&mut s, 'x');
    let (t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/select_block");
    let _ = s.on_rpc_result(
        t,
        Ok(json!({
            "position": {"line": 0, "col": 7},
            "anchor": {"line": 0, "col": 0},
        })),
    );
    // …then `,` collapses to the cursor end without moving: a point `cursor/set` at the
    // default Char grain (skipped on the wire).
    let fx = key(&mut s, ',');
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/set");
    assert_eq!(params["position"], json!({"line": 0, "col": 7}));
    assert_eq!(params["position"], params["anchor"]);
    assert_eq!(params.get("granularity"), None);
}

#[test]
fn read_delete_key_aliases_ctrl_d() {
    let mut s = read_session();
    let fx = s.on_key(KeyCode::Delete, Mods::NONE, None);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/delete_block");
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
        assert_eq!(m, "element/move");
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
    assert_eq!(method, "element/move");
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
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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

/// `v`/`Alt-v`: the editor's half-page cursor motion, verbatim — the server resolves it in editor
/// wrap geometry and the returned cursor derives focus (best-effort distance, framed landing).
///
/// The count is *pages*, not rows: the span is the viewport's own height, read server-side. This
/// used to send `visual_line` with a count of half the viewport's rows, which is how a number
/// no one typed reached the field the count rule reads as an assertion — and why `100 Alt-j`
/// clamped.
#[test]
fn read_v_rides_the_editor_half_page_motion() {
    let mut s = read_session();
    // The viewport subscription stays alive in Read; the motion needs its id for the
    // editor wrap geometry.
    s.view.viewport_id = Some(7);
    let fx = key(&mut s, 'v');
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["kind"], json!("page"));
    assert_eq!(params["motion"]["direction"], json!("down"));
    assert_eq!(params["motion"]["count"], json!(1));
    assert_eq!(params["motion"]["half"], json!(true));
}

/// `z`/`Alt-z`: the server's cursor-motion history, verbatim — the returned cursor derives
/// focus, so this is "step back/forward through reading positions".
#[test]
fn read_z_walks_the_reading_position_history() {
    let mut s = read_session();
    let fx = key(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/cursor_undo");
    let fx = s.on_key(KeyCode::Char('z'), Mods::ALT, None);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/cursor_redo");
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
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 0}));
    let _ = s.on_rpc_result(
        t,
        Ok(json!({"position": {"line": 4, "col": 0}, "anchor": {"line": 4, "col": 0}})),
    );
    {
        let read = s.view.read.as_ref().unwrap();
        let cursor = s.view.buffer.cursor.position;
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
fn space_n_shows_the_focused_target_without_following() {
    use aether_client::session::HoverText;
    let mut s = read_session();
    // On a plain block: quiet no-op.
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let fx = s.on_key(KeyCode::Char('n'), Mods::NONE, None);
    assert!(
        fx.0.is_empty(),
        "Space n on a non-interactive block does nothing"
    );
    // On a focused link: the URL in the hover popover (whose own keys then apply — Ctrl-c
    // copies it via `keymap::hover_action`), no open, no cursor move.
    focus_the_link(&mut s);
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let fx = s.on_key(KeyCode::Char('n'), Mods::NONE, None);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShowHover(HoverText::Blocks(b))
                if b.len() == 1 && b[0].text == "https://x.y" && b[0].severity.is_none()
        )),
        "Space t reveals the link target in the popover"
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
    // Cursor on the heading: `Ctrl-c` asks the *server* for the element's source. The reading
    // view holds a parse, and a parse is not the text it was made from.
    let fx = ctrl(&mut s, 'c');
    let (t, method, params) = the_request(&fx);
    assert_eq!(method, "element/source");
    assert_eq!(params["buffer_id"], json!(s.view.buffer.buffer_id));
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::WriteClipboard(_))),
        "nothing reaches the clipboard until the answer does"
    );
    let fx = s.on_rpc_result(t, Ok(json!({"text": "# Title"})));
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
    let fx = s.on_key(KeyCode::Char('j'), Mods::SHIFT, Some("J".into()));
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "element/set");
    assert_eq!(p["granularity"], json!("line"));
    assert_eq!(p["anchor"]["line"], json!(0));
    assert_eq!(p["position"]["line"], json!(2));
}

/// `x` / `Shift-x` / `Alt-x` ask the server to step the block selection.
///
/// The stepping rule itself lives server-side now — it reads the selection it already has, and
/// telling a partial range from a whole one needs the block boundaries under the current bytes,
/// which needs the document's text. What is left here is which way, and whether to extend.
#[test]
fn read_x_asks_the_server_to_step_the_block_selection() {
    let mut s = read_session();
    let fx = key(&mut s, 'x');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "element/select_block");
    assert_eq!(p["direction"], json!("down"));
    assert_eq!(p["extend"], json!(false));

    // Shift extends rather than walking.
    let fx = s.on_key(KeyCode::Char('x'), Mods::SHIFT, Some("X".into()));
    let (_t, _m, p) = the_request(&fx);
    assert_eq!(p["direction"], json!("down"));
    assert_eq!(p["extend"], json!(true));

    // Alt goes the other way.
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let (_t, _m, p) = the_request(&fx);
    assert_eq!(p["direction"], json!("up"));
    assert_eq!(p["extend"], json!(false));
}

#[test]
fn read_ctrl_c_copies_the_extended_selections_source() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // A whole-line selection over heading + first paragraph, as the server would hold it:
    // copy takes the source slice, inclusive of the end cursor's newline.
    s.view.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.view.buffer.cursor.position = LogicalPosition { line: 2, col: 11 };
    let fx = ctrl(&mut s, 'c');
    let (t, method, _p) = the_request(&fx);
    assert_eq!(
        method, "element/source",
        "the selection's source is the server's too"
    );
    let fx = s.on_rpc_result(t, Ok(json!({"text": "# Title\n\nFirst para.\n"})));
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
    let read = s.view.read.as_ref().unwrap();
    assert!(read.display_block_focus(&s.view.buffer.cursor).is_some());
    let cursor = CursorState {
        position: LogicalPosition { line: 4, col: 0 },
        anchor: LogicalPosition { line: 4, col: 0 },
        ..Default::default()
    };
    let fx = s.on_event(Event::BlockEditDone(Ok(BlockEditResult {
        buffer: 0,
        applied: true,
        reason: None,
        revision: 2,
        cursor,
        text: None,
    })));
    assert!(
        !all_requests(&fx)
            .iter()
            .any(|(m, _)| *m == "buffer/content"),
        "nothing is fetched — the pushed window re-parses"
    );
    let read = s.view.read.as_ref().unwrap();
    assert!(
        read.loading,
        "…and meanwhile the view knows its parse is stale"
    );
    assert_eq!(read.display_block_focus(&s.view.buffer.cursor), None);
    assert_eq!(read.display_target(&s.view.buffer.cursor), None);
    assert_eq!(read.display_selection(&s.view.buffer.cursor), None);
    // The pushed window's parse restores them.
    let _ = adopt_reader_window(&mut s, "# Title\n\nMoved.\n\nFirst para.\n");
    let read = s.view.read.as_ref().unwrap();
    assert!(!read.loading);
    assert_eq!(read.revision, 2);
    assert!(read.display_block_focus(&s.view.buffer.cursor).is_some());
}

#[test]
fn read_extended_selection_suppresses_the_display_target() {
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // Cursor inside the link span: the pill shows while the selection is a point…
    s.view.buffer.cursor.position = LogicalPosition { line: 4, col: 5 };
    s.view.buffer.cursor.anchor = s.view.buffer.cursor.position;
    let read = s.view.read.as_ref().unwrap();
    assert!(read.target_focus(s.view.buffer.cursor.position).is_some());
    assert!(read.display_target(&s.view.buffer.cursor).is_some());
    // …and goes away as soon as the selection is extended (one selection at a time).
    s.view.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    let read = s.view.read.as_ref().unwrap();
    assert!(read.target_focus(s.view.buffer.cursor.position).is_some());
    assert!(read.display_target(&s.view.buffer.cursor).is_none());
}

/// `i` and `a` leave for the editor and ask for the block's edge.
///
/// Which edge is the only thing said here. Finding the append point walks back over the block's
/// trailing blank lines, which needs the block's text, so the landing is the server's.
#[test]
fn read_i_and_a_ask_for_the_blocks_edges() {
    use aether_client::session::Mode;
    let mut s = read_session();
    let fx = key(&mut s, 'i');
    assert_eq!(s.view.mode, Mode::Insert);
    assert!(s.view.read.is_none());
    let (_t, method, p) = the_request_beside_open(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(p["motion"]["kind"], json!("block_edge"));
    assert_eq!(p["motion"]["at_end"], json!(false));

    let mut s = read_session();
    let fx = key(&mut s, 'a');
    assert_eq!(s.view.mode, Mode::Insert);
    let (_t, _m, p) = the_request_beside_open(&fx);
    assert_eq!(p["motion"]["at_end"], json!(true));

    // A document with no blocks is not a dead end: the same ask goes out, and the server lands on
    // the element's own edges.
    let mut s = blockless_read_session("");
    let fx = key(&mut s, 'i');
    assert_eq!(s.view.mode, Mode::Insert);
    let (_t, method, _p) = the_request_beside_open(&fx);
    assert_eq!(method, "element/move");
}

#[test]
fn read_placeholder_names_the_loading_and_empty_states() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::input::UndoResult;
    // The line every shell paints when there's nothing to lay out — spelled once, in the core.
    // A parsed document with no blocks in it is empty…
    let mut s = blockless_read_session("");
    assert_eq!(
        s.view.read.as_ref().unwrap().placeholder(),
        Some("Empty document")
    );
    // …and the same view is "Loading…" once an edit of ours moves the text under it. That is the
    // whole of loading now: prose has no wire rows, so a document arrives entire or not at all,
    // and the only gap left is the round trip to the window that re-parses it.
    s.view.read.as_mut().unwrap().revision = 1;
    let _ = s.on_event(Event::UndoRedoDone(Ok(UndoResult {
        buffer: 0,
        revision: 0,
        applied: true,
        cursor: CursorState::default(),
    })));
    assert_eq!(
        s.view.read.as_ref().unwrap().placeholder(),
        Some("Loading…")
    );
    // A document with blocks shows itself.
    assert_eq!(
        read_session().view.read.as_ref().unwrap().placeholder(),
        None
    );
}

#[test]
fn read_i_extended_uses_the_editors_selection_edge() {
    use aether_client::session::Mode;
    use aether_protocol::LogicalPosition;
    let mut s = read_session();
    // An extended whole-line selection: `i` hands the landing to the server's own
    // Insert-entry motion instead of a client-computed Goto.
    s.view.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.view.buffer.cursor.position = LogicalPosition { line: 2, col: 11 };
    let fx = key(&mut s, 'i');
    assert_eq!(s.view.mode, Mode::Insert);
    let (_t, method, p) = the_request_beside_open(&fx);
    assert_eq!(method, "element/move");
    assert_eq!(p["motion"]["kind"], json!("selection_edge"));
}

/// `Ctrl-e` asks for the block's content range, then changes it.
///
/// The range's end is the block's last *content* char, so the terminating newline and both
/// separators survive the change — which is why the range is the server's to work out, and why the
/// two requests have to arrive in this order.
#[test]
fn read_ctrl_e_asks_for_the_content_range_then_changes_it() {
    use aether_client::session::Mode;
    let mut s = read_session();
    let fx = ctrl(&mut s, 'e');
    assert_eq!(s.view.mode, Mode::Insert);
    assert!(s.view.read.is_none());
    let reqs: Vec<_> = all_requests(&fx)
        .into_iter()
        .filter(|(m, _)| *m != "view/set_read") // the source asked for, first
        .collect();
    assert_eq!(reqs.len(), 2, "content range then change: {reqs:?}");
    assert_eq!(reqs[0].0, "element/block_content");
    assert_eq!(reqs[1].0, "element/change");
}

#[test]
fn read_ctrl_o_opens_a_block_via_the_server_then_enters_insert() {
    use aether_client::session::Mode;
    // One RPC — what gets opened (sibling item / paragraph) is the server's parse to decide,
    // not ours — and the mode waits for it: the caret comes back parked in the new block.
    let mut s = read_session();
    let fx = ctrl(&mut s, 'o');
    let (token, method, p) = the_request(&fx);
    assert_eq!(method, "element/open_block");
    assert_eq!(p["above"], json!(false));
    assert_eq!(
        s.view.mode,
        Mode::Read,
        "still reading until the edit lands"
    );
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "applied": true,
            "revision": 2,
            "cursor": { "position": {"line": 4, "col": 2}, "anchor": {"line": 4, "col": 2} },
        })),
    );
    assert_eq!(s.view.mode, Mode::Insert);
    assert!(s.view.read.is_none(), "handed over to the editor");
    assert_eq!(
        s.view.buffer.cursor.position.col, 2,
        "parked past the marker"
    );
    assert!(
        !all_requests(&fx).iter().any(|(m, _)| *m == "element/move"),
        "the landing needs no correcting move"
    );
    assert!(
        all_requests(&fx).iter().any(|(m, _)| *m == "view/set_read"),
        "…only the editor's view, asked for"
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
            "buffer": 0,
            "applied": false,
            "reason": "Front matter stays at the top",
            "revision": 1,
            "cursor": { "position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0} },
        })),
    );
    assert_eq!(s.view.mode, Mode::Read);
    assert!(s.view.read.is_some());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
}

/// The edit transitions leave the reading view at once *and* ask to see the source — the next
/// pushed window would otherwise bring the reader straight back mid-Insert. The mode they set
/// survives the flip: the view and the cursor are the same.
#[test]
fn read_edit_transitions_ask_for_the_source() {
    use aether_client::session::Mode;
    let mut s = read_session();
    let view = s.view.view_id;
    let fx = key(&mut s, 'i');
    assert_eq!(s.view.mode, Mode::Insert);
    assert!(s.view.read.is_none(), "handed over to the editor at once");
    let token = the_read_request(&s, &fx, false);
    let fx = s.on_rpc_result(token, Ok(read_set(false)));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));
    assert_eq!(s.view.view_id, view);
    assert_eq!(s.view.mode, Mode::Insert, "the transition's mode stands");
    // Space u out of Read asks the same way.
    let mut s = read_session();
    let fx = leader(&mut s, 'u');
    assert_eq!(s.view.mode, Mode::Normal);
    let _ = the_read_request(&s, &fx, false);
}

/// How a file opens is the server's: a plain open asks for nothing, and the window that comes
/// back says whether it is being read.
#[test]
fn a_plain_open_leaves_the_mode_to_the_server() {
    use aether_client::session::Mode;
    let mut s = read_session();
    let fx = s.open_path_at("/tmp/other.md".into(), None, None);
    let (token, _m, params) = the_request(&fx);
    assert!(
        params.get("read").is_none(),
        "no opinion: the server presents the file as it was last shown"
    );
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer_id": 9, "view_id": 19, "language": "markdown", "line_count": 5,
            "byte_count": 40, "revision": 0, "saved_revision": 0, "path": "/tmp/other.md",
        })),
    );
    assert_eq!(s.view.view_id, ViewId(19), "the view the open presented");
    assert_eq!(s.view.mode, Mode::Normal, "until the window arrives");
    assert!(s.view.read.is_none());
    // …and a reader window puts the session in the reading view.
    let _ = adopt_reader_window(&mut s, "# Other\n");
    assert_eq!(s.view.mode, Mode::Read);
}

#[test]
fn read_ctrl_z_undoes_from_the_reading_view() {
    let mut s = read_session();
    let fx = ctrl(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/undo");
    let fx = ctrl_alt(&mut s, 'z');
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/redo");
}

#[test]
fn read_ctrl_j_k_move_blocks_and_the_editor_moves_paragraphs() {
    // Read: block grain, with the Ctrl-Alt aliases.
    let mut s = read_session();
    let fx = ctrl(&mut s, 'j');
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "element/move_block");
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
    assert_eq!(method, "element/move_block");
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
    assert_eq!(method, "element/delete_block");
    let fx = s.on_event(Event::BlockEditDone(Ok(BlockEditResult {
        buffer: 0,
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
        s.view.read.as_ref().unwrap().loading,
        "the parse is stale until the pushed window re-parses it"
    );
}

#[test]
fn read_r_reverses_the_selection_and_alt_r_orients_it_forward() {
    // The editor's own pair, reused verbatim: focus derives from the cursor, so swapping the
    // ends moves the bar to the other edge — and Shift-j/k then grow from there, because
    // `read_step` extends from the cursor's block and keeps the anchor.
    let mut s = read_session();
    let (_t, method, _p) = the_request(&key(&mut s, 'x'));
    assert_eq!(
        method, "element/select_block",
        "a block selection to reverse"
    );
    let (_t, method, p) = the_request(&key(&mut s, 'r'));
    assert_eq!(method, "element/swap_anchor");
    // `forward_only: false` is the wire default and skips (the plain toggle).
    assert!(p.get("forward_only").is_none(), "{p}");
    let fx = s.on_key(KeyCode::Char('r'), Mods::ALT, None);
    let (_t, method, p) = the_request(&fx);
    assert_eq!(method, "element/swap_anchor");
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
    assert_eq!(method, "element/delete_block");
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
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
        s.view.read.as_ref().unwrap().loading,
        "the parse is stale until the pushed window re-parses it"
    );
    assert_eq!(s.view.buffer.revision, 2);
    assert_eq!(s.view.mode, Mode::Read, "a deletion is not a transition");
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
    assert_eq!(method, "element/paste_block");
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
    assert_eq!(method, "element/block_depth");
    assert_eq!(p["deeper"], json!(true));
    // A reasoned refusal toasts…
    let refusal = |reason: Option<&str>| {
        Event::BlockEditDone(Ok(BlockEditResult {
            buffer: 0,
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
        .any(|e| matches!(e, Effect::Toast { title, .. } if title.contains("Depth"))),);
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
    let _ = enter_reader(&mut s, "- [ ] open\n- [x] done\n");
    s.view.buffer.cursor.position = LogicalPosition { line: 0, col: 6 };
    s.view.buffer.cursor.anchor = s.view.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/toggle_task");
    let _ = Event::BlockEditDone; // (adoption covered by the cut test)
}

#[test]
fn j_steps_one_block_from_a_selected_fence() {
    use aether_protocol::LogicalPosition;
    // `x` on a fence leaves the cursor on the closing fence line's newline — a byte a Code span
    // does not reach, unlike a paragraph's. Resolving the step origin there fell forward to the
    // block *after* the fence, so `j` landed two blocks down and skipped one.
    let mut s = md_session();
    let text = "Intro.\n\n```rust\nfn a() {}\n```\n\nMiddle.\n\nLast.\n";
    let _ = enter_reader(&mut s, text);
    // Cursor on the closing fence line's newline, as a whole-line block selection leaves it.
    s.view.buffer.cursor.anchor = LogicalPosition { line: 2, col: 0 };
    s.view.buffer.cursor.position = LogicalPosition { line: 4, col: 3 };
    let fx = s.on_key(KeyCode::Char('j'), Mods::NONE, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/move");
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
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/toggle_task");
    assert_eq!(params["set"], json!(true), "up checks the box");
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL_ALT, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/toggle_task");
    assert_eq!(params["set"], json!(false), "down unchecks it");
    // The same chord in the reading view resolves the same way — that is the point of it.
    let _ = enter_reader(&mut s, "- [ ] open\n");
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/toggle_task");
    assert_eq!(params["set"], json!(true));
    // A non-markdown buffer keeps the number adjust.
    let mut s = session();
    let fx = s.on_key(KeyCode::Char('a'), Mods::CTRL, None);
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "element/adjust_number");
    assert_eq!(params["delta"], json!(1));
}

#[test]
fn enter_toggles_a_task_item_holding_more_than_one_block() {
    use aether_protocol::LogicalPosition;
    // An item with a sub-list (or a second paragraph) lists its inner block as an element of its
    // own, so innermost-first resolution stops there and never sees the checkbox. The item around
    // it owns the box — the same outward walk the server's `resolve_toggle_task` does.
    let mut s = md_session();
    let _ = enter_reader(&mut s, "- [ ] outer\n\n  - [x] inner\n");
    // On the outer item's own text, whose innermost element is the paragraph, not the item.
    s.view.buffer.cursor.position = LogicalPosition { line: 0, col: 8 };
    s.view.buffer.cursor.anchor = s.view.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_t, method, _p) = the_request(&fx);
    assert_eq!(method, "element/toggle_task");
}

#[test]
fn enter_does_not_follow_a_link_the_selection_has_un_armed() {
    use aether_protocol::LogicalPosition;
    // With the selection extended the shells hide the target pill, so nothing on screen says a
    // link is armed. Enter must not follow one it isn't showing.
    let mut s = md_session();
    let _ = enter_reader(&mut s, "[docs](https://example.com) and text.\n");
    // Point cursor on the link: Enter follows it.
    s.view.buffer.cursor.position = LogicalPosition { line: 0, col: 2 };
    s.view.buffer.cursor.anchor = s.view.buffer.cursor.position;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        fx.0.iter()
            .any(|e| matches!(e, Effect::ShellAction(ShellAction::OpenUrl(_)))),
        "an armed link still follows"
    );
    // Same cursor, selection extended over the block: no navigation, no request.
    s.view.buffer.cursor.anchor = LogicalPosition { line: 0, col: 0 };
    s.view.buffer.cursor.position = LogicalPosition { line: 0, col: 30 };
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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
    let fx = leader(&mut s, 'u');
    assert_eq!(s.view.mode, Mode::Normal);
    assert!(s.view.read.is_none());
    // The reading position is framed first, then the anchor captured with it on screen, then
    // the editor asked for — so it opens showing where you were reading.
    let order: Vec<&str> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::RevealCursor(aether_client::effect::RevealStyle::Jump) => Some("reveal"),
                Effect::SaveContentAnchor => Some("anchor"),
                Effect::Request { method, .. } if *method == "view/set_read" => Some("flip"),
                _ => None,
            })
            .collect();
    assert_eq!(order, vec!["reveal", "anchor", "flip"]);
    let token = the_read_request(&s, &fx, false);
    let fx = s.on_rpc_result(token, Ok(read_set(false)));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));
    // The editor's window confirms it.
    let id = s.view.buffer.buffer_id;
    let _ = s.adopt_subscribe(editor_subscribe(id));
    assert_eq!(s.view.mode, Mode::Normal);
    assert!(s.view.read.is_none());
    // `Space u` again asks to read, whatever the app default says.
    s.markdown_read_default = false;
    let fx = leader(&mut s, 'u');
    let _ = the_read_request(&s, &fx, true);
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
                    | Action::NavUnit(_)
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
                    // Whole-buffer select and collapse: cursor-only, like the swap.
                    | Action::SelectAll
                    | Action::CollapseSelection
                    // The curated edits: undo/redo act on the buffer but create no new
                    // text shape from Read; each future edit action is added here
                    // deliberately, keeping the discipline as an explicit list.
                    | Action::Undo
                    | Action::Redo
                    // Phase 2: the to-the-editor transitions — they place the cursor
                    // and hand over to the editor's own insert/change machinery.
                    | Action::ReadInsert { .. }
                    | Action::ReadChange
                    | Action::ReadOpenBlock { .. }
                    // Phase 3: the structural edits — selection-relative server ops,
                    // atomic, refusals as applied:false.
                    | Action::MoveBlock { .. }
                    | Action::ReadCutBlock
                    | Action::ReadDeleteBlock
                    | Action::ReadPasteBlock { .. }
                    | Action::ReadBlockDepth { .. }
                    // The editor's adjust-the-value pair, re-declared in the Read table so
                    // `Ctrl-a`/`Ctrl-Alt-a` check and uncheck a task item on both sides of
                    // `Space u`. In markdown they never touch a number.
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

/// A reader's element is loaded whole, so every edit to it — another client's included — arrives
/// as a pushed window, and the push is the re-parse. The revision-only `buffer/changed` signal is
/// for consumers with lines outside the window, which a reader has none of: it is quiet here.
#[test]
fn a_pushed_window_reparses_the_reader() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification};
    let mut s = read_session();
    let id = s.view.buffer.buffer_id;
    let gen = s.view.read.as_ref().unwrap().hl_gen;
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "buffer/changed".into(),
        params: json!({"buffer_id": id, "revision": 2}),
    }));
    assert!(all_requests(&fx).is_empty(), "nothing to fetch");
    assert_eq!(
        s.view.read.as_ref().unwrap().hl_gen,
        gen,
        "…and nothing to re-parse yet"
    );

    let window = reader_subscribe(id, "# Title\n\nChanged.\n").window;
    let fx = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: "view/lines_changed".into(),
        params: json!({
            "viewport_id": 7,
            "buffer": id,
            "revision": 2,
            "window": serde_json::to_value(&window).unwrap(),
        }),
    }));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::WindowAdopted)));
    let read = s.view.read.as_ref().unwrap();
    assert_eq!(
        read.blocks,
        aether_client::markdown::parse("# Title\n\nChanged.\n"),
        "re-adopted from the push"
    );
    assert_eq!(read.revision, 2);
    assert_eq!(read.hl_gen, gen + 1);
    assert!(!read.loading);
}

#[test]
fn read_undo_marks_the_parse_stale_despite_the_restored_older_revision() {
    use aether_client::update::Event;
    use aether_protocol::cursor::CursorState;
    use aether_protocol::input::UndoResult;
    // Undo restores the undone entry's revision NUMBER — revisions identify states, they
    // don't order them — so the response can carry an *older* revision than the parse. The
    // parse is stale all the same, until the pushed window re-parses it.
    let mut s = read_session();
    s.view.read.as_mut().unwrap().revision = 1;
    let fx = s.on_event(Event::UndoRedoDone(Ok(UndoResult {
        buffer: 0,
        revision: 0,
        applied: true,
        cursor: CursorState::default(),
    })));
    assert!(all_requests(&fx).is_empty(), "nothing fetched");
    assert!(
        s.view.read.as_ref().unwrap().loading,
        "older revision → still stale"
    );
    // A response for the revision the parse already has (the push got here first) is not.
    let mut s = read_session();
    let at = s.view.read.as_ref().unwrap().revision;
    let _ = s.on_event(Event::UndoRedoDone(Ok(UndoResult {
        buffer: 0,
        revision: at,
        applied: true,
        cursor: CursorState::default(),
    })));
    assert!(!s.view.read.as_ref().unwrap().loading);
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
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/open");
    assert!(
        params.get("jump_to").is_some() && params.get("read").is_none(),
        "the jump itself says source; nothing else needs to"
    );
    let open = json!({
        "buffer_id": 7, "language": "markdown", "line_count": 5, "byte_count": 40,
        "revision": 0, "saved_revision": 0, "path": "/tmp/doc.md",
    });
    let fx = s.on_rpc_result(token, Ok(open.clone()));
    assert_eq!(s.view.mode, Mode::Normal, "jump-shaped → editor");
    assert!(s.view.read.is_none());
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));

    // File-shaped (files picker / a doc link): no opinion — the server presents the file as it
    // last was, or as the setting says, and the window decides the view.
    let fx = s.open_path_at("/tmp/other.md".into(), None, None);
    let (token, _m, params) = the_request(&fx);
    assert!(
        params.get("read").is_none(),
        "file-shaped → the server's call"
    );
    assert!(params.get("jump_to").is_none());
    let other = json!({
        "buffer_id": 8, "language": "markdown", "line_count": 5, "byte_count": 40,
        "revision": 0, "saved_revision": 0, "path": "/tmp/other.md",
    });
    let fx = s.on_rpc_result(token, Ok(other));
    assert!(fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)));
    assert_eq!(s.view.mode, Mode::Normal, "until the window arrives");
    assert!(
        !all_requests(&fx)
            .iter()
            .any(|(m, _)| *m == "buffer/content"),
        "nothing fetched: the window carries the document"
    );
}

/// `]`/`[` present a target exactly as Enter on the same jumplist row does: the *entry's shape*
/// decides, not the route. A whole-target entry (captured from the Files/view picker) has no
/// position, so a markdown one reads; a positioned entry lands in the editor where its line:col
/// means something. Regression: the step handler used to force the editor unconditionally, which
/// disagreed with select once position-less entries existed.
#[test]
fn jumplist_step_presentation_follows_the_entry_shape() {
    use aether_client::session::Mode;
    use aether_client::update::Event;
    use aether_protocol::cursor::Direction;
    use aether_protocol::jumplist::{JumplistStepResult, JumplistStepScope, JumplistStepTarget};
    use aether_protocol::view::{BufferDescription, ViewOpenResult};
    use aether_protocol::LogicalPosition;

    let opened = |buffer_id: u64, path: &str| ViewOpenResult {
        view_id: aether_protocol::ViewId(buffer_id),
        scroll: None,
        transient: true,
        read: false,
        buffer: BufferDescription {
            buffer_id,
            language: Some("markdown".into()),
            line_count: 5,
            byte_count: 40,
            revision: 0,
            saved_revision: 0,
            path: Some(path.into()),
            scratch_number: None,
            cursor: Default::default(),
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
    };
    let step = |position: Option<LogicalPosition>, buffer_id: u64, path: &str| {
        Event::JumplistStepped(
            Ok(JumplistStepResult::Moved(Box::new(JumplistStepTarget {
                path: Some(path.into()),
                view_id: None,
                position,
                anchor: None,
                index: 1,
                total: 2,
                opened: Some(opened(buffer_id, path)),
                seat: None,
                skipped: 0,
            }))),
            Direction::Forward,
            JumplistStepScope::Full,
        )
    };

    // Either shape adopts the view the step's open presented; which kind that is was the
    // server's call (a positioned step opened with a `jump_to`, which lands in the editor), and
    // the window then says. Until it arrives there is no reading view.
    let mut s = md_session();
    let _ = s.on_event(step(None, 7, "/tmp/notes.md"));
    assert_eq!(s.view.mode, Mode::Normal);
    assert!(s.view.read.is_none());
    assert_eq!(s.view.view_id, ViewId(7));

    let mut s = md_session();
    let _ = s.on_event(step(
        Some(LogicalPosition { line: 3, col: 0 }),
        8,
        "/tmp/doc.md",
    ));
    assert_eq!(s.view.mode, Mode::Normal);
    assert!(s.view.read.is_none());
    assert_eq!(s.view.view_id, ViewId(8));
}

#[test]
fn read_adopt_requests_fence_highlights_and_adopts_them() {
    let mut s = md_session();
    let fx = enter_reader(&mut s, "# T\n\n```rust\nfn x() {}\n```\n");
    // The parse fans out one highlight request per fenced block.
    let (hl_token, method, params) = the_request(&fx);
    assert_eq!(method, "syntax/highlight_snippet");
    assert_eq!(params["language"], json!("rust"));
    assert_eq!(params["text"], json!("fn x() {}"));

    // The result lands keyed by the fence's span start and bumps the layout generation.
    let gen_before = s.view.read.as_ref().unwrap().hl_gen;
    let _ = s.on_rpc_result(
        hl_token,
        Ok(json!({"highlights": [{"start": 0, "end": 2, "kind": "keyword"}]})),
    );
    let read = s.view.read.as_ref().unwrap();
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
    assert_eq!(method, "element/move");
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
    assert_eq!(method, "element/move");
    assert_eq!(params["motion"]["position"], json!({"line": 4, "col": 4}));
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y"
        )),
        "the external link opens like Enter"
    );
}

/// `Space n` on a footnote reference shows the definition's **text**, flattened from the parse.
///
/// Not its source: the popover renders plain text, so slicing the definition's span showed its
/// markup through and led with the `[^1]:` marker that names the very footnote you are standing
/// on.
#[test]
fn space_n_shows_a_footnote_definition_as_text_not_source() {
    use aether_client::session::HoverText;
    let mut s = md_session();
    let _ = enter_reader(&mut s, "A claim[^1].\n\n[^1]: The **bold** definition.\n");
    // On the reference itself (its span starts at byte 7, which is line 0 column 7).
    s.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 0, col: 7 };
    s.on_key(KeyCode::Char(' '), Mods::NONE, None);
    let fx = s.on_key(KeyCode::Char('n'), Mods::NONE, None);
    let shown =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::ShowHover(HoverText::Blocks(b)) if b.len() == 1 => Some(b[0].text.clone()),
                _ => None,
            })
            .expect("the popover shows the definition");
    assert!(
        shown.contains("bold definition"),
        "the definition's text is missing: {shown:?}"
    );
    assert!(
        !shown.contains('*'),
        "markup leaked into the popover: {shown:?}"
    );
    assert!(
        !shown.contains("[^1]"),
        "the label marker is not part of the definition: {shown:?}"
    );
}

/// A click on a footnote reference jumps to its definition (two Gotos: the ref arms, the
/// definition is where reading continues — `z` steps back to the ref).
#[test]
fn read_click_activate_jumps_to_a_footnote_definition() {
    let mut s = md_session();
    let _ = enter_reader(&mut s, "A claim[^1].\n\n[^1]: The definition.\n");
    // The ref's span starts at byte 7.
    let fx = s.read_click_activate(7);
    let gotos: Vec<_> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, params, .. } if *method == "element/move" => {
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
    s.view.buffer.path = Some("/ws/docs/doc.md".into());
    let _ = enter_reader(&mut s, "![d](../img.png)\n");
    let fx = s.read_click_activate(0);
    let (_t, method, _params) = the_request(&fx);
    assert_eq!(method, "element/move");
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
    s.view.buffer.path = Some("/ws/docs/doc.md".into());
    let _ = enter_reader(&mut s, "[next](./other.md)\n");
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
    let _ = enter_reader(
        &mut s,
        "# Title\n\nFirst para.\n\n[docs](https://x.y)\n\nLast para.\n",
    );
    // Walk down: heading → First para (2,0) → the link paragraph, landing at its rest byte
    // AFTER the link (4,19) so the bar shows alone — `l` opts into the link — → Last para.
    for (line, col) in [(2u32, 0u32), (4, 19), (6, 0)] {
        let fx = key(&mut s, 'j');
        let (t, m, params) = the_request(&fx);
        assert_eq!(m, "element/move");
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
        let read = s.view.read.as_ref().unwrap();
        let cursor = s.view.buffer.cursor.position;
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
    let _ = enter_reader(&mut s, "![logo](https://x.y/logo.svg)\n");
    // The lone image promotes to a block element; the boot cursor (0,0) focuses it.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::ShellAction(ShellAction::OpenUrl(url)) if url == "https://x.y/logo.svg"
        )),
        "Enter on a remote image opens the URL"
    );
}

/// The two focus projections: the block-grain reading position (the bar) and the interactive-grain
/// Enter target (the pill) both derive from the one server cursor. A Tab-focused link keeps its
/// containing paragraph as the position, with the link as the target; stepping to a plain paragraph
/// clears the target with no invalidation logic.
#[test]
fn focus_projections_compose_block_bar_and_link_target() {
    use aether_client::markdown::Stop;
    let mut s = read_session();
    // Step to the link paragraph and into its link (`j` `j` `l`), adopting each cursor.
    focus_the_link(&mut s);
    let cursor = s.view.buffer.cursor.position;
    let read = s.view.read.as_ref().unwrap();
    let target = read
        .target_focus(cursor)
        .expect("cursor sits inside the link");
    assert!(matches!(read.elements[target], Stop::Link { .. }));
    let block = read
        .block_focus(cursor)
        .expect("block position always present");
    assert!(matches!(read.elements[block], Stop::Block { .. }));
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
    let cursor = s.view.buffer.cursor.position;
    let read = s.view.read.as_ref().unwrap();
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
    s.view.buffer.path = Some("/ws/docs/doc.md".into());
    let _ = enter_reader(&mut s, "[next](./other.md)\n");
    // The boot cursor (0,0) sits inside the link — Ctrl-Enter opens it in a new window.
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None);
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
    let fx = s.on_key(KeyCode::Enter, Mods::CTRL, None);
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
    s.view.buffer.path = Some("/ws/docs/doc.md".into());
    let _ = enter_reader(&mut s, "![d](../img.png)\n");
    let id = s.view.buffer.buffer_id;
    // The boot cursor (0,0) sits inside the image markup — armed; Enter opens.
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
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

// -------- git commit (Space g c) --------------------------------------------------------------------

/// The whole gesture as a state machine: `Space g c` asks the server to prepare a message, the
/// answer opens that file as a buffer, and `Space Alt-x` in it saves and then commits.
#[test]
fn space_g_c_prepares_a_commit_and_alt_x_commits_it() {
    let mut s = session();

    let fx = git_leader(&mut s, 'c');
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "git/prepare_commit");
    // The repo is resolved server-side from the buffer we're on, and from nothing else.
    assert_eq!(params["buffer_id"], json!(s.view.buffer.buffer_id));
    assert!(params.get("amend").is_none(), "plain commit sends no amend");

    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "repo_id": "/src/aether",
            "path": "/src/aether/.git/COMMIT_EDITMSG",
            "staged": [{"path": "a.rs", "status": "modified"}],
        })),
    );
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "view/open");
    assert_eq!(
        params["absolute_path"],
        json!("/src/aether/.git/COMMIT_EDITMSG")
    );
    // Never transient: a preview auto-closes when hidden, which would discard a half-written
    // message the moment the user glanced at another file.
    assert_eq!(params["transient"], json!(false));

    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "buffer_id": 42,
            "view_id": 420,
            "line_count": 8,
            "byte_count": 200,
            "revision": 1,
            "saved_revision": 1,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );
    assert_eq!(
        s.pending_commit.as_ref().map(|p| p.buffer_id),
        Some(42),
        "the open is what mints the buffer id, so the pending commit adopts it"
    );

    // `Space Alt-x` in the commit buffer: save first, so `git commit -F` reads what was written.
    let fx = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    assert!(no_request(&fx));
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "view/save");

    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "git/commit", "the save is followed by the commit");
    assert_eq!(
        params["repo_id"],
        json!("/src/aether"),
        "the repo that was prepared"
    );
    // One gesture, one report: the intermediate save doesn't toast, or "Saved" and "Committed"
    // stack up and it reads as two things happening.
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })),
        "the save on the way to a commit is silent"
    );

    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "commit": {
                "commit": "a1b2c3d4e5f6",
                "author": "Ada",
                "email": "ada@example.com",
                "date": "2026-08-17 10:00:00 +0100",
                "message": "Add a line\n\nBody.",
            }
        })),
    );
    assert!(s.pending_commit.is_none(), "the commit is done");
    let toast = toast_messages(&fx)
        .first()
        .cloned()
        .expect("a toast reports the commit");
    assert!(toast.contains("a1b2c3d"), "short hash: {toast}");
    assert!(toast.contains("Add a line"), "subject only: {toast}");
    assert!(!toast.contains("Body."), "not the whole message: {toast}");
}

/// Nothing staged is caught before a buffer is opened — `git commit` would only refuse *after*
/// the user had written a message.
#[test]
fn preparing_a_commit_with_nothing_staged_opens_no_buffer() {
    let mut s = session();
    let fx = git_leader(&mut s, 'c');
    let (token, _, _) = the_request(&fx);

    let fx = s.on_rpc_result(
        token,
        Ok(json!({"repo_id": "/src/aether", "path": "/src/aether/.git/COMMIT_EDITMSG"})),
    );
    assert!(no_request(&fx), "no buffer is opened");
    assert!(s.pending_commit.is_none());
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast { title, .. } if title.contains("Nothing staged")
        )),
        "the user is told why nothing happened"
    );
}

/// A refused commit — a failing `pre-commit` hook — keeps the buffer open with the message
/// intact, and shows git's own words. Losing what you wrote because a linter complained would be
/// its own bug.
#[test]
fn a_refused_commit_keeps_the_message_buffer_open() {
    let mut s = session();
    let fx = git_leader(&mut s, 'c');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "repo_id": "/src/aether",
            "path": "/src/aether/.git/COMMIT_EDITMSG",
            "staged": [{"path": "a.rs", "status": "modified"}],
        })),
    );
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "buffer_id": 42,
            "view_id": 420,
            "line_count": 8,
            "byte_count": 200,
            "revision": 1,
            "saved_revision": 1,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    let (token, _, _) = the_request(&fx);

    let fx = s.on_rpc_result(
        token,
        Ok(json!({"message": "lint failed: tabs everywhere"})),
    );
    assert!(no_request(&fx), "no close — the buffer stays put");
    assert!(
        fx.0.iter().any(|e| matches!(
            e,
            Effect::Toast { title, body: Some(body), .. }
                if title == "Commit refused" && body.contains("lint failed: tabs everywhere")
        )),
        "git's own words, unedited — as the detail under a title naming the refusal"
    );
}

/// `Space g Alt-c` amends: the flag rides the prepare *and* the commit, or the message would be
/// written for one and applied as the other.
#[test]
fn space_g_alt_c_amends() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'g');
    let fx = s.on_key(KeyCode::Char('c'), Mods::ALT, None);
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "git/prepare_commit");
    assert_eq!(params["amend"], json!(true));

    let fx = s.on_rpc_result(
        token,
        // No staged files, which for an amend is fine: rewording stages nothing.
        Ok(json!({"repo_id": "/src/aether", "path": "/src/aether/.git/COMMIT_EDITMSG"})),
    );
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "view/open", "an amend opens the buffer regardless");
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "buffer_id": 7,
            "line_count": 8,
            "byte_count": 200,
            "revision": 1,
            "saved_revision": 1,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );

    let _ = s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::ALT, None);
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(view_saved(1)));
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "git/commit");
    assert_eq!(
        params["amend"],
        json!(true),
        "the flag must match the prepare"
    );
}

/// Pressing `Space g c` again while a message is being written must *switch to* it, not prepare
/// over it: rewriting `COMMIT_EDITMSG` under a dirty buffer flags it externally-modified, and the
/// save on the way to the commit then refuses — losing the message to a keystroke meant to
/// resume it.
#[test]
fn space_g_c_again_resumes_the_message_instead_of_overwriting_it() {
    let mut s = session();
    let fx = git_leader(&mut s, 'c');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "repo_id": "/src/aether",
            "path": "/src/aether/.git/COMMIT_EDITMSG",
            "staged": [{"path": "a.rs", "status": "modified"}],
        })),
    );
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "buffer_id": 42,
            "view_id": 420,
            "line_count": 8,
            "byte_count": 200,
            "revision": 1,
            "saved_revision": 1,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );

    // On the message buffer already: nothing is sent, and the user is reminded how to finish.
    let fx = git_leader(&mut s, 'c');
    assert!(no_request(&fx), "no second prepare");
    assert!(fx.0.iter().any(|e| matches!(
        e,
        Effect::Toast { title, .. } if title.contains("Already writing")
    )));

    // From another buffer: switch back to its view, still without re-preparing.
    s.view.buffer.buffer_id = 7;
    let fx = git_leader(&mut s, 'c');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/open");
    assert_eq!(
        params["view_id"],
        json!(420),
        "present the message's own view"
    );
    assert!(params.get("absolute_path").is_none(), "not a fresh prepare");
}

/// A session with a prepared commit message open: `Space g c`, the server's prepared path, and the
/// buffer it opened. The starting point for every close-commits test.
fn prepared_commit_session() -> Session {
    let mut s = session();
    let fx = git_leader(&mut s, 'c');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "repo_id": "/src/aether",
            "path": "/src/aether/.git/COMMIT_EDITMSG",
            "staged": [{"path": "a.rs", "status": "modified"}],
        })),
    );
    let (token, _, _) = the_request(&fx);
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "buffer": 0,
            "buffer_id": 42,
            "view_id": 420,
            "line_count": 8,
            "byte_count": 200,
            "revision": 1,
            "saved_revision": 1,
            "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
        })),
    );
    s
}

/// Closing the message buffer *is* the commit — the `$EDITOR` contract, where git reads the file
/// once the editor exits. So `Space x` fires `git/commit` rather than closing, and the buffer only
/// goes once the commit lands. Nothing is saved first: git reads the *file*, so closing without
/// saving abandons exactly as quitting an editor without writing does.
#[test]
fn closing_the_message_buffer_commits_it() {
    let mut s = prepared_commit_session();
    assert!(s.pending_commit.is_some());

    let fx = leader(&mut s, 'x'); // Space x — close buffer
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "git/commit", "the close commits instead of closing");

    // The commit landing is what closes the buffer.
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "commit": {
                "commit": "abc1234def", "author": "Ada", "email": "a@b.c",
                "date": "2026-08-18 10:00:00 +0100", "message": "Add a line",
            },
        })),
    );
    assert!(s.pending_commit.is_none(), "the pending entry is spent");
    assert!(
        find_request(&fx, "view/close").is_some(),
        "and now the buffer closes"
    );
}

/// Changing your mind: open the message, write nothing, close. git's own rule is that an empty
/// message aborts, so the buffer closes like any other — the alternative is being stuck in a
/// buffer you can't leave without committing something.
#[test]
fn closing_an_empty_message_abandons_the_commit() {
    let mut s = prepared_commit_session();

    let fx = leader(&mut s, 'x');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "git/commit");
    let fx = s.on_rpc_result(token, Ok(json!({ "empty_message": true })));
    assert!(s.pending_commit.is_none());
    assert!(
        find_request(&fx, "view/close").is_some(),
        "the buffer closes rather than trapping the user"
    );
}

/// A refusal — a `pre-commit` hook, most often — keeps the buffer, the message *and* the pending
/// entry, so fixing the complaint and closing again retries. Clearing it on refusal would turn the
/// second attempt into a silent abandon.
#[test]
fn a_refused_commit_keeps_the_buffer_and_retries_on_the_next_close() {
    let mut s = prepared_commit_session();

    let fx = leader(&mut s, 'x');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(json!({ "message": "pre-commit hook failed" })));
    assert!(
        find_request(&fx, "view/close").is_none(),
        "the buffer stays put"
    );
    assert!(
        s.pending_commit.is_some(),
        "and the commit is still pending, so closing again retries"
    );

    let fx = leader(&mut s, 'x');
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "git/commit");
}

/// `Space g z` uncommits, and the toast names what came back — "Uncommitted: Add a line" is a
/// sentence the user can check against their intent; a hash movement isn't.
#[test]
fn space_g_z_uncommits_and_names_what_came_back() {
    let mut s = session();
    let fx = git_leader(&mut s, 'z');
    let (token, method, params) = the_request(&fx);
    assert_eq!(method, "git/reset");
    assert_eq!(params["rev"], json!("HEAD^"));
    assert_eq!(params["buffer_id"], json!(s.view.buffer.buffer_id));
    assert!(params.get("repo_id").is_none(), "the server resolves it");

    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "head": {
                "commit": "aaaa1111", "author": "Ada", "email": "a@b.c",
                "date": "2026-08-17 10:00:00 +0100", "message": "init",
            },
            "undone": [{
                "commit": "bbbb2222", "author": "Ada", "email": "a@b.c",
                "date": "2026-08-17 11:00:00 +0100", "message": "Add a line\n\nBody.",
            }],
        })),
    );
    let toast = toast_messages(&fx)
        .first()
        .cloned()
        .expect("a toast reports the uncommit");
    assert!(toast.contains("Add a line"), "names the commit: {toast}");
    assert!(
        toast.contains("staged"),
        "says where the changes went: {toast}"
    );
    assert!(!toast.contains("Body."), "subject only: {toast}");
}

/// Nothing behind the initial commit: git's refusal is shown as-is rather than as an error.
#[test]
fn uncommitting_with_no_parent_shows_gits_refusal() {
    let mut s = session();
    let fx = git_leader(&mut s, 'z');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Ok(json!({"message": "fatal: ambiguous argument 'HEAD^': unknown revision"})),
    );
    assert!(fx.0.iter().any(|e| matches!(
        e,
        Effect::Toast { body: Some(body), kind, .. }
            if body.contains("ambiguous argument") && *kind == ToastKind::Warning
    )));
}

/// `Space Alt-i` is the diff toggle's Alt sibling: plain toggles the inline view, Alt chooses what
/// it compares against. The pair names one verb at two levels, and the cheap chord stays with the
/// gesture you make many times a session.
#[test]
fn space_alt_i_opens_the_baseline_picker() {
    use aether_protocol::picker::PickerKind;
    let mut s = session();
    s.workspace = "p".into();
    s.workspace_paths = vec!["/p".into()];

    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('i'), Mods::ALT, None);
    let params = find_request(&fx, "picker/view").expect("Space Alt-i opens a picker");
    assert_eq!(params["kind"], "git_baseline");
    assert_eq!(
        params["buffer_id"],
        serde_json::json!(s.view.buffer.buffer_id),
        "the picker resolves its repo from the buffer you are looking at"
    );
    assert_eq!(
        s.picker.as_ref().map(|p| p.kind),
        Some(PickerKind::GitBaseline)
    );
}

/// Enter on a baseline row fires `git/set_baseline` with the row's own `choice`, verbatim — the
/// row and the RPC share a type so there is nothing to translate.
#[test]
fn baseline_picker_enter_sets_the_baseline() {
    use aether_protocol::git::GitBaselineChoice;
    use aether_protocol::picker::{PickerItem, PickerKind};
    let row = |label: &str, choice: Option<GitBaselineChoice>| PickerItem::GitBaseline {
        repo_id: "/p".into(),
        choice,
        label: label.into(),
        match_indices: vec![],
    };
    let open = |selected: u32| {
        let mut s = session();
        s.workspace = "p".into();
        s.workspace_paths = vec!["/p".into()];
        let _ = s.open_picker(PickerKind::GitBaseline, None, None, false, None);
        {
            let p = s.picker.as_mut().expect("picker open");
            p.items = vec![
                row("(index)", None),
                row("(saved)", Some(GitBaselineChoice::Saved)),
                row("main", Some(GitBaselineChoice::Rev { rev: "main".into() })),
            ];
            p.total_matches = 3;
            p.selected = selected;
        }
        s
    };

    let mut s = open(1);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let params = find_request(&fx, "git/set_baseline").expect("Enter sets the baseline");
    assert_eq!(params["repo_id"], "/p");
    assert_eq!(params["source"], serde_json::json!({"kind": "saved"}));
    assert!(s.picker.is_none(), "the picker closes behind the choice");

    let mut s = open(2);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let params = find_request(&fx, "git/set_baseline").expect("Enter sets the baseline");
    assert_eq!(
        params["source"],
        serde_json::json!({"kind": "rev", "rev": "main"})
    );

    // The `index` row clears the baseline: absent `source`, which is the same absent the RPC
    // already took to mean "back to the default".
    let mut s = open(0);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let params = find_request(&fx, "git/set_baseline").expect("Enter sets the baseline");
    assert!(
        params.get("source").is_none(),
        "the default row sends no source, got {params}"
    );
}

/// A clean tree answers `git/show` with no buffer at all, and the client says so in a toast rather
/// than landing on nothing. One line: there is a single fact here, and the title/detail split had
/// the second line restating the first.
#[test]
fn a_clean_tree_toasts_instead_of_switching() {
    use aether_client::update::Event;
    use aether_protocol::git::GitShowResult;

    let mut s = session();
    let before = s.view.buffer.buffer_id;
    let fx = s.on_event(Event::Shown(Ok(GitShowResult {
        opened: None,
        baseline: None,
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(msg.contains("Nothing to commit"), "got {msg:?}");
    assert!(
        !msg.contains("versus"),
        "the default baseline is not a state to announce, got {msg:?}"
    );
    assert_eq!(s.view.buffer.buffer_id, before, "nowhere to switch to");
    assert!(
        find_request(&fx, "view/subscribe").is_none(),
        "and nothing to subscribe to"
    );
}

/// With a baseline pinned, the toast has to name it. "Nothing to commit" alone is only self-evident
/// against the default: the tree can be thick with uncommitted work and still hold nothing *since
/// `main`*, and under the saved-file baseline the view is empty by construction.
#[test]
fn an_empty_view_names_the_baseline_it_found_nothing_against() {
    use aether_client::update::Event;
    use aether_protocol::git::{GitBaselineSource, GitShowResult};

    let mut s = session();
    let fx = s.on_event(Event::Shown(Ok(GitShowResult {
        opened: None,
        baseline: Some(GitBaselineSource::Rev {
            label: "main".into(),
            commit: "abc1234".into(),
        }),
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("Nothing to commit (versus main)"),
        "got {msg:?}"
    );

    let fx = s.on_event(Event::Shown(Ok(GitShowResult {
        opened: None,
        baseline: Some(GitBaselineSource::Saved),
    })));
    let msg = toast_messages(&fx).join(" ");
    assert!(
        msg.contains("Nothing to commit (versus the files on disk)"),
        "the same words the set_baseline confirmation uses, got {msg:?}"
    );
}

/// Refusals coming *out of* the patch view must not be worded as "not in a git repository" — the
/// repo's own diff is what's on screen. `NeedsFile` is the revert refusal, and it points at the key
/// that does work.
#[test]
fn the_patch_views_revert_refusal_does_not_deny_the_repo() {
    use aether_client::update::Event;
    use aether_protocol::git::{ApplyHunkStatus, ApplyScope, GitApplyHunkResult, HunkAction};

    let mut s = session();
    let msg = toast_messages(&s.on_event(Event::HunkApplied {
        action: HunkAction::Revert,
        scope: ApplyScope::Cursor,
        result: Ok(GitApplyHunkResult {
            cursor: Default::default(),
            status: ApplyHunkStatus::NeedsFile,
        }),
    }))
    .join(" ");
    assert!(msg.contains("Revert"), "names the action, got {msg:?}");
    assert!(msg.contains("Enter"), "and the way through, got {msg:?}");
    assert!(!msg.contains("repository"), "got {msg:?}");
}

// ---- composed views bind to the buffer their focused element windows -----------------------------

/// A subscribe result whose element windows `element_buffer`, carrying the focus the server
/// resolved for it.
fn subscribe_over(
    element_buffer: u64,
    focus: aether_protocol::viewport::ViewportFocusElementResult,
) -> aether_protocol::viewport::ViewportSubscribeResult {
    use aether_protocol::viewport::{Element, Window};
    aether_protocol::viewport::ViewportSubscribeResult {
        viewport_id: 7,
        buffer_status: Default::default(),
        focus,
        window: Window {
            other_elements_dirty: false,
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                collapsed: false,
                element: 0,
                buffer: element_buffer,
                rows: 3,
                // A hunk, so the element's lines are nowhere near the view's own line 0.
                first_row: aether_protocol::coords::ElementRow(0),
                laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                role: aether_protocol::ui::ElementRole::Field,
                first_buffer_line: 17,
                lines: vec![],
            },
        },
    }
}

/// The focus a server sends with a composed view's subscribe: element 1, windowing buffer 9, with
/// the cursor seated inside it.
fn focus_on(
    element: u32,
    buffer_id: u64,
    line: u32,
) -> aether_protocol::viewport::ViewportFocusElementResult {
    aether_protocol::viewport::ViewportFocusElementResult {
        element,
        buffer: aether_protocol::view::BufferDescription {
            buffer_id,
            cursor: aether_protocol::cursor::CursorState {
                position: aether_protocol::LogicalPosition { line, col: 0 },
                anchor: aether_protocol::LogicalPosition { line, col: 0 },
                match_bracket: None,
                jumplist_position: None,
            },
            language: None,
            line_count: 40,
            byte_count: 400,
            revision: 1,
            saved_revision: 1,
            path: Some("/repo/a.rs".into()),
            scratch_number: None,
            lsp_server: None,
            title: None,
            commit: None,
            read_only: false,
            is_patch: false,
        },
        buffer_status: Default::default(),
    }
}

/// Subscribing to a **composed** view binds the session to the buffer its focused element windows.
///
/// A patch's elements window real files while the buffer it was opened as is the patch's own
/// document. Holding both at once is two line spaces at the same time: the cursor is a position in
/// the view's document, every rendered line belongs to a file, so nothing draws the cursor and every
/// reveal is owed against a window that can never carry it. In the terminal that became a fetch
/// loop — the scroll re-seated on every reply, so the screen flickered and would not scroll.
#[test]
fn subscribing_to_a_composed_view_binds_to_its_elements_buffer() {
    let mut s = session();
    let view_buffer = s.view.buffer.buffer_id;
    s.adopt_subscribe(subscribe_over(9, focus_on(1, 9, 17)));
    assert_eq!(
        s.view.buffer.buffer_id, 9,
        "the view acts on the file its element windows, not on the patch document"
    );
    assert_eq!(
        s.view.focused_element, 1,
        "and on the element the server focused"
    );
    assert_eq!(
        s.view.buffer.cursor.position.line, 17,
        "with the cursor the server seated inside that element"
    );
    assert_ne!(
        view_buffer, 9,
        "the fixture is only meaningful if they differ"
    );
}

/// `Tab` carries the new element's buffer-level status with it — breadcrumb, diagnostic counts,
/// language-server health, external-change flags.
///
/// All four are facts about the buffer the cursor is in, and crossing an element crosses into
/// another buffer. The reply used to carry only the element and its buffer, so the status bar went
/// on describing the hunk you had just left: the pushes that would have corrected it
/// (`lsp/symbol_path_changed`, `lsp/diagnostics_changed`) are keyed to a buffer *and* only fire on a
/// change, so nothing arrived until something unrelated moved. Raw JSON deliberately — this is the
/// wire shape, and a rename that breaks it should fail here rather than in the status bar.
#[test]
fn focusing_another_element_adopts_its_buffer_status() {
    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));
    // Element 0's status, as a subscribe would have left it.
    s.view.symbol_path = vec![aether_protocol::lsp::SymbolCrumb {
        name: "fn left_behind".into(),
        kind: aether_protocol::picker::SymbolKind::Function,
    }];
    s.view.diagnostics = aether_protocol::lsp::DiagnosticCounts {
        errors: 7,
        ..Default::default()
    };

    s.view.window = Some(window_with_an_input(1));
    let fx = s.on_key(KeyCode::Tab, Mods::NONE, None);
    let token =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request { token, method, .. } if *method == "view/focus_element" => {
                    Some(*token)
                }
                _ => None,
            })
            .expect("Tab focuses the next element");
    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "element": 1,
            "buffer": {
                "buffer_id": 11,
                "language": null,
                "line_count": 40,
                "byte_count": 400,
                "revision": 1,
                "saved_revision": 1,
                "path": "/repo/b.rs",
                "cursor": {
                    "position": {"line": 3, "col": 0},
                    "anchor": {"line": 3, "col": 0},
                },
                "transient": false,
                "read_only": false,
                "is_patch": false,
            },
            "buffer_status": {
                "externally_modified": true,
                "externally_deleted": false,
                "diagnostics": {"errors": 1, "warnings": 2, "infos": 0, "hints": 0},
                "symbol_path": [{"name": "fn arrived", "kind": "function"}],
            },
        })),
    );

    assert_eq!(s.view.buffer.buffer_id, 11, "focus crossed into b.rs");
    assert_eq!(
        s.view
            .symbol_path
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["fn arrived"],
        "the breadcrumb is the new element's, not the one Tab left"
    );
    assert_eq!(
        (s.view.diagnostics.errors, s.view.diagnostics.warnings),
        (1, 2),
        "and so are the diagnostic counts"
    );
    assert!(
        s.view.externally_modified,
        "and the external-change flags, which are per file too"
    );
}

/// An ordinary editor view — one element, windowing the buffer it *is* — keeps its binding. The
/// server still names the focused element (0, over that same buffer), and the session takes the
/// element without re-describing a buffer it already holds.
#[test]
fn subscribing_to_an_ordinary_view_changes_no_binding() {
    let mut s = session();
    let bound = s.view.buffer.buffer_id;
    let cursor = s.view.buffer.cursor.position;
    s.adopt_subscribe(subscribe_over(bound, focus_on(0, bound, cursor.line)));
    assert_eq!(s.view.buffer.buffer_id, bound);
    assert_eq!(s.view.focused_element, 0);
    assert_eq!(
        s.view.buffer.cursor.position, cursor,
        "and the cursor is not moved"
    );
}

/// A press in another element focuses it *and* sets the cursor there — in that order.
///
/// Every shell hit-tests a click to an element, and the rest of the press belongs to that element:
/// the cursor it sets, the drag it anchors, the buffer both act on. Leaving the focus half to the
/// shells meant the terminal did it and the GUI didn't, and in the GUI a click outside the focused
/// element set a cursor the server bounded straight back into the old one — clicking only worked in
/// the focused editor. The order matters as much as the pair: shells send requests as emitted, and
/// the server resolves the cursor against whichever element has focus when it runs.
#[test]
fn a_press_in_another_element_focuses_it_before_setting_the_cursor() {
    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));
    // A second element, windowing another buffer, is what the click lands in.
    if let Some(w) = s.view.window.as_mut() {
        let aether_protocol::viewport::Element::Editor { .. } = &w.root else {
            panic!("fixture is a single editor");
        };
        w.root = aether_protocol::viewport::Element::Column {
            edges: aether_protocol::ui::Edges::NONE,
            band: aether_protocol::ui::Band::None,
            title: Vec::new(),
            children: vec![
                w.root.clone(),
                aether_protocol::viewport::Element::Editor {
                    collapsed: false,
                    element: 1,
                    buffer: 12,
                    rows: 3,
                    first_row: aether_protocol::coords::ElementRow(0),
                    laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                    role: aether_protocol::ui::ElementRole::Field,
                    first_buffer_line: 40,
                    lines: vec![],
                },
            ],
        };
    }

    let fx = s.pointer_press(
        1,
        aether_protocol::LogicalPosition { line: 41, col: 2 },
        aether_protocol::cursor::Granularity::Char,
        false,
    );
    let reqs = all_requests(&fx);
    assert_eq!(
        reqs.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
        vec!["view/focus_element", "element/set"],
        "focus first, then the cursor it decides the scope of"
    );
    assert_eq!(reqs[0].1["target"]["element"], 1);
    assert_eq!(
        reqs[1].1["buffer_id"], 12,
        "the cursor is set in the buffer that element windows"
    );
}

/// A press in the element that already has focus asks for no focus change — the common case, and
/// one round trip is enough for it.
#[test]
fn a_press_in_the_focused_element_only_sets_the_cursor() {
    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));
    let fx = s.pointer_press(
        0,
        aether_protocol::LogicalPosition { line: 18, col: 0 },
        aether_protocol::cursor::Granularity::Char,
        false,
    );
    assert_eq!(
        all_requests(&fx)
            .iter()
            .map(|(m, _)| *m)
            .collect::<Vec<_>>(),
        vec!["element/set"]
    );
}

/// `Space x` on a composed view asks about the **view**, not the element under the cursor.
///
/// A working-changes view holds several documents. Guarding on the focused one meant closing it with
/// unsaved edits in a hunk you had scrolled past went through without a word — nothing was lost
/// (those files are separate buffers and stay open), but a confirm that only sometimes appears is
/// worse than one that always does. The second term is the same flag the status dot reads, so the
/// prompt and the dot cannot disagree about whether the view is dirty.
#[test]
fn closing_a_composed_view_asks_about_unsaved_edits_in_any_element() {
    use aether_client::session::{ConfirmKind, Prompt};
    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));
    // The focused element is clean...
    s.view.buffer.revision = 1;
    s.view.buffer.saved_revision = 1;
    // ...but another element of the same view is not.
    if let Some(w) = s.view.window.as_mut() {
        w.other_elements_dirty = true;
    }

    //  — close is a leader chord; bare  selects a line.
    s.on_key(KeyCode::Char(' '), Mods::NONE, Some(" ".into()));
    let fx = s.on_key(KeyCode::Char('x'), Mods::NONE, Some("x".into()));
    assert!(
        find_request(&fx, "view/close").is_none(),
        "a dirty view must stage a confirm rather than closing straight away"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::DiscardOnClose { label },
            ..
        }) => assert_eq!(
            label,
            &s.view.view_label.joined(),
            "the prompt names the view — the dirty document may not be the focused one"
        ),
        other => panic!("expected a discard-on-close confirm, got {other:?}"),
    }
}

/// `Enter` in a composed view opens the file the focused element windows.
///
/// One verb, several resolvers, chosen by what the cursor is *in*. In a working-changes view the
/// cursor is already inside the real file's document, so the most-wanted destination is that file —
/// and promoting it needs no new operation, because the buffer is already open: opening it *as the
/// view* is an ordinary `view/open`.
///
/// The test is structural rather than a kind flag: "the buffer I am editing is not the one I
/// opened" is what composed means, and it is the same predicate the breadcrumb uses.
///
/// **This knowingly spends `Enter` on the file rather than on go-to-definition.** Inside a patch,
/// go-to-definition genuinely works — the element is a real buffer with a real language server —
/// which is what makes the trade affordable: `Enter` twice gets you there.
#[test]
fn enter_in_a_composed_view_opens_the_focused_elements_file() {
    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));
    let view_buffer = s.view.view_buffer;
    assert_ne!(
        s.view.buffer.buffer_id, view_buffer,
        "the fixture must be composed, or this proves nothing"
    );

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        find_request(&fx, "lsp/goto_definition").is_none(),
        "the file wins over go-to-definition here"
    );
    let open = find_request(&fx, "view/open").expect("Enter promotes the element to a view");
    assert_eq!(
        (&open["view_id"], &open["element"]),
        (&json!(s.view.view_id.get()), &json!(s.view.focused_element)),
        "it names the element through the view it is in — the file the element windows"
    );
    assert!(open.get("buffer_id").is_none(), "no buffer on the wire");
    assert_eq!(
        open["record_nav_from"],
        json!(view_buffer),
        "nav history records the *view*, so Backspace returns to the review"
    );
}

/// An ordinary view is untouched: `Enter` is still go-to-definition, which is the whole point of
/// defining the verb at view level — it degenerates to what it always did when the view has one
/// element.
#[test]
fn enter_in_an_ordinary_view_still_goes_to_the_definition() {
    let mut s = session();
    let bound = s.view.buffer.buffer_id;
    let line = s.view.buffer.cursor.position.line;
    s.adopt_subscribe(subscribe_over(bound, focus_on(0, bound, line)));
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert!(
        find_request(&fx, "lsp/goto_definition").is_some(),
        "one element, so nothing to promote"
    );
    assert!(find_request(&fx, "view/open").is_none());
}

/// `o`/`Alt-o` in the reading view is the **same** symbol navigation the editor uses.
///
/// One outline per view, whatever is looking at it: the breadcrumb, `Space o` and this key all read
/// the language server's document symbols, and for a markdown file those symbols *are* its headings.
/// It used to be a third, client-side implementation walking the parsed AST — a reading-flavoured
/// twin that happened to agree, rather than the same thing.
#[test]
fn read_mode_headings_use_the_same_outline_as_the_editor() {
    let mut s = read_session();
    s.view.viewport_id = Some(7);
    let fx = s.on_key(KeyCode::Char('o'), Mods::NONE, Some("o".into()));
    let (_t, method, params) = the_request(&fx);
    assert_eq!(
        method, "view/navigate_change",
        "the reading view asks the server to step the outline, not its own parse"
    );
    assert_eq!(params["grain"], json!("outline"));
    assert_eq!(params["direction"], json!("next"));

    let fx = s.on_key(KeyCode::Char('o'), Mods::ALT, Some("o".into()));
    let (_t, method, params) = the_request(&fx);
    assert_eq!(method, "view/navigate_change");
    assert_eq!(params["direction"], json!("previous"));
}

/// Selecting an outline row focuses its element *and* lands the cursor — in that order.
///
/// A composed view draws its cursor only inside the **focused** element. Moving it into another
/// element without focusing there puts it outside the window that renders it, so the jump resolved,
/// travelled, was applied, and nothing moved on screen. The order matters as much as the pair: the
/// server resolves a cursor against whichever element holds focus, so setting first would apply the
/// line to a different file.
///
/// The same pair, in the same order, that a click already uses.
#[test]
fn selecting_a_view_row_focuses_its_element_then_sets_the_cursor() {
    use aether_protocol::picker::PickerSelectResult;

    let mut s = session();
    s.adopt_subscribe(subscribe_over(9, focus_on(0, 9, 17)));

    // Drive a real select and answer it with a `ViewElement`. The picker's kind is irrelevant — the
    // client dispatches on the *result*, which is the point of the variant.
    grep_with_groups(&mut s);
    s.picker.as_mut().unwrap().selected = 0;
    let accept = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let token = accept
        .0
        .iter()
        .find_map(|e| match e {
            Effect::Request { token, method, .. } if *method == "picker/select" => Some(*token),
            _ => None,
        })
        .expect("Enter selects the row");
    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "kind": "view_element",
            "element": 1,
            "buffer_id": 11,
            "position": { "line": 42, "col": 0 },
        })),
    );
    let _ = PickerSelectResult::ViewElement {
        element: 1,
        buffer_id: 11,
        position: aether_protocol::LogicalPosition { line: 42, col: 0 },
        open: None,
    };

    let methods: Vec<&str> =
        fx.0.iter()
            .filter_map(|e| match e {
                Effect::Request { method, .. } => Some(*method),
                _ => None,
            })
            .collect();
    assert_eq!(
        methods,
        vec!["view/focus_element", "element/set"],
        "focus first, then the cursor — the other order applies the line to whichever element \
         happens to hold focus"
    );

    let set = find_request(&fx, "element/set").expect("the cursor is set");
    assert_eq!(set["buffer_id"], json!(11), "on the element's own buffer");
    assert_eq!(set["position"]["line"], json!(42));
}

// ---- shell views ---------------------------------------------------------------------------------

/// Put the session on a shell view: a run above, the input below, and the caret wherever
/// `focused` says. The window is what the server would push, since that is the only thing that
/// tells the client this view is a shell at all.
fn shell_session(focused: u32) -> Session {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;

    let mut s = session();
    s.view.viewport_id = Some(7);
    s.view.view_id = ViewId(10);
    s.view.view_buffer = 10;
    s.view.buffer.buffer_id = 10;
    let _ = s.on_event(Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: json!({
            "viewport_id": 7,
            "buffer": 10,
            "revision": 1,
            "window": {
                "max_line_width": 0,
                "root": {"node": "column", "children": [
                    {"node": "row", "band": "chrome", "children": [
                        {"node": "text", "text": "$ echo hi"}]},
                    {"node": "editor", "element": 0, "buffer": 10, "rows": 1, "first_row": 0,
                     "first_buffer_line": 0, "lines": [{"logical_line": 0, "visual_rows": [
                        {"byte_offset": 0, "continuation_indent": 0,
                         "segments": [{"text": "hi", "highlights": []}]}]}]},
                    {"node": "editor", "element": 1, "buffer": 11, "rows": 1, "first_row": 0,
                     "role": "input", "first_buffer_line": 0, "lines": [{"logical_line": 0,
                       "visual_rows": [{"byte_offset": 0, "continuation_indent": 0,
                         "segments": [{"text": "ls -la", "highlights": []}]}]}]},
                ]},
            },
        }),
    }));
    s.view.focused_element = focused;
    // The focused element's buffer is what every text op addresses.
    s.view.buffer.buffer_id = if focused == 1 { 11 } else { 10 };
    s
}

fn shell_run_push(
    view_id: u64,
    command: &str,
    status: serde_json::Value,
) -> aether_client::update::Event {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: aether_protocol::shell::ShellRunChanged::NAME.into(),
        params: json!({
            "view_id": view_id,
            "run": {"run": 1, "command": command, "status": status},
        }),
    })
}

/// `Space Alt-t` always asks for a **new** shell — from an ordinary view and from inside a shell
/// alike. The old "give me the idle one" heuristic went with the shells picker: `Space t` lists
/// what you have, so the open key has one meaning and carries no parameters at all.
#[test]
fn space_alt_t_always_asks_for_a_new_shell() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('t'), Mods::ALT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "shell/open");
    assert_eq!(params, json!({}), "no `new` flag survives");

    let mut s = shell_session(1);
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('t'), Mods::ALT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "shell/open");
    assert_eq!(params, json!({}));
}

/// `Space t` opens the shells picker — the other half of the pair.
#[test]
fn space_t_opens_the_shells_picker() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 't');
    let params = find_request(&fx, "picker/view").expect("the shells picker opens");
    assert_eq!(params["kind"], "shells");
}

/// `Space a` opens the agents picker; `Space Alt-a` always mints a conversation, with no
/// `from_view` to decide anything from.
#[test]
fn space_a_lists_agents_and_alt_a_makes_one() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'a');
    let params = find_request(&fx, "picker/view").expect("the agents picker opens");
    assert_eq!(params["kind"], "agents");

    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('a'), Mods::ALT, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "agent/open");
    assert_eq!(params, json!({}), "no `from_view`, no `agent`");
}

/// `Space b` lists buffers; `Space Alt-b` is the new scratch.
#[test]
fn space_b_lists_buffers_and_alt_b_makes_a_scratch() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'b');
    let params = find_request(&fx, "picker/view").expect("the buffers picker opens");
    assert_eq!(params["kind"], "buffers");

    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('b'), Mods::ALT, None);
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "view/open");
}

/// Opening a shell lands the caret in its input, in Insert.
#[test]
fn opening_a_shell_focuses_the_input_and_enters_insert() {
    let mut s = session();
    let _ = key(&mut s, ' ');
    let fx = s.on_key(KeyCode::Char('t'), Mods::ALT, None);
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "shell/open");

    let _ = s.on_rpc_result(
        token,
        Ok(json!({
            "input": 3,
            "opened": {
                "view_id": 12,
                "buffer_id": 10,
                "line_count": 1,
                "byte_count": 0,
                "revision": 0,
                "saved_revision": 0,
                "path": null,
                "title": "Shell 1",
                "read_only": true,
                "transient": false,
                "cursor": {"position": {"line": 0, "col": 0}, "anchor": {"line": 0, "col": 0}},
                "scroll": {"element": 3, "line": 0, "sub_row": 0.0},
            },
        })),
    );
    assert_eq!(s.view.view_id, ViewId(12));
    assert_eq!(s.view.focused_element, 3, "the caret goes into the input");
    assert_eq!(s.view.mode, aether_client::session::Mode::Insert);
}

/// Insert-mode `Enter` in the input runs the command, exactly as Normal-mode `Enter` does there:
/// which mode you are in decides how text is edited, never whether a command runs. Everywhere
/// else — the transcript, an ordinary file — it is the newline it has always been.
#[test]
fn insert_mode_enter_in_the_input_submits() {
    let mut s = shell_session(1);
    s.view.mode = aether_client::session::Mode::Insert;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/submit_input");
    assert_eq!(params["view_id"], 10);

    // The transcript element of the same view: no input focused, so no submit.
    let mut s = shell_session(0);
    s.view.mode = aether_client::session::Mode::Insert;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "element/newline_and_indent");

    // And in a view with no input at all.
    let mut s = session();
    s.view.mode = aether_client::session::Mode::Insert;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, _) = the_request(&fx);
    assert_eq!(method, "element/newline_and_indent");
}

/// `Alt-Enter` is the newline that survives the input's re-routing — the only way to type a
/// multi-line command or prompt, so it must reach the edit from inside the input too. Shift is
/// ignored on it for the reason every Insert-mode Alt chord ignores it: Insert has no selection
/// to extend, and a held Shift must not drop the chord back onto the submitting row.
#[test]
fn alt_enter_is_the_newline_even_in_the_input() {
    let alt_shift = Mods {
        shift: true,
        ..Mods::ALT
    };
    for mods in [Mods::ALT, alt_shift] {
        for focused in [1, 0] {
            let mut s = shell_session(focused);
            s.view.mode = aether_client::session::Mode::Insert;
            let fx = s.on_key(KeyCode::Enter, mods, None);
            let (_, method, _) = the_request(&fx);
            assert_eq!(
                method, "element/newline_and_indent",
                "focused element {focused}, mods {mods:?}"
            );
        }
    }
}

/// Normal-mode `Enter` on the input runs the command — the one way to run one. Without the guard
/// it would take the composed-view path and open the input as a view of its own.
#[test]
fn normal_mode_enter_on_the_input_submits() {
    let mut s = shell_session(1);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, _) = the_request(&fx);
    // One method for both kinds of composed view: the client cannot tell a shell from an agent
    // conversation, so the server decides what submitting means here.
    assert_eq!(method, "view/submit_input");

    // On the transcript it is `Enter`'s composed-view meaning: follow what the line names.
    let mut s = shell_session(0);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/follow_line");
    assert_eq!(params["view_id"], 10);
}

/// `Enter` follows the line in every composed view, through one method — the client never asks
/// which sort of generated content it is looking at, and an ordinary buffer still goes to the
/// language server without paying a round trip to be told so.
#[test]
fn enter_follows_the_line_in_any_composed_view() {
    // A patch: the flag the open carried.
    let mut s = session();
    s.view.view_id = ViewId(4);
    s.view.buffer.is_patch = true;
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/follow_line");
    assert_eq!(params["view_id"], 4);
    assert_eq!(
        params.as_object().map(|o| o.len()),
        Some(1),
        "no position rides: the cursor is the server's"
    );

    // An ordinary buffer.
    let mut s = session();
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    assert_eq!(the_request(&fx).1, "lsp/goto_definition");
}

/// `Enter` on a hunk of a review opens the file it windows **as its own view** — a switch, even
/// though the cursor is already in that file. The adopter once judged "same buffer" as "a move
/// within this view" and left the review on screen with the cursor nudged; a different view of
/// the buffer you are in is a different window.
#[test]
fn enter_in_a_review_switches_to_the_file_under_the_cursor() {
    let mut s = session();
    // A review: its identity is the patch (view 9 over buffer 9); the cursor is in file 42, which
    // element 2 windows.
    s.view.view_id = ViewId(9);
    s.view.view_buffer = 9;
    s.view.buffer.buffer_id = 42;
    // A review's element windows a **file**, and that is what makes it promotable: a composed
    // view's elements can also be documents internal to it — a conversation's blocks — which are
    // not openable as views of their own and must fall through to `view/follow_line` instead.
    s.view.buffer.path = Some("/p/src/other.rs".into());
    s.view.focused_element = 2;

    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    // The open, picked out by name: a buffer with a real path behind it also asks the server to
    // follow blame, which is nothing to do with what `Enter` did.
    let (token, params) =
        fx.0.iter()
            .find_map(|e| match e {
                Effect::Request {
                    token,
                    method,
                    params,
                    ..
                } if *method == "view/open" => Some((*token, params.clone())),
                _ => None,
            })
            .expect("`Enter` in a review opens the file its element windows");
    assert_eq!(params["view_id"], json!(9), "named through the review");
    assert_eq!(params["element"], json!(2), "the element the cursor is in");
    assert_eq!(
        params["record_nav_from"],
        json!(9),
        "Backspace returns to the review"
    );

    let fx = s.on_rpc_result(
        token,
        Ok(json!({
            "view_id": 42,
            "buffer_id": 42,
            "language": "rust",
            "line_count": 10,
            "byte_count": 100,
            "revision": 1,
            "saved_revision": 1,
            "path": "/p/a.rs",
            "cursor": { "position": {"line": 3, "col": 0}, "anchor": {"line": 3, "col": 0} },
        })),
    );
    assert_eq!(
        s.view.view_id,
        ViewId(42),
        "the file's own view is what is on screen now"
    );
    assert_eq!(s.view.view_buffer, 42, "and it is its own buffer's view");
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a switch re-subscribes the viewport"
    );
}

/// A history step onto a patch — regenerated, so a new view over a new buffer — is a switch, and
/// the step carries the seat: a scroll anchored in the hunk's element, and the cursor there.
#[test]
fn a_history_step_onto_a_regenerated_patch_switches_to_it() {
    use aether_client::update::Event;
    let mut s = session();
    s.view.view_id = ViewId(5);
    s.view.view_buffer = 5;
    s.view.buffer.buffer_id = 5;
    let patch = json!({
        "view_id": 12,
        "buffer_id": 12,
        "language": null,
        "line_count": 40,
        "byte_count": 800,
        "revision": 1,
        "saved_revision": 1,
        "path": null,
        "title": "abc1234 — Add thing",
        "read_only": true,
        "is_patch": true,
        "scroll": { "element": 3, "line": 7, "sub_row": 0.0 },
        "cursor": { "position": {"line": 7, "col": 0}, "anchor": {"line": 7, "col": 0} },
    });
    let fx = s.on_event(Event::NavDone {
        forward: false,
        result: Ok(serde_json::from_value(json!({ "target": patch })).unwrap()),
    });
    assert_eq!(s.view.view_id, ViewId(12), "the patch's view is on screen");
    assert_eq!(s.view.buffer.buffer_id, 12);
    assert!(s.view.buffer.is_patch);
    assert_eq!(
        s.view.buffer.scroll.map(|sc| sc.element),
        Some(3),
        "seated in the hunk's element for the subscribe"
    );
    assert!(
        fx.0.iter().any(|e| matches!(e, Effect::Resubscribe)),
        "a switch re-subscribes"
    );
}

/// A line that leads nowhere is silence, not an error — `Enter` is a common key and most lines of
/// output are not paths.
#[test]
fn following_a_line_that_leads_nowhere_says_nothing() {
    let mut s = shell_session(0);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(json!({})));
    assert!(no_request(&fx));
    assert!(toast_messages(&fx).is_empty());
}

/// A busy shell refuses the submit, and the refusal is information rather than an error — the
/// text you typed is still in the input, waiting.
#[test]
fn a_refused_submit_says_what_is_in_the_way() {
    let mut s = shell_session(1);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Err(aether_client::transport::RpcError {
            method: "shell/run",
            code: aether_protocol::error::ErrorCode::SHELL_BUSY.code(),
            message: "Shell 1 is running sleep 100 — Space v c stops it".into(),
        }),
    );
    let toasts = toast_messages(&fx);
    assert_eq!(
        toasts,
        vec!["Already running — Shell 1 is running sleep 100 — Space v c stops it".to_string()]
    );
    assert!(!has_error_toast(&fx), "busy is not a failure");
}

/// `Space v c` stops whatever the view you are looking at is running.
///
/// One request whatever the view is — the client makes no guess about the kind, so a file view
/// sends it too and the server's `interrupted: false` is what produces the toast. That is the
/// whole reason "Not a shell" is gone: there was never a way for the client to know.
#[test]
fn space_v_c_interrupts_the_focused_view() {
    let mut s = shell_session(1);
    let _ = s.on_event(shell_run_push(10, "sleep 100", json!({"kind": "running"})));
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'v');
    let fx = key(&mut s, 'c');
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "view/interrupt");
    assert_eq!(params["view_id"], 10);

    // A file view asks all the same — and the answer is what says nothing was running.
    let mut s = session();
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'v');
    let fx = key(&mut s, 'c');
    let (token, method, _) = the_request(&fx);
    assert_eq!(method, "view/interrupt");
    let fx = s.on_rpc_result(token, Ok(json!({ "interrupted": false })));
    assert_eq!(
        toast_messages(&fx),
        vec!["Nothing is running here".to_string()]
    );
}

/// A stop that landed says nothing: the finish arrives as a push.
#[test]
fn a_landed_interrupt_is_silent() {
    let mut s = shell_session(1);
    let _ = s.on_event(shell_run_push(10, "sleep 100", json!({"kind": "running"})));
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'v');
    let fx = key(&mut s, 'c');
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(token, Ok(json!({ "interrupted": true })));
    assert!(toast_messages(&fx).is_empty());
}

/// `Esc` in the view sub-leader cancels the chord rather than acting: no verb is bound to it, and
/// an unbound key clears the pending prefix.
#[test]
fn esc_cancels_the_view_sub_leader() {
    let mut s = shell_session(1);
    let _ = s.on_event(shell_run_push(10, "sleep 100", json!({"kind": "running"})));
    let _ = key(&mut s, ' ');
    let _ = key(&mut s, 'v');
    let fx = s.on_key(KeyCode::Esc, Mods::NONE, None);
    assert!(no_request(&fx), "Esc is not a verb in the table");
    // …and the chord is gone: the next `c` is an ordinary Normal-mode key, not the interrupt.
    let fx = key(&mut s, 'c');
    assert!(
        find_request(&fx, "view/interrupt").is_none(),
        "the pending chord was cancelled"
    );
}

/// A run is appended above the input, so the input's element number goes up by one with every
/// command. The caret follows the input, not the number — otherwise `Enter` would start putting
/// newlines into the output of the command it had just run.
#[test]
fn the_caret_follows_the_input_when_a_run_is_appended_above_it() {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;

    let line = |text: &str| {
        json!([{"logical_line": 0, "visual_rows": [
            {"byte_offset": 0, "continuation_indent": 0,
             "segments": [{"text": text, "highlights": []}]}]}])
    };
    let two_runs = || {
        Event::ServerPush(Notification {
            jsonrpc: JsonRpc,
            method: ViewportLinesChanged::NAME.into(),
            params: json!({
                "viewport_id": 7,
                "buffer": 11,
                "revision": 2,
                "window": {
                    "max_line_width": 0,
                    "root": {"node": "column", "children": [
                        {"node": "editor", "element": 0, "buffer": 10, "rows": 1, "first_row": 0,
                         "first_buffer_line": 0, "lines": line("hi")},
                        {"node": "editor", "element": 1, "buffer": 10, "rows": 1, "first_row": 1,
                         "first_buffer_line": 1, "lines": line("there")},
                        {"node": "editor", "element": 2, "buffer": 11, "rows": 1, "first_row": 0,
                         "role": "input", "first_buffer_line": 0, "lines": line("")},
                    ]},
                },
            }),
        })
    };

    // The caret was in the input, element 1; the input is element 2 now.
    let mut s = shell_session(1);
    assert!(s.shell_input_focused());
    let _ = s.on_event(two_runs());
    assert_eq!(s.view.focused_element, 2);
    assert!(s.shell_input_focused(), "`Enter` still submits");

    // A caret parked on a run's output stays on that run.
    let mut s = shell_session(0);
    let _ = s.on_event(two_runs());
    assert_eq!(s.view.focused_element, 0);
    assert!(!s.shell_input_focused());
}

/// A run's start and finish are tracked per view, and a finish elsewhere is announced — while a
/// finish in the shell you are watching is not, because its header already says so.
#[test]
fn a_finished_run_is_announced_only_when_you_are_looking_elsewhere() {
    let mut s = shell_session(1);
    let fx = s.on_event(shell_run_push(
        10,
        "cargo build",
        json!({"kind": "running"}),
    ));
    assert!(toast_messages(&fx).is_empty(), "starting is not news");

    let fx = s.on_event(shell_run_push(
        10,
        "cargo build",
        json!({"kind": "exited", "code": 1}),
    ));
    assert!(
        toast_messages(&fx).is_empty(),
        "the run's own header says how it went"
    );

    // The same finish in a shell you are not looking at.
    let mut s = shell_session(1);
    let _ = s.on_event(shell_run_push(
        99,
        "cargo build",
        json!({"kind": "running"}),
    ));
    let fx = s.on_event(shell_run_push(
        99,
        "cargo build",
        json!({"kind": "exited", "code": 1}),
    ));
    assert_eq!(
        toast_messages(&fx),
        vec!["cargo build — exit 1".to_string()]
    );
}

/// `Up`/`Down` in the input recall commands; on the transcript they stay ordinary motions.
#[test]
fn up_and_down_recall_commands_in_the_input() {
    use aether_protocol::history::{HistoryEntry, HistoryKind};

    let mut s = shell_session(1);
    s.view.mode = aether_client::session::Mode::Insert;
    s.history
        .record(HistoryKind::Shell, HistoryEntry::bare("cargo test"));
    s.history
        .record(HistoryKind::Shell, HistoryEntry::bare("cargo build"));

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    let (_, method, params) = the_request(&fx);
    assert_eq!(method, "element/replace_line");
    assert_eq!(params["text"], "cargo build", "the newest entry first");

    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(the_request(&fx).2["text"], "cargo test");

    let fx = s.on_key(KeyCode::Down, Mods::NONE, None);
    assert_eq!(the_request(&fx).2["text"], "cargo build", "and back down");

    // On the transcript element `Up` is the motion it always was.
    let mut s = shell_session(0);
    s.view.mode = aether_client::session::Mode::Insert;
    s.history
        .record(HistoryKind::Shell, HistoryEntry::bare("cargo build"));
    let fx = s.on_key(KeyCode::Up, Mods::NONE, None);
    assert_eq!(the_request(&fx).1, "element/move");
}

/// Submitting records the command locally, so `Up` recalls it without waiting for the server's
/// copy to come back.
#[test]
fn submitting_records_the_command_for_recall() {
    use aether_protocol::history::HistoryKind;

    let mut s = shell_session(1);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (token, _, _) = the_request(&fx);
    assert!(
        s.history.list(HistoryKind::Shell).is_empty(),
        "not until the server has accepted it"
    );
    // The server says which recall list the line belongs to; the client files it there without
    // ever learning what sort of view it was typed in.
    let _ = s.on_rpc_result(token, Ok(json!({"submitted": true, "history": "shell"})));
    let values: Vec<&str> = s
        .history
        .list(HistoryKind::Shell)
        .iter()
        .map(|e| e.value.as_str())
        .collect();
    assert_eq!(
        values,
        vec!["ls -la"],
        "what the input held when it was sent"
    );
}

/// A line the shell would not accept is said so, mildly — the word at fault is already selected
/// in the input — and is not recalled by `Up`.
#[test]
fn a_rejected_line_is_not_accepted_and_not_recalled() {
    use aether_protocol::history::HistoryKind;

    let mut s = shell_session(1);
    let fx = s.on_key(KeyCode::Enter, Mods::NONE, None);
    let (token, _, _) = the_request(&fx);
    let fx = s.on_rpc_result(
        token,
        Err(aether_client::transport::RpcError {
            method: "shell/run",
            code: aether_protocol::error::ErrorCode::SHELL_REJECTED.code(),
            message: "unknown command `lss`".into(),
        }),
    );
    assert_eq!(
        toast_messages(&fx),
        vec!["Not accepted — unknown command `lss`".to_string()]
    );
    assert!(!has_error_toast(&fx), "a refusal is not a failure");
    assert!(s.history.list(HistoryKind::Shell).is_empty());
}

/// The status indicator names the command you are waiting on, and counts the ones you are not.
#[test]
fn the_work_indicator_names_the_focused_run_and_counts_the_rest() {
    let mut s = shell_session(1);
    assert_eq!(s.work_indicator(), None, "nothing running");

    let _ = s.on_event(shell_run_push(
        10,
        "cargo build",
        json!({"kind": "running"}),
    ));
    assert_eq!(
        s.work_indicator().as_deref(),
        Some("cargo build"),
        "the shell in front of you is named by its command"
    );

    let _ = s.on_event(shell_run_push(99, "npm test", json!({"kind": "running"})));
    assert_eq!(
        s.work_indicator().as_deref(),
        Some("cargo build"),
        "the focused one still wins"
    );

    // Focused shell finishes; the other is counted rather than named.
    let _ = s.on_event(shell_run_push(
        10,
        "cargo build",
        json!({"kind": "exited", "code": 0}),
    ));
    assert_eq!(s.work_indicator().as_deref(), Some("1 running"));
    let _ = s.on_event(shell_run_push(99, "npm test", json!({"kind": "killed"})));
    assert_eq!(s.work_indicator(), None);
}

/// A composed view containing a prose element is **not** the reading view.
///
/// The reader is a whole view — one element over its whole buffer, nothing around it — and the
/// client recognised it by that element's *kind* alone. An agent's reply is prose too, the very
/// same element the reader is now made of, so focusing one replaced the entire conversation with
/// a reading view over that single block: the native client showed one paragraph and nothing else.
#[test]
fn a_prose_block_does_not_turn_a_conversation_into_the_reader() {
    use aether_protocol::viewport::{Element, Window};

    // The reader itself still is one: one element, nothing around it.
    let mut s = session();
    let _ = adopt_reader_window(&mut s, "# A document\n\nProse.");
    assert!(
        s.view.read.is_some(),
        "the reading view stopped recognising itself"
    );

    // A conversation is not, even with the very same element in it and the cursor on it.
    let mut s = session();
    let reader = reader_subscribe(s.view.buffer.buffer_id, "# Findings\n\nProse.");
    let block = reader.window.root.clone();
    assert!(matches!(block, Element::Prose { .. }), "the reply is prose");
    let input = Element::Editor {
        collapsed: false,
        element: 1,
        buffer: s.view.buffer.buffer_id + 1,
        rows: 1,
        first_row: aether_protocol::coords::ElementRow::ZERO,
        laid_out_by: aether_protocol::ui::LayoutOwner::Server,
        role: aether_protocol::ui::ElementRole::Input,
        first_buffer_line: 0,
        lines: vec![],
    };
    let conversation = aether_protocol::viewport::ViewportSubscribeResult {
        window: Window {
            root: Element::column(vec![block, input]),
            ..reader.window
        },
        ..reader
    };
    let _ = s.adopt_subscribe(conversation);
    assert!(
        s.view.read.is_none(),
        "focusing a rendered reply put the whole client into the reading view"
    );
    assert_ne!(s.view.mode, aether_client::session::Mode::Read);
}

// ---- external-change notices --------------------------------------------------------------------

/// A subscribe whose status snapshot already carries the external-change flag has to *say* so.
///
/// The flags used to be announced only from the `buffer/state` push — the file changing while you
/// watched. Arriving in the open snapshot they were installed in silence, which is the shape that
/// hurts most: recover-on-open can put rescued unsaved content on screen in place of the file, so
/// the buffer looks wrong (or, if that content was empty, looks empty) with nothing but a status
/// dot to explain it.
#[test]
fn subscribing_to_a_file_that_changed_on_disk_says_so() {
    let mut s = session();
    let mut sub = subscribe_over(9, focus_on(0, 9, 17));
    sub.buffer_status.externally_modified = true;
    let fx = s.adopt_subscribe(sub);

    assert!(s.view.externally_modified, "the flag is adopted");
    assert_eq!(
        toast_parts(&fx),
        vec![(
            "File changed on disk".to_string(),
            Some("Save to overwrite it, or reload".to_string())
        )],
        "and announced, not installed in silence"
    );
}

/// When the buffer *also* holds unsaved content, the notice says which of the two you are looking
/// at. This is the recover-on-open shape — a backup restored over a file that moved on — and
/// "the file changed" alone would leave the content on screen unexplained.
#[test]
fn a_changed_file_under_unsaved_content_names_the_content() {
    let mut s = session();
    let mut focus = focus_on(0, 9, 17);
    focus.buffer.revision = 4;
    focus.buffer.saved_revision = 1; // restored from a backup: dirty on arrival
    let mut sub = subscribe_over(9, focus);
    sub.buffer_status.externally_modified = true;
    let fx = s.adopt_subscribe(sub);

    let (title, body) = toast_parts(&fx).into_iter().next().expect("a notice");
    assert_eq!(title, "File changed on disk");
    assert!(
        body.as_deref()
            .is_some_and(|b| b.contains("unsaved changes")),
        "the body names the unsaved content on screen, got {body:?}"
    );
}

/// Said once per buffer, not once per message. The flags are a *state* the server re-sends with
/// every snapshot and every push, so announcing on "is it set" would toast on every focus reply.
/// A save clears the flag, and a fresh divergence after that speaks again.
#[test]
fn the_external_change_notice_is_raised_once_per_buffer() {
    let mut s = session();
    let flagged = || {
        let mut sub = subscribe_over(9, focus_on(0, 9, 17));
        sub.buffer_status.externally_modified = true;
        sub
    };
    assert_eq!(toast_parts(&s.adopt_subscribe(flagged())).len(), 1);
    assert!(
        toast_parts(&s.adopt_subscribe(flagged())).is_empty(),
        "re-subscribing to the same flagged buffer repeats nothing"
    );

    // Back in step with disk, then diverged again: a new fact, said again.
    let mut clear = subscribe_over(9, focus_on(0, 9, 17));
    clear.buffer_status.externally_modified = false;
    let _ = s.adopt_subscribe(clear);
    assert_eq!(
        toast_parts(&s.adopt_subscribe(flagged())).len(),
        1,
        "a divergence after the buffer went clean is news again"
    );
}

// ---- closing something that is still going -------------------------------------------------------

/// A `agent/turn_changed` push, as the server sends it.
fn agent_turn_push(view_id: u64, running: bool) -> aether_client::update::Event {
    use aether_client::update::Event;
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    Event::ServerPush(Notification {
        jsonrpc: JsonRpc,
        method: aether_protocol::agent::AgentTurnChanged::NAME.into(),
        params: json!({
            "view_id": view_id,
            "turn": { "running": running },
        }),
    })
}

/// `Space x` on a shell with a command in flight asks first — closing the view kills the process
/// group, which is the same class of loss as discarding unsaved text.
///
/// One gate (`close_confirm_for`) for this and for every picker's `Ctrl-d`, so the two can never
/// disagree about what is worth a prompt.
#[test]
fn closing_a_running_shell_confirms_and_an_idle_one_does_not() {
    use aether_client::session::{ConfirmKind, Prompt};
    let mut s = shell_session(1);
    let _ = s.on_event(shell_run_push(10, "sleep 100", json!({"kind": "running"})));
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'x');
    assert!(fx.0.is_empty(), "the confirm stages, nothing is sent");
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::CloseRunningShell { title },
            ..
        }) => assert_eq!(title, &s.view.view_label.joined()),
        other => panic!("expected a running-shell confirm, got {other:?}"),
    }
    // `y` goes through to the ordinary close.
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    assert!(find_request(&fx, "view/close").is_some());

    // An idle shell closes with no question at all.
    let mut s = shell_session(1);
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'x');
    assert!(s.prompt.is_none(), "nothing is running — nothing to ask");
    assert!(find_request(&fx, "view/close").is_some());
}

/// The agent counterpart: a turn in flight is worth asking about, an idle conversation is not.
#[test]
fn closing_a_busy_agent_confirms() {
    use aether_client::session::{ConfirmKind, Prompt};
    let mut s = shell_session(1);
    let _ = s.on_event(agent_turn_push(10, true));
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'x');
    assert!(fx.0.is_empty());
    assert!(
        matches!(
            &s.prompt,
            Some(Prompt::Confirm {
                kind: ConfirmKind::CloseBusyAgent { .. },
                ..
            })
        ),
        "expected a busy-agent confirm, got {:?}",
        s.prompt
    );

    // The turn ending clears it.
    let mut s = shell_session(1);
    let _ = s.on_event(agent_turn_push(10, true));
    let _ = s.on_event(agent_turn_push(10, false));
    let _ = key(&mut s, ' ');
    let fx = key(&mut s, 'x');
    assert!(s.prompt.is_none());
    assert!(find_request(&fx, "view/close").is_some());
}

/// `Ctrl-d` in the shells and agents pickers goes through the same gate, reading the row's own
/// badge instead of the focused view's state — the picker can close something you are not looking
/// at, so the current view says nothing about it.
#[test]
fn picker_ctrl_d_confirms_a_running_row_and_closes_an_idle_one() {
    use aether_client::session::{ConfirmKind, Prompt};
    use aether_protocol::picker::{AgentRowState, PickerItem, PickerKind};

    let shell_row = |view_id: u64, title: &str, running: bool| PickerItem::Shell {
        view_id: ViewId(view_id),
        title: title.into(),
        cwd: "~/proj".into(),
        last_command: Some("cargo test".into()),
        running,
        exit: None,
        elapsed_ms: None,
        dormant: false,
        match_indices: vec![],
    };

    let mut s = session();
    let _ = s.open_picker(PickerKind::Shells, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![
            shell_row(31, "Shell 1", true),
            shell_row(32, "Shell 2", false),
        ];
        p.offset = 0;
        p.total_matches = 2;
        p.selected = 0;
    }
    let fx = ctrl(&mut s, 'd');
    assert!(
        find_request(&fx, "view/close").is_none(),
        "a running shell asks first"
    );
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::CloseRunningShell { title },
            ..
        }) => assert_eq!(title, "Shell 1"),
        other => panic!("expected a running-shell confirm, got {other:?}"),
    }
    let fx = s.on_key(KeyCode::Char('y'), Mods::NONE, Some("y".into()));
    let close = find_request(&fx, "view/close").expect("the confirm closes it");
    assert_eq!(close["view_id"], json!(31));

    // The idle row closes straight away.
    s.picker.as_mut().unwrap().selected = 1;
    let fx = ctrl(&mut s, 'd');
    assert!(s.prompt.is_none());
    assert_eq!(
        find_request(&fx, "view/close").expect("closes")["view_id"],
        json!(32)
    );

    // An agent blocked on a permission request is *not* idle: the turn is stopped, not over.
    let agent_row = |view_id: u64, title: &str, state: AgentRowState| PickerItem::Agent {
        view_id: ViewId(view_id),
        title: title.into(),
        agent: "Claude Code".into(),
        state,
        last_prompt: None,
        dormant: false,
        match_indices: vec![],
    };
    let mut s = session();
    let _ = s.open_picker(PickerKind::Agents, None, None, false, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.items = vec![
            agent_row(41, "Agent 1", AgentRowState::AwaitingPermission),
            agent_row(42, "Agent 2", AgentRowState::Idle),
        ];
        p.offset = 0;
        p.total_matches = 2;
        p.selected = 0;
    }
    let _ = ctrl(&mut s, 'd');
    match &s.prompt {
        Some(Prompt::Confirm {
            kind: ConfirmKind::CloseBusyAgent { title },
            ..
        }) => assert_eq!(title, "Agent 1"),
        other => panic!("expected a busy-agent confirm, got {other:?}"),
    }
    s.prompt = None;
    s.picker.as_mut().unwrap().selected = 1;
    let fx = ctrl(&mut s, 'd');
    assert!(s.prompt.is_none(), "an idle conversation closes at once");
    assert_eq!(
        find_request(&fx, "view/close").expect("closes")["view_id"],
        json!(42)
    );
}
