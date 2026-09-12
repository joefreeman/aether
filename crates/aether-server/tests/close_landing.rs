//! Where a `view/close` puts you: the step back your own history would have taken.
//!
//! Closing is a move as much as a teardown, and the trail is what says where to. These pin the
//! whole rule — the prune (a close is an explicit "not this"), the back step, the forward step
//! once back is empty, and the MRU fallback for a window whose trail has nothing to say — plus
//! the per-client version of it that rides the `view/closed` push.

mod common;

use common::*;
use std::time::Duration;

/// A workspace holding `files`, activated on one connection.
async fn setup(files: &[(&str, &str)]) -> (aether_server::ServerHandle, Ws) {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }
    let mut server = spawn_for_test("test-proj", vec![dir.path().to_path_buf()])
        .await
        .unwrap();
    server.keep_alive(dir);
    let ws = join(&server).await;
    (server, ws)
}

/// A second (or first) client standing in the fixture's workspace.
async fn join(server: &aether_server::ServerHandle) -> Ws {
    let mut ws = Ws::connect(server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "test-proj".into(),
            open_last: false,
        },
    )
    .await;
    ws
}

/// Open `rel` kept and put this connection's viewport on it — a client switching files.
/// `from` is the origin the open records onto the back stack (`record_nav_from`), which is what
/// makes the jump a step the history knows about.
///
/// Kept, not a preview: these tests come back to files they left, and a preview would have closed
/// itself the moment the next subscribe hid it.
async fn open(ws: &mut Ws, rel: &str, from: Option<u64>) -> ViewOpenResult {
    let opened: ViewOpenResult = send_request::<ViewOpen>(
        ws,
        &ViewOpenParams {
            transient: Some(false),
            path_index: Some(0),
            relative_path: Some(rel.into()),
            record_nav_from: from,
            ..Default::default()
        },
    )
    .await;
    subscribe(ws, opened.view_id, 0).await;
    opened
}

async fn subscribe(ws: &mut Ws, view_id: ViewId, element: u32) -> ViewportSubscribeResult {
    send_request::<ViewportSubscribe>(
        ws,
        &ViewportSubscribeParams {
            view_id,
            cols: 80,
            rows: 10,
            overscan_rows: 0,
            scroll: ScrollPosition {
                element,
                line: 0,
                sub_row: 0.0,
            },
            focus: None,
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
            diff_view: false,
        },
    )
    .await
}

/// `Space x`: close the current view and take the successor it hands back.
async fn close(ws: &mut Ws, view_id: ViewId) -> ViewCloseResult {
    send_request::<ViewClose>(
        ws,
        &ViewCloseParams {
            view_id,
            open_next: true,
        },
    )
    .await
}

async fn step(ws: &mut Ws, buffer_id: u64, direction: Direction) -> Option<ViewOpenResult> {
    send_request::<NavStep>(
        ws,
        &NavStepParams {
            buffer_id,
            direction,
        },
    )
    .await
    .target
}

fn at(line: u32, col: u32) -> LogicalPosition {
    LogicalPosition { line, col }
}

/// A → B: closing B lands back on A, at the cursor A was left with — and the trail is then empty,
/// so nothing reopens B.
///
/// The recorded cursor is the point of the exercise: A's live cursor is moved elsewhere after the
/// jump, so a plain MRU re-open (the old rule, which lands on A too) would show line 0 and only a
/// history step shows line 1.
#[tokio::test]
async fn closing_lands_on_the_file_it_was_opened_from() {
    let (_server, mut ws) =
        setup(&[("a.txt", "alpha\nsecond\nthird\n"), ("b.txt", "bravo\n")]).await;
    let a = open(&mut ws, "a.txt", None).await;
    set_cursor(&mut ws, a.buffer_id, 1, 4).await;
    let b = open(&mut ws, "b.txt", Some(a.buffer_id)).await;

    // Whatever has happened to A since is not where the trail says you were.
    set_cursor(&mut ws, a.buffer_id, 0, 0).await;

    let closed = close(&mut ws, b.view_id).await;
    let landed = closed
        .opened
        .expect("a close with open_next lands somewhere");
    assert_eq!(
        landed.buffer_id, a.buffer_id,
        "back to where B was opened from"
    );
    assert_eq!(
        landed.cursor.position,
        at(1, 4),
        "at the cursor the history recorded, not the one A has been left with"
    );

    // The step was taken, not merely read: nothing remains in either direction, and in particular
    // the closed view was not left on the forward stack for `Alt-Right` to resurrect.
    assert!(
        step(&mut ws, a.buffer_id, Direction::Backward)
            .await
            .is_none(),
        "the trail is spent"
    );
    assert!(
        step(&mut ws, a.buffer_id, Direction::Forward)
            .await
            .is_none(),
        "a close leaves nothing to go forward to"
    );
}

/// A → B → C, stepped back to B: closing B takes the *back* step (A), leaving the forward entry
/// (C) alone. C is the MRU successor, so this is the case where the two rules disagree.
#[tokio::test]
async fn closing_after_a_step_back_keeps_going_back() {
    let (_server, mut ws) = setup(&[
        ("a.txt", "alpha\n"),
        ("b.txt", "bravo\n"),
        ("c.txt", "charlie\n"),
    ])
    .await;
    let a = open(&mut ws, "a.txt", None).await;
    let b = open(&mut ws, "b.txt", Some(a.buffer_id)).await;
    let c = open(&mut ws, "c.txt", Some(b.buffer_id)).await;

    let back = step(&mut ws, c.buffer_id, Direction::Backward)
        .await
        .expect("back to b.txt");
    assert_eq!(back.buffer_id, b.buffer_id);
    subscribe(&mut ws, back.view_id, 0).await;

    let landed = close(&mut ws, b.view_id).await.opened.expect("a successor");
    assert_eq!(
        landed.buffer_id, a.buffer_id,
        "the back stack still held A; the MRU would have said C"
    );
    // And C is still ahead of where we now are.
    assert_eq!(
        step(&mut ws, a.buffer_id, Direction::Forward)
            .await
            .map(|t| t.buffer_id),
        Some(c.buffer_id),
        "the forward stack was left as it was"
    );
}

/// A → B, stepped back to A: the back stack is empty and B is on the forward stack, so closing A
/// goes forward. After stepping back and then closing, the place you came from is the landing.
///
/// B's cursor is moved after the step back, so the restored one proves the forward *entry* was
/// used rather than B merely being what remained.
#[tokio::test]
async fn closing_with_an_empty_back_stack_takes_the_forward_step() {
    let (_server, mut ws) =
        setup(&[("a.txt", "alpha\n"), ("b.txt", "bravo\nsecond\nthird\n")]).await;
    let a = open(&mut ws, "a.txt", None).await;
    let b = open(&mut ws, "b.txt", Some(a.buffer_id)).await;
    set_cursor(&mut ws, b.buffer_id, 2, 3).await;

    let back = step(&mut ws, b.buffer_id, Direction::Backward)
        .await
        .expect("back to a.txt");
    assert_eq!(back.buffer_id, a.buffer_id);
    subscribe(&mut ws, back.view_id, 0).await;
    // The forward entry holds where B *was*; where it is now is somewhere else.
    set_cursor(&mut ws, b.buffer_id, 0, 0).await;

    let landed = close(&mut ws, a.view_id).await.opened.expect("a successor");
    assert_eq!(
        landed.buffer_id, b.buffer_id,
        "forward, since back was empty"
    );
    assert_eq!(
        landed.cursor.position,
        at(2, 3),
        "seated where the forward entry recorded it"
    );
}

/// A → B → A: closing A strikes *every* entry naming it from both stacks, so the trail cannot walk
/// back into the file you just closed. The landing is B, and there is nothing behind it.
#[tokio::test]
async fn a_closed_file_leaves_no_entry_in_the_trail() {
    let (_server, mut ws) = setup(&[("a.txt", "alpha\n"), ("b.txt", "bravo\n")]).await;
    let a = open(&mut ws, "a.txt", None).await;
    let b = open(&mut ws, "b.txt", Some(a.buffer_id)).await;
    // Back to A the way a jump does it — recorded, so the back stack is [A, B].
    let a_again = open(&mut ws, "a.txt", Some(b.buffer_id)).await;
    assert_eq!(a_again.buffer_id, a.buffer_id, "the same file, reopened");

    let landed = close(&mut ws, a.view_id).await.opened.expect("a successor");
    assert_eq!(landed.buffer_id, b.buffer_id, "the step back from A");

    // Without the prune the older A entry is still under B, and one more `Backspace` reopens the
    // file that was just closed.
    assert!(
        step(&mut ws, b.buffer_id, Direction::Backward)
            .await
            .is_none(),
        "no A entry survived the close"
    );
    assert!(
        step(&mut ws, b.buffer_id, Direction::Forward)
            .await
            .is_none(),
        "nor in the other direction"
    );
}

/// A window whose trail has nothing to say falls back to the successor rule it always had: the
/// workspace's MRU top, and a transient scratch once nothing is left at all.
#[tokio::test]
async fn an_empty_trail_falls_back_to_the_mru_successor() {
    let (_server, mut ws) = setup(&[("a.txt", "alpha\n"), ("b.txt", "bravo\n")]).await;
    // Neither open records: nothing ever jumped, so there is no trail.
    let a = open(&mut ws, "a.txt", None).await;
    let b = open(&mut ws, "b.txt", None).await;

    let landed = close(&mut ws, b.view_id).await.opened.expect("a successor");
    assert_eq!(landed.buffer_id, a.buffer_id, "the MRU top");

    let closed = close(&mut ws, view_of(a.buffer_id)).await;
    assert_eq!(
        closed.next_view_id, None,
        "nothing live and nothing dormant"
    );
    let landed = closed.opened.expect("a placeholder");
    assert!(landed.scratch_number.is_some(), "a scratch");
    assert!(landed.transient, "a preview, not a scratch you now own");
}

/// Working changes → `Enter` into one of its files → close the file: the landing is the review,
/// regenerated from its key, seated in the hunk the file was entered from.
///
/// Two changed files, so the hunk is element 1 — element 0 is what a landing that forgot the
/// entry's element would show.
#[tokio::test]
async fn closing_a_file_entered_from_a_review_returns_to_its_hunk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "one.rs", "fn one() {}\n");
    commit_file(&repo, "two.rs", "fn two() {}\n");
    std::fs::write(root.join("one.rs"), "fn one() {}\nfn ADDED_ONE() {}\n").unwrap();
    std::fs::write(root.join("two.rs"), "fn two() {}\nfn ADDED_TWO() {}\n").unwrap();
    let (server, mut ws) = setup_repos_workspace(vec![root.clone()]).await;

    let review: ViewOpenResult = show_buffer(
        &mut ws,
        &GitShowParams {
            repo_id: Some(root.to_string_lossy().into_owned()),
            buffer_id: None,
            target: ShowTarget::WorkingChanges,
            focus_path: None,
            record_nav_from: None,
        },
    )
    .await;
    // Focus the *second* file's hunk, the way scrolling onto it does.
    let bound = subscribe(&mut ws, review.view_id, 1).await;
    let hunk_file = bound.focus.buffer.buffer_id;
    assert_ne!(hunk_file, review.buffer_id, "element 1 windows a file");

    // `Enter` on the hunk: the file becomes a view of its own, and the review is the origin.
    let file: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            view_id: Some(review.view_id),
            element: Some(1),
            record_nav_from: Some(review.buffer_id),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(file.buffer_id, hunk_file, "the file the hunk windows");
    subscribe(&mut ws, file.view_id, 0).await;

    let landed = close(&mut ws, file.view_id)
        .await
        .opened
        .expect("a successor");
    assert!(landed.is_patch, "back in the review");
    assert_eq!(
        landed.scroll.map(|s| s.element),
        Some(1),
        "seated in the hunk it was entered from, not at the top of the patch"
    );
    drop(server);
}

/// A landing that refuses — the review you came from, whose tree has gone clean in the meantime —
/// falls through to the successor rather than failing the close. The buffer is already torn down
/// by then, so an error would leave the client with nothing on screen.
#[tokio::test]
async fn a_landing_that_cannot_be_presented_falls_back_to_the_successor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "fn a() {}\n");
    std::fs::write(root.join("a.rs"), "fn a() {}\nfn ADDED() {}\n").unwrap();
    let (server, mut ws) = setup_repos_workspace(vec![root.clone()]).await;

    let review: ViewOpenResult = show_buffer(
        &mut ws,
        &GitShowParams {
            repo_id: Some(root.to_string_lossy().into_owned()),
            buffer_id: None,
            target: ShowTarget::WorkingChanges,
            focus_path: None,
            record_nav_from: None,
        },
    )
    .await;
    subscribe(&mut ws, review.view_id, 0).await;
    let file: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            view_id: Some(review.view_id),
            element: Some(0),
            record_nav_from: Some(review.buffer_id),
            ..Default::default()
        },
    )
    .await;
    subscribe(&mut ws, file.view_id, 0).await;

    // The change lands on the branch, so there are no working changes to go back to.
    commit_file(&repo, "a.rs", "fn a() {}\nfn ADDED() {}\n");

    let landed = close(&mut ws, file.view_id)
        .await
        .opened
        .expect("the close still lands somewhere");
    assert_ne!(
        landed.buffer_id, file.buffer_id,
        "not the view that was just closed"
    );
    drop(server);
}

/// Two clients on one file, each having reached it from somewhere else: when one closes it, the
/// other's push names *its own* previous location, not the closer's — and not the workspace MRU,
/// which here says something else again.
#[tokio::test]
async fn each_client_is_sent_where_its_own_trail_says() {
    let (server, mut one) = setup(&[
        ("a.txt", "alpha\n"),
        ("b.txt", "bravo\n"),
        ("z.txt", "zulu\n"),
    ])
    .await;
    let mut two = join(&server).await;

    // Client two came to B from A; client one came to B from Z. Touch order leaves the MRU top
    // (after the close) at Z, so "its own trail" and "the MRU" disagree for client two.
    let a = open(&mut two, "a.txt", None).await;
    let z = open(&mut one, "z.txt", None).await;
    let b_two = open(&mut two, "b.txt", Some(a.buffer_id)).await;
    let b_one = open(&mut one, "b.txt", Some(z.buffer_id)).await;
    assert_eq!(b_one.buffer_id, b_two.buffer_id, "one file, two clients");

    let landed = close(&mut one, b_one.view_id)
        .await
        .opened
        .expect("a successor");
    assert_eq!(
        landed.buffer_id, z.buffer_id,
        "the closer follows its own trail"
    );

    let push: ViewClosedParams =
        expect_notification_within::<ViewClosed>(&mut two, Duration::from_secs(5)).await;
    assert_eq!(push.view_id, b_two.view_id, "the view that went away");
    assert_eq!(
        push.next_view_id,
        Some(a.view_id),
        "client two's own previous location, not client one's ({push:?})"
    );
    drop(server);
}

/// A close lands only on entries of the context the client is standing in. The trail built in
/// `proj-a` is still there — the switch neither cleared it nor carried it — but the close happens
/// in `proj-b`, whose trail is empty, so the successor rule answers instead of A's back stack.
///
/// Both workspaces are rooted at `path_index: 0` of their own directory, which is what a
/// cross-workspace landing used to resolve against: the entry named "a1.txt" and the client was
/// standing somewhere that word meant nothing.
#[tokio::test]
async fn a_close_lands_only_on_the_active_context() {
    let dir_a = tempfile::tempdir().unwrap();
    std::fs::write(dir_a.path().join("a1.txt"), "alpha\n").unwrap();
    std::fs::write(dir_a.path().join("a2.txt"), "another\n").unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    std::fs::write(dir_b.path().join("b1.txt"), "bravo\n").unwrap();
    let mut server = spawn_for_test_multi(vec![
        ("proj-a".to_string(), vec![dir_a.path().to_path_buf()]),
        ("proj-b".to_string(), vec![dir_b.path().to_path_buf()]),
    ])
    .await
    .unwrap();
    server.keep_alive(dir_a);
    server.keep_alive(dir_b);

    let mut ws = Ws::connect(&server).await;
    let activate = |name: &'static str| WorkspaceActivateParams {
        worktrees: None,
        name: name.into(),
        open_last: false,
    };
    let _: WorkspaceActivateResult =
        send_request::<WorkspaceActivate>(&mut ws, &activate("proj-a")).await;
    let a1 = open(&mut ws, "a1.txt", None).await;
    let a2 = open(&mut ws, "a2.txt", Some(a1.buffer_id)).await;

    let _: WorkspaceActivateResult =
        send_request::<WorkspaceActivate>(&mut ws, &activate("proj-b")).await;
    let b1 = open(&mut ws, "b1.txt", None).await;

    let landed = close(&mut ws, b1.view_id)
        .await
        .opened
        .expect("a close with open_next lands somewhere");
    assert_ne!(landed.buffer_id, a1.buffer_id, "not A's back stack");
    assert_ne!(landed.buffer_id, a2.buffer_id, "not A's other file either");
    assert!(
        landed.path.is_none(),
        "nothing left in this context, so the placeholder scratch: {:?}",
        landed.path
    );

    // A's trail is untouched by the close over in B — going back finds it as it was.
    let _: WorkspaceActivateResult =
        send_request::<WorkspaceActivate>(&mut ws, &activate("proj-a")).await;
    let a2_again = open(&mut ws, "a2.txt", None).await;
    assert_eq!(a2_again.buffer_id, a2.buffer_id);
    assert_eq!(
        step(&mut ws, a2.buffer_id, Direction::Backward)
            .await
            .map(|t| t.buffer_id),
        Some(a1.buffer_id),
        "the trail A kept is the one A's steps take"
    );
    drop(server);
}
