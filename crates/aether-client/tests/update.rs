//! The sans-IO payoff (docs/client-core.md): the update loop tested as a pure state
//! machine — key events in, `Effect::Request`s out, canned JSON results back in — with no
//! transport, no mock, no async runtime.

use aether_client::effect::{Effect, Effects, ToastKind};
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
        fx.0.iter().any(|e| matches!(e, Effect::RevealCursor)),
        "a cursor move reveals the cursor"
    );
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
    assert_eq!(method, "input/undo");
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
            method: "input/undo",
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
            method: "input/undo",
            code: 0,
            message: "connection closed".into(),
        }),
    );
    assert!(fx.0.is_empty());
}

#[test]
fn input_is_suspended_while_disconnected() {
    let mut s = session();
    let _ = s.on_event(aether_client::update::Event::ConnectionLost);
    let fx = key(&mut s, 'i');
    assert!(fx.0.is_empty(), "keys are inert until reestablished");
    assert_eq!(s.mode, aether_client::session::Mode::Normal);
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
    let _ = s.on_event(Event::SwitchedPrimed(Ok(Some(("needle".into(), open)))));

    assert!(
        s.search.active,
        "the primed search is active after the switch"
    );
    assert_eq!(s.search.query, "needle");
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
    let _ = s.open_picker(PickerKind::Grep, None, None);
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
    let fx = s.open_picker(PickerKind::Grep, None, None);
    assert!(
        !fx.0.iter().any(|e| matches!(e, Effect::PickerScrollReset)),
        "grep (state-preserving) open must not reset the scroll — it resumes onto its selection"
    );

    let mut s = session();
    let fx = s.open_picker(PickerKind::Files, None, None);
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
    assert!(fx.0.iter().any(|e| matches!(e, Effect::RevealCursor)));

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

#[test]
fn explorer_delete_confirms_then_trashes_and_relists() {
    use aether_client::session::Prompt;
    use aether_protocol::picker::{PickerItem, PickerKind};

    let mut s = session();
    let _ = s.open_picker(PickerKind::Explorer, None, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
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
        Some(Prompt::Confirm { message, .. }) => {
            assert!(
                message.contains("Delete file \"old.rs\""),
                "got {message:?}"
            );
        }
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
    // The result re-lists the still-open Explorer.
    let fx = s.on_rpc_result(token, Ok(json!({"closed_buffer_ids": []})));
    assert!(
        find_request(&fx, "picker/view").is_some(),
        "a successful delete re-lists the explorer"
    );
}

#[test]
fn explorer_create_makes_a_file_with_create_if_missing() {
    use aether_protocol::picker::PickerKind;

    let mut s = session();
    s.project_paths = vec!["/proj".into()];
    let _ = s.open_picker(PickerKind::Explorer, None, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "new.rs".into();
        p.cursor = p.query.len();
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
    let _ = s.open_picker(PickerKind::Explorer, None, None);
    {
        let p = s.picker.as_mut().unwrap();
        p.directory = Some("/proj/src".into());
        p.query = "sub/".into();
        p.cursor = p.query.len();
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
    let _ = s.open_picker(PickerKind::Explorer, None, None);
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
