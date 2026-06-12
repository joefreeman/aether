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
    assert_eq!(params["motion"], json!({"kind": "selection_edge", "edge": "start"}));
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
    let fx = s.on_rpc_result(token, Err(RpcError {
        method: "input/undo",
        code: 0,
        message: "connection closed".into(),
    }));
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
