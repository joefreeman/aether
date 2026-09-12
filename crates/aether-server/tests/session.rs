//! What the session file records and what a restore does with it.
//!
//! The unit is the workspace's MRU **view** list, verbatim minus clean scratches — previews
//! included, each marked as one. That is what makes "exit in a diff, come back to it" work, and it
//! is only safe because the hide collector has already closed every preview nothing shows: the
//! transient views in the MRU at any instant are exactly the ones a connected window is looking at.
//! The other half of the rule is that transience is honoured for the **landing** only — every other
//! preview a restore brings back is dropped once activation has decided where to land.

mod common;

use common::*;

/// Activate `name`, optionally landing on the workspace's last view.
async fn activate(ws: &mut Ws, name: &str, open_last: bool) -> WorkspaceActivateResult {
    send_request::<WorkspaceActivate>(
        ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: name.into(),
            open_last,
        },
    )
    .await
}

/// The session file's recorded views for `workspace`, as raw JSON objects — the shape is what is
/// under test, so this deliberately doesn't go through the server's own types.
fn recorded(sessions: &std::path::Path, workspace: &str) -> Vec<serde_json::Value> {
    let Ok(raw) = std::fs::read_to_string(sessions) else {
        return Vec::new();
    };
    let json: serde_json::Value = serde_json::from_str(&raw).expect("the session file parses");
    json["workspaces"][workspace]["views"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn head_of(repo: &git2::Repository) -> String {
    repo.head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string()
}

fn show_commit(root: &std::path::Path, rev: &str) -> GitShowParams {
    GitShowParams {
        repo_id: Some(root.to_string_lossy().into_owned()),
        buffer_id: None,
        target: ShowTarget::Commit { rev: rev.into() },
        focus_path: None,
        record_nav_from: None,
    }
}

/// A file **at** a revision, the one git target that is a document rather than a view of one.
fn show_file_at(root: &std::path::Path, rev: &str, path: &str) -> GitShowParams {
    GitShowParams {
        repo_id: Some(root.to_string_lossy().into_owned()),
        buffer_id: None,
        target: ShowTarget::File {
            rev: rev.into(),
            path: path.into(),
        },
        focus_path: None,
        record_nav_from: None,
    }
}

/// What the buffers picker lists, by display name.
async fn buffer_rows(ws: &mut Ws) -> Vec<String> {
    send_request::<PickerView>(ws, &view_params(PickerKind::Buffers))
        .await
        .update
        .map(|u| {
            u.items()
                .iter()
                .filter_map(|i| match i {
                    PickerItem::Buffer { display, .. } => Some(display.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Write the `p.toml` a **cold** load needs. The test seams pre-register workspaces, which takes
/// the already-loaded path and restores nothing; a second life has to find the workspace on disk.
fn declare_workspace_on_disk(root: &std::path::Path) -> std::path::PathBuf {
    let store = root.join("workspaces");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("p.toml"),
        format!("[[roots]]\npath = {:?}\n", root.display().to_string()),
    )
    .unwrap();
    store
}

/// A repo with one commit, a workspace `p` over it, and a connected, activated client.
async fn repo_workspace(
    root: &std::path::Path,
    sessions: &std::path::Path,
) -> (aether_server::ServerHandle, Ws) {
    let server = aether_server::spawn_for_test_multi_with_sessions(
        vec![("p".to_string(), vec![root.to_path_buf()])],
        Some(sessions.to_path_buf()),
    )
    .await
    .unwrap();
    let mut ws = Ws::connect(&server).await;
    activate(&mut ws, "p", false).await;
    (server, ws)
}

/// Showing a diff is enough on its own to write the session file. It has no handler of its own that
/// persists — the write is the request dispatch's, off the dirty mark `view/open`'s MRU touch left.
/// That is the whole point of making persistence structural: a method nobody thought about records
/// itself, and the diff you were looking at is there to come back to.
#[tokio::test]
async fn opening_a_diff_writes_the_session_with_no_other_action() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");
    let (server, mut ws) = repo_workspace(&root, &sessions).await;
    assert!(
        recorded(&sessions, "p").is_empty(),
        "nothing open yet: {:?}",
        recorded(&sessions, "p")
    );

    let opened = show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
    assert!(opened.transient, "a revision opens as a preview");

    // Read straight after the response, with no other request in between: the flush runs before
    // the reply, so a client that reads the file the moment it sees the response sees the write.
    let views = recorded(&sessions, "p");
    assert_eq!(views.len(), 1, "the diff alone: {views:?}");
    assert_eq!(views[0]["kind"], "virtual", "{views:?}");
    assert_eq!(
        views[0]["transient"],
        serde_json::json!(true),
        "recorded as the preview it is: {views:?}"
    );

    drop(server);
}

/// **The hide collector must never write.** A client that quits while previewing a diff has the
/// preview closed out from under it by the disconnect teardown — and if that teardown persisted,
/// it would erase the very entry this exists to keep. The file must still name the diff, at the
/// front and transient, after the buffer is gone.
#[tokio::test]
async fn a_disconnect_in_a_preview_leaves_it_at_the_front_of_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");
    let (server, mut ws) = repo_workspace(&root, &sessions).await;

    let opened = show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
    // Actually *shown*, so the disconnect really runs the collector over it.
    let _: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &transient_sub_params(opened.buffer_id)).await;
    let before = recorded(&sessions, "p");
    assert_eq!(before.len(), 1, "the preview is recorded: {before:?}");

    drop(ws);
    wait_for_client_count(&server, 0).await;

    assert!(
        !server
            .state
            .lock()
            .await
            .buffers
            .contains_key(&opened.buffer_id),
        "the collector closed the preview nothing shows any more"
    );
    assert_eq!(
        recorded(&sessions, "p"),
        before,
        "and wrote nothing while doing it"
    );

    drop(server);
}

/// Leaving a workspace is the collector's other no-user-action moment: the preview you were in is
/// closed as you go, and the workspace you left must be recorded exactly as it was. A cold restore
/// then lands on it.
#[tokio::test]
async fn a_workspace_leave_writes_nothing_and_the_preview_is_still_the_landing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let elsewhere = dir.path().join("q");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let sessions = root.join("sessions.json");

    let server = aether_server::spawn_for_test_multi_with_sessions(
        vec![
            ("p".to_string(), vec![root.clone()]),
            ("q".to_string(), vec![elsewhere.clone()]),
        ],
        Some(sessions.clone()),
    )
    .await
    .unwrap();
    let mut ws = Ws::connect(&server).await;
    activate(&mut ws, "p", false).await;
    let opened = show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
    let _: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &transient_sub_params(opened.buffer_id)).await;
    let before = recorded(&sessions, "p");
    assert_eq!(before.len(), 1, "the preview is recorded: {before:?}");

    // Leave. The teardown closes the preview; nothing about `p` may be rewritten.
    activate(&mut ws, "q", false).await;
    assert!(
        !server
            .state
            .lock()
            .await
            .buffers
            .contains_key(&opened.buffer_id),
        "leaving closed the preview"
    );
    assert_eq!(
        recorded(&sessions, "p"),
        before,
        "the workspace we left is untouched"
    );

    drop(ws);
    drop(server);

    // A cold load of `p` lands on it.
    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    let activated = activate(&mut ws, "p", true).await;
    let landed = activated.opened.expect("open_last lands somewhere");
    assert!(landed.title.is_some(), "a revision, which has a title");
    assert!(landed.is_patch, "the diff, not a scratch");
    assert!(landed.transient, "and still a preview");

    drop(server);
}

/// The landing honours transience, and only the landing: a preview restored as the landing is a
/// preview again, so it closes as soon as you look at something else — and closing it that way is
/// the collector, which writes nothing, so the *next* thing you open is what the session records.
#[tokio::test]
async fn a_restored_preview_is_the_landing_and_closes_when_you_move_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");

    {
        let (server, mut ws) = repo_workspace(&root, &sessions).await;
        show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
        drop(ws);
        drop(server);
    }

    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    let activated = activate(&mut ws, "p", true).await;
    let landed = activated.opened.expect("open_last lands somewhere");
    assert!(landed.is_patch, "the diff is where we left off");
    assert!(landed.transient, "as the preview it was");
    let _: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &transient_sub_params(landed.buffer_id)).await;

    // Move on. The switch collects the preview, and the write the open dirtied names the file.
    let file: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            path_index: Some(0),
            relative_path: Some("a.rs".into()),
            ..Default::default()
        },
    )
    .await;
    let _: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &transient_sub_params(file.buffer_id)).await;
    assert!(
        !server
            .state
            .lock()
            .await
            .buffers
            .contains_key(&landed.buffer_id),
        "the preview closed once nothing showed it"
    );
    let views = recorded(&sessions, "p");
    assert_eq!(views.len(), 1, "the file alone: {views:?}");
    assert_eq!(views[0]["kind"], "file", "{views:?}");

    drop(server);
}

/// `Space k` on a **file at a revision** is unchanged by any of this: it is a genuine read-only
/// document, so a kept one is recorded without the preview flag, comes back kept, and is a listed
/// row rather than a landing that evaporates.
#[tokio::test]
async fn a_kept_file_at_a_revision_still_restores_kept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");
    let head = head_of(&repo);

    {
        let (server, mut ws) = repo_workspace(&root, &sessions).await;
        let opened = show_buffer(&mut ws, &show_file_at(&root, &head, "a.rs")).await;
        let kept: ViewSetTransientResult = send_request::<ViewSetTransient>(
            &mut ws,
            &ViewSetTransientParams {
                view_id: opened.view_id,
                transient: false,
            },
        )
        .await;
        assert!(!kept.transient);
        let views = recorded(&sessions, "p");
        assert_eq!(views.len(), 1, "{views:?}");
        assert!(
            views[0].get("transient").is_none(),
            "a kept view carries no preview flag: {views:?}"
        );
        drop(ws);
        drop(server);
    }

    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    let activated = activate(&mut ws, "p", true).await;
    let landed = activated.opened.expect("open_last lands somewhere");
    assert!(landed.read_only, "the revision came back");
    assert!(!landed.transient, "kept, as it was recorded");
    assert!(
        buffer_rows(&mut ws)
            .await
            .iter()
            .any(|d| d.contains("a.rs")),
        "and is a listed row"
    );

    drop(server);
}

/// A commit's patch cannot be kept any more — but a `sessions.json` written before that rule can
/// say one was. Such an entry is restored as a **preview** whatever it recorded: honoured as the
/// landing, dropped otherwise, and never a listed row. Left kept it would be a dormant row no
/// picker lists and no collector reaches, re-written on every persist for ever.
#[tokio::test]
async fn a_legacy_kept_commit_entry_restores_only_as_the_landing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");

    // Write the old shape by taking a current one and deleting the preview flag — the entry a
    // pre-stage-4 `Space k` on a diff left behind.
    {
        let (server, mut ws) = repo_workspace(&root, &sessions).await;
        show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
        drop(ws);
        drop(server);
    }
    let mut legacy: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sessions).unwrap()).unwrap();
    for entry in legacy["workspaces"]["p"]["views"]
        .as_array_mut()
        .expect("the diff is recorded")
    {
        entry
            .as_object_mut()
            .expect("an entry is an object")
            .remove("transient");
    }
    std::fs::write(&sessions, serde_json::to_string(&legacy).unwrap()).unwrap();
    assert!(
        !recorded(&sessions, "p")[0]
            .as_object()
            .unwrap()
            .contains_key("transient"),
        "the fixture is a kept entry: {legacy}"
    );

    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    let landed = activate(&mut ws, "p", true)
        .await
        .opened
        .expect("open_last lands somewhere");
    assert!(landed.is_patch, "the diff is still where you left off");
    assert!(landed.transient, "but only as the preview it always was");
    assert!(
        buffer_rows(&mut ws).await.is_empty(),
        "and is no row: a patch is a view, not a buffer"
    );

    drop(server);
}

/// Two windows leave two previews in one list, and only the front one is where you left off. The
/// rest are dropped once the landing is decided — a dormant row is re-written on every persist, so
/// an unopened preview would otherwise never die.
#[tokio::test]
async fn two_previews_leave_only_the_front_after_activation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let first = head_of(&repo);
    commit_file(&repo, "a.rs", "two\n");
    let second = head_of(&repo);
    let sessions = root.join("sessions.json");

    {
        // Two windows on one workspace, each previewing a different commit. Neither is collected:
        // each is still shown by the window that opened it.
        let server = aether_server::spawn_for_test_multi_with_sessions(
            vec![("p".to_string(), vec![root.clone()])],
            Some(sessions.clone()),
        )
        .await
        .unwrap();
        let mut a = Ws::connect(&server).await;
        activate(&mut a, "p", false).await;
        let mut b = Ws::connect(&server).await;
        activate(&mut b, "p", false).await;
        show_buffer(&mut a, &show_commit(&root, &first)).await;
        show_buffer(&mut b, &show_commit(&root, &second)).await;

        let views = recorded(&sessions, "p");
        assert_eq!(views.len(), 2, "both previews are recorded: {views:?}");
        assert!(
            views
                .iter()
                .all(|v| v["transient"] == serde_json::json!(true)),
            "both as previews: {views:?}"
        );
        // Most-recently-used first: `b`'s is the front.
        assert!(
            views[0]["key"].as_str().unwrap().contains(&second[..8]),
            "the last one looked at is the front: {views:?}"
        );
        drop(a);
        drop(b);
        drop(server);
    }

    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    activate(&mut ws, "p", true).await;

    let views = recorded(&sessions, "p");
    assert_eq!(views.len(), 1, "only the landing survived: {views:?}");
    assert!(
        views[0]["key"].as_str().unwrap().contains(&second[..8]),
        "and it is the front one: {views:?}"
    );

    drop(server);
}

/// A tethered launch (`ae file.rs`) lands on the file it was given, not on the preview — so the
/// preview must not linger as a listed row either. `workspace/open_path` activates with
/// `open_last: false`, which honours nothing's transience and drops every restored preview.
#[tokio::test]
async fn a_tethered_launch_drops_the_preview_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    commit_file(&repo, "a.rs", "one\n");
    let sessions = root.join("sessions.json");

    {
        let (server, mut ws) = repo_workspace(&root, &sessions).await;
        show_buffer(&mut ws, &show_commit(&root, &head_of(&repo))).await;
        assert_eq!(recorded(&sessions, "p").len(), 1);
        drop(ws);
        drop(server);
    }

    let store = declare_workspace_on_disk(&root);
    let server = aether_server::spawn_for_test_multi_with_sessions(vec![], Some(sessions.clone()))
        .await
        .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    // No `workspace/activate` first: the path is what says which workspace this is, exactly as
    // `ae file.rs` does.
    let opened: WorkspaceActivateResult = send_request::<WorkspaceOpenPath>(
        &mut ws,
        &WorkspaceOpenPathParams {
            path: root.join("a.rs").display().to_string(),
            transient: None,
            create_if_missing: false,
            jump_to: None,
        },
    )
    .await;
    assert_eq!(opened.workspace.name, "p", "inferred from the path");
    let landed = opened.opened.expect("the file it was given");
    assert!(!landed.is_patch, "landed on the file, not the diff");

    let views = recorded(&sessions, "p");
    assert_eq!(views.len(), 1, "the preview row is gone: {views:?}");
    assert_eq!(views[0]["kind"], "file", "{views:?}");
    assert!(
        server.state.lock().await.workspaces["p"]
            .dormant_views
            .is_empty(),
        "and nothing dormant is left to re-persist it"
    );

    drop(server);
}
