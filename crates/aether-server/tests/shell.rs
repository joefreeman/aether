//! `shell/*` end to end: opening a shell view, running commands in it, and stopping them.
//!
//! Everything here goes through the real WebSocket and the real dispatch, and every command is a
//! real process — the shell's own language, run by the shell's own executor. The output assertions
//! read the transcript the way a client does — through a viewport's window — so a test cannot pass
//! on a document the shells would never render.

mod common;
use common::*;

// ---- fixtures -----------------------------------------------------------------------------------

/// A workspace with one file in it, ready for `shell/open`.
async fn setup() -> (aether_server::ServerHandle, Ws, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("shell-proj", vec![root]).await.unwrap();
    server.keep_alive(());
    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "shell-proj".into(),
            open_last: false,
        },
    )
    .await;
    (server, ws, dir)
}

async fn open_shell(ws: &mut Ws, new: bool) -> ShellOpenResult {
    send_request::<ShellOpen>(ws, &ShellOpenParams { new }).await
}

/// Subscribe a viewport to a shell's view, wide and tall enough to hold everything these tests
/// produce, and load the whole thing.
async fn shell_window(
    ws: &mut Ws,
    open: &ShellOpenResult,
) -> (u64, aether_protocol::viewport::Window) {
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        ws,
        &ViewportSubscribeParams {
            view_id: open.opened.view_id,
            cols: 200,
            rows: 200,
            overscan_rows: 0,
            scroll: open.opened.scroll.unwrap_or(ScrollPosition {
                element: 0,
                line: 0,
                sub_row: 0.0,
            }),
            focus: None,
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
            diff_view: false,
        },
    )
    .await;
    let window = whole_view(ws, sub.viewport_id, sub.window).await;
    (sub.viewport_id, window)
}

/// Type `text` into the shell's input, exactly as a keystroke would.
async fn type_command(ws: &mut Ws, open: &ShellOpenResult, input_buffer: u64, text: &str) {
    let _: EditResult = send_request::<InputText>(
        ws,
        &InputTextParams {
            buffer_id: input_buffer,
            text: text.into(),
            select_pasted: false,
            at: None,
            replace_selection: false,
        },
    )
    .await;
    let _ = open;
}

/// The buffer the shell's input element windows, found by its **role** rather than by the index
/// the open reported — that index moves as runs are appended above it, which is exactly the sort
/// of drift the role exists to remove.
async fn input_buffer_of(server: &aether_server::ServerHandle, open: &ShellOpenResult) -> u64 {
    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the shell's view");
    view.elements
        .iter()
        .find(|e| e.role.is_input())
        .expect("a shell view has an input")
        .buffer_id
}

/// Move this viewport's focus, as `Tab` and `Shift-Tab` do.
async fn focus(
    ws: &mut Ws,
    viewport_id: u64,
    target: aether_protocol::viewport::FocusTarget,
) -> aether_protocol::viewport::ViewportFocusElementResult {
    send_request::<aether_protocol::viewport::ViewportFocusElement>(
        ws,
        &aether_protocol::viewport::ViewportFocusElementParams {
            viewport_id,
            target,
        },
    )
    .await
}

/// Which element the server has this viewport's caret in.
async fn focused_element(server: &aether_server::ServerHandle, viewport_id: u64) -> u32 {
    let s = server.state.lock().await;
    s.viewports
        .get(&viewport_id)
        .expect("the subscribed viewport")
        .focused
}

/// What the input holds right now. Read through `buffer/content` rather than a viewport, because
/// the input has no view of its own to subscribe to — it is a field of the shell's.
async fn input_text(ws: &mut Ws, input: u64) -> String {
    send_request::<BufferContent>(ws, &BufferContentParams { buffer_id: input })
        .await
        .text
}

/// Run `command` and wait for it to finish, answering its final state.
async fn run_and_wait(
    ws: &mut Ws,
    server: &aether_server::ServerHandle,
    open: &ShellOpenResult,
    command: &str,
) -> RunState {
    let input = input_buffer_of(server, open).await;
    type_command(ws, open, input, command).await;
    let _: ShellRunResult = send_request::<ShellRun>(
        ws,
        &ShellRunParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    finished_run(ws).await
}

/// Read pushes until a run reports that it is over.
async fn finished_run(ws: &mut Ws) -> RunState {
    loop {
        let p = expect_notification::<ShellRunChanged>(ws).await;
        if let Some(run) = p.run.filter(|r| !r.is_running()) {
            return run;
        }
    }
}

/// Every line of text the view's editors are showing, in order.
fn body(window: &aether_protocol::viewport::Window) -> Vec<String> {
    window
        .root
        .lines()
        .iter()
        .map(|l| {
            l.visual_rows
                .iter()
                .flat_map(|r| r.segments.iter())
                .map(|s| s.text.as_str())
                .collect::<String>()
        })
        .collect()
}

/// The text of every chrome row in the view — the commands the runs ran.
fn headers(window: &aether_protocol::viewport::Window) -> Vec<String> {
    chrome_nodes(window)
        .iter()
        .map(|n| chrome_text(n))
        .collect()
}

/// The name on every box in the view, in order — where each run ran and how it went.
///
/// Titles ride the boxes' top borders rather than standing in rows of their own, so they are read
/// off the containers rather than out of [`headers`].
fn titles(window: &aether_protocol::viewport::Window) -> Vec<String> {
    box_titles(window)
}

// ---- opening ------------------------------------------------------------------------------------

/// A new shell is a real state: one element, the input, and the cursor already in it.
#[tokio::test]
async fn opening_a_shell_lands_in_its_input() {
    let (server, mut ws, _dir) = setup().await;
    let open = open_shell(&mut ws, false).await;
    assert_eq!(open.opened.title.as_deref(), Some("Shell 1"));
    assert!(open.opened.read_only, "the transcript is not editable");
    assert!(!open.opened.is_patch, "and it is not a patch either");

    let (_, window) = shell_window(&mut ws, &open).await;
    let editors = window.root.editors();
    assert_eq!(editors.len(), 1, "a fresh shell is just its input");
    let input_element = window
        .root
        .input_element()
        .expect("the window says which element is the input");
    assert_eq!(input_element, open.input);
    // The scroll names it, which is what makes a subscribe focus it.
    assert_eq!(open.opened.scroll.map(|s| s.element), Some(open.input));

    // And the element really windows an editable document: this is what you type into.
    let input = input_buffer_of(&server, &open).await;
    assert_ne!(input, open.opened.buffer_id);
    type_command(&mut ws, &open, input, "echo hi").await;
    assert_eq!(input_text(&mut ws, input).await, "echo hi");
}

/// `new: false` hands back the idle shell you already have; `new: true` mints the next one.
#[tokio::test]
async fn a_second_shell_is_asked_for_explicitly() {
    let (_server, mut ws, _dir) = setup().await;
    let first = open_shell(&mut ws, false).await;
    let again = open_shell(&mut ws, false).await;
    assert_eq!(
        again.opened.view_id, first.opened.view_id,
        "an idle shell is the one you meant"
    );

    let second = open_shell(&mut ws, true).await;
    assert_ne!(second.opened.view_id, first.opened.view_id);
    assert_eq!(second.opened.title.as_deref(), Some("Shell 2"));
}

/// A shell that is busy is not one you can type into, so `Space b` finds another.
#[tokio::test]
async fn a_busy_shell_is_not_reused() {
    let (server, mut ws, _dir) = setup().await;
    let busy = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &busy).await;
    type_command(&mut ws, &busy, input, "sleep 100").await;
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: busy.opened.view_id,
        },
    )
    .await;

    let next = open_shell(&mut ws, false).await;
    assert_ne!(
        next.opened.view_id, busy.opened.view_id,
        "a shell running a build is not one you can type into"
    );
    assert_eq!(next.opened.title.as_deref(), Some("Shell 2"));

    let _: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: busy.opened.view_id,
        },
    )
    .await;
}

/// Commands run in the root of the project the file you were looking at belongs to.
#[tokio::test]
async fn a_shell_runs_in_the_project_root() {
    let (server, mut ws, dir) = setup().await;
    // Look at a file first, so there is a project to resolve from.
    let open: ViewOpenResult =
        send_request::<ViewOpen>(&mut ws, &file_open_params("a.txt", None)).await;
    let _: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &transient_sub_params(open.buffer_id)).await;

    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "pwd").await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });

    let (_, window) = shell_window(&mut ws, &shell).await;
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(
        body(&window).first().map(String::as_str),
        Some(root.to_string_lossy().as_ref()),
        "the run's working directory is the project root"
    );
    // And the run's box says so, so you can read it without running `pwd`: the directory names the
    // box, and the command is the row inside it.
    let titles = titles(&window);
    let name = root.file_name().unwrap().to_str().unwrap();
    assert!(
        titles.iter().any(|t| t.contains(name)),
        "a box is named for the directory it ran in: {titles:?}"
    );
    let headers = headers(&window);
    // The command, then the blank row of ground before the input's box.
    assert_eq!(
        headers.iter().map(|h| h.trim()).collect::<Vec<_>>(),
        vec!["pwd", ""],
        "and the command is the only chrome in the view: {headers:?}"
    );
}

// ---- running ------------------------------------------------------------------------------------

/// The command is read from the input and the input is cleared — a submit is a submit, not a copy.
#[tokio::test]
async fn a_run_reads_and_clears_the_input() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "  echo hello  ").await;

    let started: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert_eq!(started.run, Some(1));
    assert_eq!(
        input_text(&mut ws, input).await,
        "",
        "the input is cleared, not merely read"
    );
    let run = finished_run(&mut ws).await;
    assert_eq!(run.command, "echo hello", "trimmed, as submitted");
    assert_eq!(run.status, RunStatus::Exited { code: 0 });

    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window), vec!["hello".to_string(), String::new()]);
}

/// An empty input is not a command. Refused before anything is appended, so pressing `Enter` on a
/// blank line does nothing at all.
#[tokio::test]
async fn an_empty_input_is_refused() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let err = send_request_expect_err::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(err.contains("nothing to run"), "{err}");

    // Whitespace is no better.
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "   ").await;
    let err = send_request_expect_err::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(err.contains("nothing to run"), "{err}");
    assert_eq!(
        input_text(&mut ws, input).await,
        "   ",
        "and a refusal leaves what you typed alone"
    );
}

/// One run at a time, and typing ahead of it costs nothing: the refusal names the command in the
/// way and leaves the text where it is.
#[tokio::test]
async fn typing_ahead_survives_a_refused_submit() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "sleep 100").await;
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;

    type_command(&mut ws, &shell, input, "echo next").await;
    let err = send_request_expect_err::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(err.contains("Shell 1 is running sleep 100"), "{err}");
    assert!(
        err.contains("Space Alt-b"),
        "and says how to stop it: {err}"
    );
    assert_eq!(
        input_text(&mut ws, input).await,
        "echo next",
        "the text you typed ahead is still there"
    );

    let cancelled: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(cancelled.cancelled);
}

/// The exit status names the run's box, where it is read without running anything else.
#[tokio::test]
async fn an_exit_code_lands_on_the_runs_box() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "sh -c \"exit 3\"").await;
    assert_eq!(run.status, RunStatus::Exited { code: 3 });

    let (_, window) = shell_window(&mut ws, &shell).await;
    let (titles, headers) = (titles(&window), headers(&window));
    // The run's box is named with the outcome, and the command row says the same thing — the
    // command *was* `exit 3`.
    assert!(
        titles.iter().any(|t| t.contains("exit 3")),
        "the outcome names the box: {titles:?}"
    );
    assert!(
        headers.iter().any(|h| h.trim() == "sh -c \"exit 3\""),
        "and the command row is what ran: {headers:?}"
    );
    assert!(
        !titles
            .iter()
            .chain(&headers)
            .any(|h| h.contains("Space Alt-b")),
        "a finished run offers nothing to stop: {titles:?} {headers:?}"
    );
}

/// Output arrives while it is being produced, not in one lump at the end.
#[tokio::test]
async fn output_arrives_in_more_than_one_push() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let (viewport_id, _) = shell_window(&mut ws, &shell).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(
        &mut ws,
        &shell,
        input,
        "printf \"one\\n\"; sleep 0.4; printf \"two\\n\"; sleep 0.4; printf \"three\\n\"",
    )
    .await;
    ws.clear_seen();
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    let run = finished_run(&mut ws).await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });

    let content: Vec<_> = ws
        .saw::<ViewportLinesChanged>()
        .into_iter()
        .filter(|p| p.viewport_id == viewport_id)
        .collect();
    assert!(
        content.len() >= 2,
        "output should stream, not arrive in one lump (saw {} pushes)",
        content.len()
    );
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            String::new()
        ]
    );
}

/// A progress bar rewrites its line rather than spamming one per frame — the reason `cargo build`
/// is readable in a transcript at all.
#[tokio::test]
async fn a_carriage_return_rewrites_the_line_it_lands_in() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(
        &mut ws,
        &server,
        &shell,
        "sh -c \"printf 'Building 10%%\\\\rBuilding 60%%\\\\rBuilding 100%%\\\\ndone\\\\n'\"",
    )
    .await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec![
            "Building 100%".to_string(),
            "done".to_string(),
            String::new()
        ]
    );
}

/// Colour is stripped: this is a transcript, not a terminal, and a stray escape in a buffer is
/// unreadable rather than merely unstyled.
#[tokio::test]
async fn ansi_escapes_never_reach_the_transcript() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(
        &mut ws,
        &server,
        &shell,
        "printf \"\\\\033[31mred\\\\033[0m plain\\\\n\"",
    )
    .await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window).first().map(String::as_str), Some("red plain"));
}

/// stdout and stderr are one stream, in the order the command produced them.
#[tokio::test]
async fn both_streams_land_in_the_transcript() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(
        &mut ws,
        &server,
        &shell,
        "sh -c \"printf 'out\\\\n'; sleep 0.3; printf 'err\\\\n' >&2\"",
    )
    .await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec!["out".to_string(), "err".to_string(), String::new()]
    );
}

/// A run that says nothing owns no line: its box is its title and its command, with no output
/// row — and `Tab` steps over it, since there is nothing in it to land on.
#[tokio::test]
async fn a_silent_run_shows_no_output() {
    use aether_protocol::ui::Element;
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "true").await;
    run_and_wait(&mut ws, &server, &shell, "echo hi").await;
    let (viewport_id, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window), vec!["hi".to_string(), String::new()]);
    let editors = window.root.editors();
    assert_eq!(editors.len(), 3, "both runs and the input are elements");
    let Element::Editor { rows, lines, .. } = editors[0] else {
        panic!()
    };
    assert_eq!((*rows, lines.len()), (0, 0), "the silent run draws no row");
    assert!(
        headers(&window).iter().any(|h| h.trim() == "true"),
        "but its command is there: {:?}",
        headers(&window)
    );

    // From the input, stepping back lands on `echo hi` and then stays there: the silent run has
    // no line to stop on.
    use aether_protocol::viewport::{FocusStep, FocusTarget};
    let r = focus(&mut ws, viewport_id, FocusTarget::Element { element: 2 }).await;
    assert_eq!(r.element, 2, "start in the input");
    for expected in [1, 1] {
        let r = focus(
            &mut ws,
            viewport_id,
            FocusTarget::Step {
                direction: FocusStep::Previous,
            },
        )
        .await;
        assert_eq!(r.element, expected);
    }
}

// ---- several runs -------------------------------------------------------------------------------

/// A second run adds an element below the first and leaves the first exactly where it was.
#[tokio::test]
async fn a_second_run_appends_without_moving_the_first() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "echo first").await;
    let before = {
        let s = server.state.lock().await;
        let t = s
            .try_doc_of(shell.opened.buffer_id)
            .and_then(|d| d.transcript())
            .expect("a shell");
        (t.runs[0].start_line, t.runs[0].end_line_exclusive)
    };

    run_and_wait(&mut ws, &server, &shell, "echo second").await;
    let (after, second) = {
        let s = server.state.lock().await;
        let t = s
            .try_doc_of(shell.opened.buffer_id)
            .and_then(|d| d.transcript())
            .expect("a shell");
        (
            (t.runs[0].start_line, t.runs[0].end_line_exclusive),
            (t.runs[1].start_line, t.runs[1].end_line_exclusive),
        )
    };
    assert_eq!(before, after, "the first run's extent is final");
    assert_eq!(second, (1, 2));

    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(window.root.editors().len(), 3, "two runs and the input");
    assert_eq!(
        body(&window),
        vec!["first".to_string(), "second".to_string(), String::new()]
    );
    let headers = headers(&window);
    assert!(
        headers.iter().any(|h| h.trim() == "echo first"),
        "{headers:?}"
    );
    assert!(
        headers.iter().any(|h| h.trim() == "echo second"),
        "{headers:?}"
    );
}

/// Every run sits in a box of its own — named for where it ran and how it went, holding the
/// command it ran over its output — and so does the input. Every box closes itself: separate
/// boxes, not one ruled list, because each one carries its own name.
#[tokio::test]
async fn every_run_and_the_input_sit_in_their_own_box() {
    use aether_protocol::ui::Element;
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "echo first").await;
    run_and_wait(&mut ws, &server, &shell, "echo second").await;

    let (_, window) = shell_window(&mut ws, &shell).await;
    let Element::Column { children, .. } = &window.root else {
        panic!("a composed view is a column: {:?}", window.root);
    };
    let mut gaps = 0;
    let boxes: Vec<_> = children
        .iter()
        .filter_map(|c| match c {
            Element::Column {
                edges,
                title,
                children,
                ..
            } if edges.border.left == 1 => Some((edges, title, children)),
            // A blank row of ground between boxes.
            Element::Row { children, .. } if children.is_empty() => {
                gaps += 1;
                None
            }
            other => panic!("not a box or a gap: {other:?}"),
        })
        .collect();
    assert_eq!(boxes.len(), 3, "two runs and the input");
    assert_eq!(
        gaps, 2,
        "a blank row between each pair of boxes, and before none"
    );
    for (edges, title, _) in &boxes {
        assert_eq!(
            (
                edges.border.top,
                edges.border.right,
                edges.border.bottom,
                edges.border.left
            ),
            (1, 1, 1, 1),
            "every box closes itself"
        );
        assert!(!edges.collapse, "and shares no edge with the next");
        assert!(!title.is_empty(), "and is named");
    }
    // The name is the box's, not a row of its own: what a run's box *holds* is the command and the
    // output, and the input's holds only the line you type into.
    let texts = |kids: &[Element]| -> Vec<String> {
        kids.iter()
            .filter_map(|k| match k {
                Element::Row { .. } => Some(k.text_content()),
                Element::Editor { .. } => Some("<editor>".into()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(texts(boxes[0].2), vec!["echo first", "<editor>"]);
    assert_eq!(texts(boxes[1].2), vec!["echo second", "<editor>"]);
    assert_eq!(
        texts(boxes[2].2),
        vec!["<editor>"],
        "the input's box holds the line you type and nothing above it"
    );
    // And the names, in order: each run's directory and outcome, then the input's directory alone.
    let titles = titles(&window);
    assert_eq!(titles.len(), 3, "{titles:?}");
    assert!(
        titles[0].contains("ok") || titles[0].contains("exit"),
        "{titles:?}"
    );
    let directory = titles[2].clone();
    for title in &titles {
        assert!(
            title.starts_with(&directory),
            "every box opens with the same directory: {titles:?}"
        );
    }
}

// ---- the language: refusal at Enter ----------------------------------------------------------------

/// The client's selection on the input, as `(anchor, cursor)`.
async fn input_selection(
    server: &aether_server::ServerHandle,
    input: u64,
) -> (
    aether_protocol::LogicalPosition,
    aether_protocol::LogicalPosition,
) {
    let s = server.state.lock().await;
    let c = s
        .cursors
        .iter()
        .find(|((_, b), _)| *b == input)
        .map(|(_, c)| c)
        .expect("a cursor on the input");
    (c.anchor, c.position)
}

/// Submit what is in the input and expect a refusal: the error object the server answered.
async fn refused(ws: &mut Ws, shell: &ShellOpenResult) -> serde_json::Value {
    send_request_expect_error::<ShellRun>(
        ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await
}

fn pos(line: u32, col: u32) -> aether_protocol::LogicalPosition {
    aether_protocol::LogicalPosition { line, col }
}

/// A line naming a command that is nowhere is refused before anything runs: its own code, the
/// word at fault selected so typing replaces it, the text otherwise untouched, and no run made.
#[tokio::test]
async fn an_unknown_command_is_refused_and_selected() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "lss -la").await;

    let err = refused(&mut ws, &shell).await;
    assert_eq!(
        err["code"],
        serde_json::json!(aether_protocol::error::ErrorCode::SHELL_REJECTED.code())
    );
    assert_eq!(err["message"], "unknown command `lss`");
    assert_eq!(input_text(&mut ws, input).await, "lss -la");
    assert_eq!(
        input_selection(&server, input).await,
        (pos(0, 0), pos(0, 2)),
        "the word, inclusive at both ends"
    );
    let runs = {
        let s = server.state.lock().await;
        s.try_doc_of(shell.opened.buffer_id)
            .and_then(|d| d.transcript())
            .map(|t| t.runs.len())
    };
    assert_eq!(runs, Some(0), "nothing was recorded");
}

/// Every kind of refusal, end to end, each pointing at its word.
#[tokio::test]
async fn each_kind_of_refusal_names_its_word() {
    let (server, mut ws, dir) = setup().await;
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let cases: &[(&str, &str, (u32, u32))] = &[
        ("printf $NOPE", "`$NOPE` is not set", (7, 12)),
        ("ls *.zzz", "nothing matches `*.zzz`", (3, 8)),
        ("printf \"\\d\"", "unknown escape `\\d`", (8, 10)),
        ("printf \"abc", "unterminated quote", (7, 11)),
        ("sort < nope.txt", "no such file `nope.txt`", (7, 15)),
        (
            "./sub ls",
            "`./sub` is a directory — a directory change takes no arguments",
            (0, 5),
        ),
        ("./nowhere", "no such file or directory `./nowhere`", (0, 9)),
        ("sleep 1 &", "background jobs aren't supported", (8, 9)),
        ("!ls", "unknown command `!ls`", (0, 3)),
        (
            "FOO = bar",
            "unknown command `FOO` — to set a variable, write `FOO=…`",
            (0, 3),
        ),
    ];
    for (line, message, (start, end)) in cases {
        // A fresh shell per case, so each starts from an empty input in the same directory.
        let shell = open_shell(&mut ws, true).await;
        let input = input_buffer_of(&server, &shell).await;
        type_command(&mut ws, &shell, input, line).await;
        let err = refused(&mut ws, &shell).await;
        assert_eq!(err["message"], *message, "{line}");
        assert_eq!(input_text(&mut ws, input).await, *line, "{line}: text kept");
        assert_eq!(
            input_selection(&server, input).await,
            (pos(0, *start), pos(0, *end - 1)),
            "{line}: selection"
        );
    }
}

/// Submit what is in the input after typing `line`, and answer what the server said it became.
async fn submit(
    ws: &mut Ws,
    server: &aether_server::ServerHandle,
    shell: &ShellOpenResult,
    line: &str,
) -> ShellRunResult {
    let input = input_buffer_of(server, shell).await;
    type_command(ws, shell, input, line).await;
    send_request::<ShellRun>(
        ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await
}

/// A lone path-shaped word naming a directory moves the shell there, silently: no box, just the
/// directory in the input's title — and `..` and `-` move it back. Runs keep their own directory
/// in their titles after the shell has moved on.
#[tokio::test]
async fn a_path_shaped_word_changes_directory() {
    let (server, mut ws, dir) = setup().await;
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    let sub = root.join("sub").to_string_lossy().into_owned();
    let shell = open_shell(&mut ws, false).await;

    let moved = submit(&mut ws, &server, &shell, "./sub").await;
    assert_eq!(moved.run, None, "nothing ran");
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(window.root.editors().len(), 1, "no box was added");
    assert_eq!(
        titles(&window),
        vec![sub.clone()],
        "the input's title moved"
    );
    let input = input_buffer_of(&server, &shell).await;
    assert_eq!(
        input_text(&mut ws, input).await,
        "",
        "and the line was taken"
    );

    run_and_wait(&mut ws, &server, &shell, "pwd").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window)[0], sub, "commands run there now");

    submit(&mut ws, &server, &shell, "..").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    let names = titles(&window);
    assert!(
        names[0].starts_with(&sub),
        "the run still says where it happened: {names:?}"
    );
    assert_eq!(names[1], root.to_string_lossy(), "`..` went up");

    submit(&mut ws, &server, &shell, "-").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(titles(&window)[1], sub, "`-` came back");
}

/// `NAME=value` alone sets the shell's environment for every run after it; with a command it does
/// not persist. Read back through the user's shell, which sees the transcript's environment.
#[tokio::test]
async fn an_assignment_persists_across_runs() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;

    let run = run_and_wait(&mut ws, &server, &shell, "GREETING=hello").await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });
    run_and_wait(&mut ws, &server, &shell, "sh -c \"echo \\$GREETING\"").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window)[0],
        "hello",
        "the assignment's box has no output row"
    );

    run_and_wait(&mut ws, &server, &shell, "ONCE=1 true").await;
    let err = {
        let input = input_buffer_of(&server, &shell).await;
        type_command(&mut ws, &shell, input, "printf $ONCE").await;
        refused(&mut ws, &shell).await
    };
    assert_eq!(err["message"], "`$ONCE` is not set");
}

/// The user's real shell is a command like any other: `sh -c "…"`, in the shell's directory and
/// environment, with `\$` for a variable meant for it rather than for us.
#[tokio::test]
async fn the_users_shell_is_just_a_command() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "sh -c \"exit 1\"").await;
    assert_eq!(run.status, RunStatus::Exited { code: 1 });
    run_and_wait(&mut ws, &server, &shell, "sh -c \"echo hi | tr a-z A-Z\"").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window)[0], "HI", "the silent `exit 1` owns no line");
    let headers = headers(&window);
    assert!(
        headers.iter().any(|h| h.trim() == "sh -c \"exit 1\""),
        "the command row shows what was typed: {headers:?}"
    );
}

// ---- the executor -------------------------------------------------------------------------------

/// A pipeline's stages are wired stdout to stdin, and its status is its first failing stage's —
/// `false | cat` is not a success.
#[tokio::test]
async fn a_pipeline_runs_and_reports_its_first_failing_stage() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "printf \"b\\na\\n\" | sort").await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window)[..2], ["a".to_string(), "b".to_string()]);

    let run = run_and_wait(&mut ws, &server, &shell, "false | cat").await;
    assert_eq!(run.status, RunStatus::Exited { code: 1 });
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert!(
        titles(&window)[1].contains("exit 1"),
        "{:?}",
        titles(&window)
    );
}

/// `&&` runs the next only on success, `||` only on failure, `;` regardless; the line's status is
/// the last pipeline that ran.
#[tokio::test]
async fn lists_short_circuit() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "false && echo no").await;
    assert_eq!(run.status, RunStatus::Exited { code: 1 });
    let run = run_and_wait(&mut ws, &server, &shell, "false || echo fallback").await;
    assert_eq!(run.status, RunStatus::Exited { code: 0 });
    run_and_wait(&mut ws, &server, &shell, "false; echo after").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec!["fallback".to_string(), "after".to_string(), String::new()]
    );
}

/// `>` truncates, `>>` appends, `<` feeds — all relative to the shell's directory.
#[tokio::test]
async fn redirections_write_and_read_files() {
    let (server, mut ws, dir) = setup().await;
    std::fs::write(dir.path().join("in.txt"), "b\na\n").unwrap();
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "echo hi > out.txt").await;
    run_and_wait(&mut ws, &server, &shell, "echo more >> out.txt").await;
    run_and_wait(&mut ws, &server, &shell, "cat out.txt").await;
    run_and_wait(&mut ws, &server, &shell, "sort < in.txt").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec![
            "hi".to_string(),
            "more".to_string(),
            "a".to_string(),
            "b".to_string(),
            String::new()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "hi\nmore\n"
    );
}

/// `NAME=value command` is that command's environment and nobody else's.
#[tokio::test]
async fn a_prefix_assignment_is_for_that_command_only() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "FOO=bar sh -c \"echo \\$FOO\"").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window)[0], "bar");
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "printf $FOO").await;
    assert_eq!(
        refused(&mut ws, &shell).await["message"],
        "`$FOO` is not set"
    );
}

/// `%` is the file most recently looked at in the workspace — a real file, never the shell.
#[tokio::test]
async fn the_current_file_word_names_the_file_being_looked_at() {
    let (server, mut ws, dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "cat %").await;
    assert_eq!(
        refused(&mut ws, &shell).await["message"],
        "`%` has no file to point at"
    );

    let file = dir.path().canonicalize().unwrap().join("a.txt");
    let _: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            absolute_path: Some(file.to_string_lossy().into_owned()),
            ..Default::default()
        },
    )
    .await;
    // Back to the shell, which is now the most recent view — and not a file.
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "cat %").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(body(&window)[0], "hello");
}

/// `**` walks directories; the matches arrive as separate arguments, in order.
#[tokio::test]
async fn globs_expand_recursively() {
    let (server, mut ws, dir) = setup().await;
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(dir.path().join("src/x.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/deep/y.rs"), "").unwrap();
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "printf \"%s\\n\" **/*.rs").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window)[..2],
        ["src/deep/y.rs".to_string(), "src/x.rs".to_string()]
    );
}

/// `pwd` and `type` are the shell's own.
#[tokio::test]
async fn pwd_and_type_are_builtins() {
    let (server, mut ws, dir) = setup().await;
    let root = dir.path().canonicalize().unwrap();
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "pwd").await;
    run_and_wait(&mut ws, &server, &shell, "type ls pwd").await;
    let run = run_and_wait(&mut ws, &server, &shell, "type nope").await;
    assert_eq!(run.status, RunStatus::Exited { code: 1 });
    let (_, window) = shell_window(&mut ws, &shell).await;
    let lines = body(&window);
    assert_eq!(lines[0], root.to_string_lossy());
    assert!(lines[1].starts_with("ls is /"), "{lines:?}");
    assert_eq!(lines[2], "pwd is a shell builtin");
    assert_eq!(lines[3], "nope: not found");
}

/// A directory that has gone since the shell moved there is reported as a failed run, not as a
/// spawn error nobody sees.
#[tokio::test]
async fn a_vanished_directory_is_reported() {
    let (server, mut ws, dir) = setup().await;
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let shell = open_shell(&mut ws, false).await;
    submit(&mut ws, &server, &shell, "./sub").await;
    std::fs::remove_dir(dir.path().join("sub")).unwrap();
    let run = run_and_wait(&mut ws, &server, &shell, "pwd").await;
    assert_eq!(run.status, RunStatus::Exited { code: 1 });
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert!(
        body(&window)[0].starts_with("working directory no longer exists"),
        "{:?}",
        body(&window)
    );
}

/// A cancel reaches every stage of a pipeline, not only the last.
#[tokio::test]
async fn cancelling_kills_every_stage_of_a_pipeline() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    submit(&mut ws, &server, &shell, "sleep 100 | cat").await;
    let cancelled: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(cancelled.cancelled);
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), finished_run(&mut ws))
        .await
        .expect("the cancel must not wait the sleep out");
    assert_eq!(run.status, RunStatus::Killed);
}

// ---- persistence --------------------------------------------------------------------------------

/// Poll until the shell snapshot for `number` in workspace `p` mentions `needle`.
async fn wait_for_snapshot(backups: &std::path::Path, number: u32, needle: &str) -> String {
    let path = backups.join("shell").join("p").join(number.to_string());
    for _ in 0..200 {
        if let Ok(json) = std::fs::read_to_string(&path) {
            if json.contains(needle) {
                return json;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no snapshot mentioning {needle:?} at {}", path.display());
}

/// Activate the workspace named `p`, as the restart test's two servers both must.
async fn activate_p(ws: &mut Ws) {
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "p".into(),
            open_last: false,
        },
    )
    .await;
}

/// A shell survives a restart: its transcript and runs, where it was, what it assigned, and what
/// was being typed come back from its snapshot; a run that was going comes back killed; a new
/// shell numbers after it; and closing it discards the snapshot.
#[tokio::test]
async fn a_shell_survives_a_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    let sessions_path = root.join("sessions.json");
    let backups = root.join("backups");

    {
        let server = aether_server::spawn_for_test_multi_with_persistence(
            vec![("p".to_string(), vec![root.clone()])],
            Some(sessions_path.clone()),
            Some(backups.clone()),
        )
        .await
        .unwrap();
        let mut ws = Ws::connect(&server).await;
        activate_p(&mut ws).await;
        let shell = open_shell(&mut ws, false).await;
        run_and_wait(&mut ws, &server, &shell, "echo one").await;
        run_and_wait(&mut ws, &server, &shell, "GREETING=hello").await;
        submit(&mut ws, &server, &shell, "./sub").await;
        submit(&mut ws, &server, &shell, "sleep 100").await;
        let input = input_buffer_of(&server, &shell).await;
        type_command(&mut ws, &shell, input, "typed ahead").await;
        wait_for_snapshot(&backups, 1, "typed ahead").await;
        drop(ws);
        drop(server);
    }

    // Second life: the workspace cold-loads from its on-disk config (a pre-registered workspace
    // takes the hot path, which restores nothing), so the session's shell entry comes back as a
    // dormant row over its snapshot.
    let store = root.join("workspaces");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("p.toml"),
        format!("[[roots]]\npath = {:?}\n", root.display().to_string()),
    )
    .unwrap();
    let server = aether_server::spawn_for_test_multi_with_persistence(
        vec![],
        Some(sessions_path.clone()),
        Some(backups.clone()),
    )
    .await
    .unwrap();
    server.state.lock().await.workspaces_dir = Some(store);
    let mut ws = Ws::connect(&server).await;
    activate_p(&mut ws).await;

    // Dormant until opened, and holding its number: a fresh shell is "Shell 2".
    let dormant_view = {
        let s = server.state.lock().await;
        let entry = s.workspaces.get("p").expect("the workspace");
        entry
            .dormant_views
            .iter()
            .find(|d| format!("{:?}", d.source) == "Shell { number: 1 }")
            .map(|d| d.view)
            .unwrap_or_else(|| {
                panic!(
                    "Shell 1 is dormant: session={:?} dormant={:?} snapshots={:?}",
                    std::fs::read_to_string(&sessions_path),
                    entry
                        .dormant_views
                        .iter()
                        .map(|d| format!("{:?}", d.source))
                        .collect::<Vec<_>>(),
                    std::fs::read_dir(backups.join("shell").join("p"))
                        .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
                )
            })
    };
    let fresh = open_shell(&mut ws, false).await;
    assert_eq!(fresh.opened.title.as_deref(), Some("Shell 2"));

    let restored: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            view_id: Some(dormant_view),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(restored.title.as_deref(), Some("Shell 1"));
    let shell = ShellOpenResult {
        opened: restored,
        input: 0,
    };
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        body(&window),
        vec!["one".to_string(), "typed ahead".to_string()],
        "the transcript, then what was being typed"
    );
    let names = titles(&window);
    assert!(names[0].contains("ok"), "{names:?}");
    assert!(
        names[2].contains("killed"),
        "the run that was going: {names:?}"
    );
    assert!(
        names.last().unwrap().ends_with("sub"),
        "where it was: {names:?}"
    );

    // The assignment came back, over a freshly resolved environment.
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "").await;
    let _ = input;
    let state_env = {
        let s = server.state.lock().await;
        s.try_doc_of(shell.opened.buffer_id)
            .and_then(|d| d.transcript())
            .map(|t| (t.env.get("GREETING").cloned(), t.env.contains_key("PATH")))
            .expect("a shell")
    };
    assert_eq!(state_env, (Some("hello".to_string()), true));

    // Closing it is a discard: the snapshot goes with it.
    let _: aether_protocol::view::ViewCloseResult = send_request::<ViewClose>(
        &mut ws,
        &ViewCloseParams {
            view_id: shell.opened.view_id,
            open_next: false,
        },
    )
    .await;
    assert!(
        !backups.join("shell").join("p").join("1").exists(),
        "closing discards the snapshot"
    );
}

/// The input is this shell's own command line, not bash: it is not highlighted as such.
#[tokio::test]
async fn the_input_is_not_highlighted_as_bash() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    let s = server.state.lock().await;
    assert_eq!(s.doc_of(input).language, None);
}

/// The caret stays in the input across a run. Runs are appended *above* the input, so its element
/// number goes up by one with every command — and that number is what a viewport's focus is.
/// Without the follow, each command would hand the caret to its own output as it ran.
#[tokio::test]
async fn the_caret_stays_in_the_input_across_a_run() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let (viewport_id, window) = shell_window(&mut ws, &shell).await;
    assert_eq!(
        window.root.input_element(),
        Some(0),
        "a fresh shell is only its input"
    );
    assert_eq!(focused_element(&server, viewport_id).await, 0);

    run_and_wait(&mut ws, &server, &shell, "echo first").await;
    assert_eq!(
        focused_element(&server, viewport_id).await,
        1,
        "the run took element 0; the input is element 1 now, and the caret is in it"
    );

    // A caret parked on a run's output is not dragged along: focus follows the input only when
    // that is where it was.
    let _ = send_request::<aether_protocol::viewport::ViewportFocusElement>(
        &mut ws,
        &aether_protocol::viewport::ViewportFocusElementParams {
            viewport_id,
            target: aether_protocol::viewport::FocusTarget::Element { element: 0 },
        },
    )
    .await;
    run_and_wait(&mut ws, &server, &shell, "echo second").await;
    assert_eq!(focused_element(&server, viewport_id).await, 0);
}

/// `c` and `o` step between runs and never stop on the input: an outline lists what a view
/// contains, and the input contains nothing yet.
#[tokio::test]
async fn changes_and_outline_step_the_runs_not_the_input() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "echo first").await;
    run_and_wait(&mut ws, &server, &shell, "echo second").await;
    let (viewport_id, _) = shell_window(&mut ws, &shell).await;

    // `Space o` — the outline is one row per run, labelled by the command.
    let outline: aether_protocol::picker::PickerViewResult = send_request::<PickerView>(
        &mut ws,
        &PickerViewParams {
            kind: PickerKind::DocumentSymbols,
            buffer_id: Some(shell.opened.buffer_id),
            view_id: Some(shell.opened.view_id),
            ..view_params(PickerKind::DocumentSymbols)
        },
    )
    .await;
    let update = outline.update.expect("the outline answers with rows");
    // The rows arrive grouped under the shell, collapsed, like every other outline.
    assert_eq!(
        group_rows(update.items()),
        vec![("Shell 1".to_string(), 2, false)],
        "two runs under the shell, and nothing for the input"
    );
    // Expanded, the rows are the commands.
    let (_, expanded) = expand_file_group(&mut ws, PickerKind::DocumentSymbols, "Shell 1").await;
    let labels: Vec<&str> = expanded
        .items()
        .iter()
        .filter_map(|i| match i {
            PickerItem::GitChange { preview, .. } => Some(preview.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        vec!["echo first", "echo second"],
        "one row per run, labelled by its command"
    );

    // `c` — stepping changes walks run to run.
    let step = |direction| aether_protocol::viewport::ViewportNavigateChangeParams {
        viewport_id,
        direction,
        count: Some(1),
        grain: aether_protocol::viewport::NavigateGrain::Change,
        extend: false,
    };
    let landed: aether_protocol::viewport::ViewportFocusElementResult =
        send_request::<aether_protocol::viewport::ViewportNavigateChange>(
            &mut ws,
            &step(aether_protocol::viewport::FocusStep::Next),
        )
        .await;
    assert_eq!(landed.element, 1, "from the first run to the second");
    let landed: aether_protocol::viewport::ViewportFocusElementResult =
        send_request::<aether_protocol::viewport::ViewportNavigateChange>(
            &mut ws,
            &step(aether_protocol::viewport::FocusStep::Next),
        )
        .await;
    assert_eq!(
        landed.element, 1,
        "and stops there: the input is not a change to step onto"
    );
}

// ---- stopping -----------------------------------------------------------------------------------

/// Cancelling kills the whole process group, so a build's compiler goes with the shell that
/// started it. The survivor would write a file; the assertion is that it never does.
#[tokio::test]
async fn cancelling_kills_the_run_and_its_group() {
    let (server, mut ws, dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let marker = dir.path().join("survivor");
    let input = input_buffer_of(&server, &shell).await;
    type_command(
        &mut ws,
        &shell,
        input,
        &format!(
            "sh -c \"( sleep 5; : > {} ) & sleep 100\"",
            marker.display()
        ),
    )
    .await;
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;

    let started = std::time::Instant::now();
    let cancelled: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(cancelled.cancelled);
    let run = finished_run(&mut ws).await;
    assert_eq!(run.status, RunStatus::Killed);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(90),
        "the cancel must not have waited the sleep out"
    );

    // A second cancel has nothing to stop, which is success rather than an error.
    let again: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    assert!(!again.cancelled);

    // Well past the grandchild's own sleep: if the group survived, the file is there.
    tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
    assert!(
        !marker.exists(),
        "the grandchild outlived the cancel — the process group was not killed"
    );
}

/// Closing a shell stops what it was running. Nothing else can: the view is the only handle.
#[tokio::test]
async fn closing_a_shell_kills_its_run() {
    let (server, mut ws, dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let marker = dir.path().join("survivor");
    let input = input_buffer_of(&server, &shell).await;
    type_command(
        &mut ws,
        &shell,
        input,
        &format!(
            "sh -c \"( sleep 5; : > {} ) & sleep 100\"",
            marker.display()
        ),
    )
    .await;
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;

    let _: ViewCloseResult = send_request::<ViewClose>(
        &mut ws,
        &ViewCloseParams {
            view_id: shell.opened.view_id,
            open_next: false,
        },
    )
    .await;
    {
        let s = server.state.lock().await;
        assert!(
            !s.buffers.contains_key(&shell.opened.buffer_id),
            "the transcript went with the view"
        );
        assert!(
            !s.buffers.contains_key(&input),
            "and so did the input — nothing else can reach it"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
    assert!(
        !marker.exists(),
        "closing the view must kill the run's whole group"
    );
}

/// A run that will not stop producing output is stopped for it, and the transcript says so rather
/// than simply ending.
#[tokio::test]
async fn a_runaway_run_is_capped() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let run = run_and_wait(&mut ws, &server, &shell, "yes 0123456789abcdef").await;
    assert_eq!(run.status, RunStatus::Truncated);

    let (_, window) = shell_window(&mut ws, &shell).await;
    let lines = body(&window);
    assert!(
        lines.iter().any(|l| l.contains("output truncated")),
        "the transcript says why it stops: {:?}",
        lines.last()
    );
    let s = server.state.lock().await;
    let doc = s.try_doc_of(shell.opened.buffer_id).expect("a shell");
    assert!(
        doc.byte_count() < 32 * 1024 * 1024,
        "the cap bounds what one run can put in a document"
    );
}

// ---- what a shell is excluded from ---------------------------------------------------------------

/// The input is a field of the view, not a document of the user's: it is in no picker, in no
/// session, and in no dirty count — however much is typed into it.
#[tokio::test]
async fn the_input_is_never_listed_never_saved_and_never_dirty() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    type_command(&mut ws, &shell, input, "cargo build").await;

    let picker: aether_protocol::picker::PickerViewResult =
        send_request::<PickerView>(&mut ws, &view_params(PickerKind::Views)).await;
    let update = picker.update.expect("the views picker answers with rows");
    let rows: Vec<&str> = update
        .items()
        .iter()
        .map(|i| match i {
            PickerItem::View { display, .. } => display.as_str(),
            other => panic!("expected a view row, got {other:?}"),
        })
        .collect();
    assert!(
        rows.contains(&"Shell 1"),
        "the shell itself is listed: {rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.starts_with("(scratch")).count(),
        0,
        "and its input is not a scratch row: {rows:?}"
    );
    assert_eq!(
        rows.len(),
        1,
        "one row for the shell, none for its parts: {rows:?}"
    );

    let s = server.state.lock().await;
    assert!(
        !s.has_unprotected_unsaved_buffers(),
        "a half-typed command is not unsaved work"
    );
    assert_eq!(
        format!("{:?}", s.session_views("shell-proj")),
        "[Shell { number: 1 }]",
        "the shell is written to the session file by its number; its input is not"
    );
    let doc = s.try_doc_of(input).expect("the input");
    assert!(!doc.dirty);
    assert_eq!(doc.saved_revision(), doc.revision);
}

/// The view's dirty marker ignores the input, so typing a command does not make a shell look
/// modified.
#[tokio::test]
async fn typing_a_command_does_not_make_the_shell_look_modified() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert!(!window.other_elements_dirty);

    type_command(&mut ws, &shell, input, "cargo build").await;
    let (_, window) = shell_window(&mut ws, &shell).await;
    assert!(
        !window.other_elements_dirty,
        "the input is not an element that can be dirty"
    );
}

/// A running command keeps an auto-started server alive: reaping it would kill the build and lose
/// output nobody has read.
#[tokio::test]
async fn a_running_command_pins_the_server_open() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    let input = input_buffer_of(&server, &shell).await;
    {
        let s = server.state.lock().await;
        assert!(!s.has_running_shell(), "nothing running yet");
    }
    type_command(&mut ws, &shell, input, "sleep 100").await;
    let _: ShellRunResult = send_request::<ShellRun>(
        &mut ws,
        &ShellRunParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    {
        let s = server.state.lock().await;
        assert!(s.has_running_shell());
    }
    let _: ShellCancelResult = send_request::<ShellCancel>(
        &mut ws,
        &ShellCancelParams {
            view_id: shell.opened.view_id,
        },
    )
    .await;
    finished_run(&mut ws).await;
    let s = server.state.lock().await;
    assert!(!s.has_running_shell(), "and stops pinning once it ends");
}

/// `shell/run` and `shell/cancel` on something that is not a shell are refused rather than
/// silently doing nothing to a file.
#[tokio::test]
async fn shell_rpcs_refuse_a_view_that_is_not_a_shell() {
    let (_server, mut ws, _dir) = setup().await;
    let open: ViewOpenResult =
        send_request::<ViewOpen>(&mut ws, &file_open_params("a.txt", None)).await;
    let view_id = view_of(open.buffer_id);
    let err = send_request_expect_err::<ShellRun>(&mut ws, &ShellRunParams { view_id }).await;
    assert!(err.contains("is not a shell"), "{err}");
    let err = send_request_expect_err::<ShellCancel>(&mut ws, &ShellCancelParams { view_id }).await;
    assert!(err.contains("is not a shell"), "{err}");
}

// ---- following a line ----------------------------------------------------------------------------

/// Park the cursor on `line` of the shell's transcript and press `Enter`.
async fn follow(
    ws: &mut Ws,
    server: &aether_server::ServerHandle,
    open: &ShellOpenResult,
    line: u32,
) -> ViewFollowLineResult {
    // The viewport is what says which element the cursor is in; element 0 is the first run.
    let _ = shell_window(ws, open).await;
    set_point_cursor(ws, open.opened.buffer_id, LogicalPosition { line, col: 0 }).await;
    let _ = server;
    send_request::<ViewFollowLine>(
        ws,
        &ViewFollowLineParams {
            view_id: open.opened.view_id,
        },
    )
    .await
}

/// `Enter` on a line of output that names a file opens it, at the place it named — resolved
/// against the directory the command ran in.
#[tokio::test]
async fn enter_follows_a_path_printed_by_a_command() {
    let (server, mut ws, dir) = setup().await;
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {}\nlet x = 1;\nlet y = 2;\n",
    )
    .unwrap();
    let shell = open_shell(&mut ws, false).await;
    // The shapes a compiler prints, one per line.
    run_and_wait(
        &mut ws,
        &server,
        &shell,
        "printf \"error somewhere\\n  --> src/main.rs:3:5\\n\"",
    )
    .await;

    // Line 0 is prose with no path in it: a quiet no-op.
    let followed = follow(&mut ws, &server, &shell, 0).await;
    assert!(
        followed.opened.is_none(),
        "a line with no path leads nowhere"
    );

    // Line 1 names the file, 1-based, and lands 0-based.
    let followed = follow(&mut ws, &server, &shell, 1).await;
    let opened = followed.opened.expect("the file the line named");
    assert_eq!(
        opened.path.as_deref(),
        Some(
            dir.path()
                .canonicalize()
                .unwrap()
                .join("src/main.rs")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(
        opened.cursor.position,
        LogicalPosition { line: 2, col: 4 },
        "1-based on the wire of every compiler, 0-based here"
    );
}

/// A path that does not exist is not a path: the parse is permissive on purpose, and this is what
/// makes that safe.
#[tokio::test]
async fn enter_on_something_that_only_looks_like_a_path_does_nothing() {
    let (server, mut ws, _dir) = setup().await;
    let shell = open_shell(&mut ws, false).await;
    run_and_wait(&mut ws, &server, &shell, "printf \"10:30:45 done\\n\"").await;
    let followed = follow(&mut ws, &server, &shell, 0).await;
    assert!(followed.opened.is_none());
}

/// Total, not partial: the same method on an ordinary file's view answers "nowhere" instead of
/// erroring, so a client can route every `Enter` through it without knowing what it is looking at.
#[tokio::test]
async fn following_a_line_in_an_ordinary_view_answers_nothing() {
    let (_server, mut ws, _dir) = setup().await;
    let open: ViewOpenResult =
        send_request::<ViewOpen>(&mut ws, &file_open_params("a.txt", None)).await;
    let followed: ViewFollowLineResult = send_request::<ViewFollowLine>(
        &mut ws,
        &ViewFollowLineParams {
            view_id: view_of(open.buffer_id),
        },
    )
    .await;
    assert!(followed.opened.is_none());
}
