//! What one window push costs, in the views where it can get big: a long agent conversation with a
//! reply streaming into its end, a shell with many runs and one still printing, and a
//! working-changes view over many files while they change on disk.
//!
//! Measured where the costs are: the server's mutation and push build (render, diff against what
//! the client holds, serialisation) timed under the state lock; the wire as the bytes a push takes;
//! the client as the core's own handling of the push (parse, apply or replace). Beside each, what
//! the same push would cost sent whole, which is what every push was before patches. The shells'
//! painting is not here — that is per shell, and each has its own painter tests.
//!
//! Ignored by default: these are measurements to read, not assertions. Run them in release, where
//! the debug-only check that every patch rebuilds its window is compiled out:
//!
//! ```text
//! cargo test --release -p aether-server --test push_cost -- --ignored --nocapture --test-threads=1
//! ```

mod common;
use common::*;

use aether_client::update::Event;
use aether_protocol::agent::{AgentPrompt, AgentPromptParams, AgentStart, AgentStartParams};
use aether_protocol::envelope::Notification;
use aether_protocol::shell::{
    ShellCancel, ShellCancelParams, ShellRun, ShellRunChanged, ShellRunParams, ShellStart,
    ShellStartParams,
};
use aether_protocol::viewport::{ViewportElementsChanged, Window};
use aether_server::agent::dummy::{self, Script, Step};
use aether_server::bench::{collect_lines_changed_pushes, render_window, sent_whole, SneakLabels};
use agent_client_protocol::schema::v1::{ToolCallStatus, ToolKind};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- measuring ---------------------------------------------------------------------------------

/// The running totals for one scenario, reported per push.
#[derive(Default)]
struct Costs {
    pushes: u32,
    patches: u32,
    /// The change itself, before any push: the append and the layout rebuild it carries.
    mutate: Duration,
    /// Building the push: render, diff against the client's window, and the JSON value.
    build: Duration,
    /// The render alone, timed apart from the push that also contains one.
    render: Duration,
    /// Serialising the push to the text that goes over the socket.
    serialize: Duration,
    sent_bytes: usize,
    /// The same push sent whole, as every push was before patches.
    whole_bytes: usize,
    whole_serialize: Duration,
    /// The client core handling the push: parse the frame, then apply or replace.
    client: Duration,
    client_whole: Duration,
}

impl Costs {
    fn report(&self, label: &str, extra: &str) {
        let n = self.pushes.max(1);
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n as f64;
        let kb = |b: usize| b as f64 / 1024.0 / n as f64;
        // A scenario whose pushes went out over the real socket has no push build to report: its
        // server time is reported beside it instead.
        let server = if self.build.is_zero() {
            format!("server: render {:.2} ms", ms(self.render))
        } else {
            format!(
                "server: mutate {:.2} ms + push {:.2} ms (render {:.2} ms, serialize {:.3} ms)",
                ms(self.mutate),
                ms(self.build),
                ms(self.render),
                ms(self.serialize)
            )
        };
        println!(
            "{label}\n  {server}\
             \n  wire:   {:.1} KB sent — whole would be {:.1} KB, serialize {:.2} ms\
             \n  client: {:.3} ms — whole would be {:.2} ms\
             \n  {}/{} pushes went as patches{extra}\n",
            kb(self.sent_bytes),
            kb(self.whole_bytes),
            ms(self.whole_serialize),
            ms(self.client),
            ms(self.client_whole),
            self.patches,
            self.pushes,
        );
    }
}

/// Two client cores holding `window` as viewport `viewport_id` shows it: one fed the pushes as they
/// are, one fed each push's window whole — the counterfactual.
struct Clients {
    patched: aether_client::session::Session,
    whole: aether_client::session::Session,
}

impl Clients {
    fn holding(viewport_id: u64, window: Window) -> Self {
        let client = |window: Window| {
            let mut s = aether_client::session::Session::placeholder();
            s.view.viewport_id = Some(viewport_id);
            s.view.window = Some(window);
            s
        };
        Self {
            patched: client(window.clone()),
            whole: client(window),
        }
    }
}

/// Time the client core handling `frame`, the text of one notification.
fn client_handles(client: &mut aether_client::session::Session, frame: &str) -> Duration {
    let t = Instant::now();
    let n: Notification = serde_json::from_str(frame).expect("a notification");
    let _ = client.on_event(Event::ServerPush(n));
    t.elapsed()
}

/// Build the push for `viewport_id` after a change to `buffers`, timing every part, and hand it to
/// both clients. Called under the state lock, right after the change.
fn measure_push(
    s: &aether_server::state::ServerState,
    viewport_id: u64,
    buffers: Vec<u64>,
    costs: &mut Costs,
    clients: &mut Clients,
) {
    let t = Instant::now();
    let pushes = collect_lines_changed_pushes(s, buffers);
    costs.build += t.elapsed();
    let push = pushes
        .into_iter()
        .map(|(_, n)| n)
        .find(|n| n.params["viewport_id"] == serde_json::json!(viewport_id))
        .expect("a push for the viewport");
    let t = Instant::now();
    let frame = serde_json::to_string(&push).unwrap();
    costs.serialize += t.elapsed();
    costs.sent_bytes += frame.len();
    if push.method == ViewportElementsChanged::NAME {
        costs.patches += 1;
    }
    costs.pushes += 1;

    let vp = &s.viewports[&viewport_id];
    let t = Instant::now();
    let whole = render_window(s, vp, SneakLabels::Hidden);
    costs.render += t.elapsed();
    let t = Instant::now();
    let whole_frame = serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": aether_protocol::viewport::ViewportLinesChanged::NAME,
        "params": {
            "viewport_id": viewport_id,
            "buffer": 0,
            "revision": 0,
            "window": whole,
        },
    }))
    .unwrap();
    costs.whole_serialize += t.elapsed();
    costs.whole_bytes += whole_frame.len();

    costs.client += client_handles(&mut clients.patched, &frame);
    costs.client_whole += client_handles(&mut clients.whole, &whole_frame);
    assert_eq!(
        clients.patched.view.window.as_ref().map(|w| &w.root),
        Some(&whole.root),
        "the patched client is not showing what the server rendered"
    );
}

/// Start the measured client from the window the server records as sent, so the first patch fits.
fn baseline(s: &aether_server::state::ServerState, viewport_id: u64) -> Window {
    let vp = &s.viewports[&viewport_id];
    sent_whole(vp, render_window(s, vp, SneakLabels::Hidden))
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn subscribe_params(view_id: aether_protocol::ViewId, element: u32) -> ViewportSubscribeParams {
    ViewportSubscribeParams {
        view_id,
        cols: 120,
        rows: 60,
        overscan_rows: 20,
        scroll: ScrollPosition {
            element,
            line: 0,
            sub_row: 0.0,
        },
        focus: None,
        wrap: WrapMode::Soft,
        continuation_marker_width: 2,
        tab_width: 4,
        diff_view: false,
    }
}

async fn activate(server: &aether_server::ServerHandle, name: &str) -> Ws {
    let mut ws = Ws::connect(server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: name.into(),
            open_last: false,
        },
    )
    .await;
    ws
}

// ---- an agent conversation ---------------------------------------------------------------------

/// A reply of the shape an agent writes: a heading, prose, a code block, a list, a quote.
fn reply(i: usize) -> String {
    format!(
        "## Looking at module {i}\n\nThe function `handle_{i}` reads its input twice, which is why \
         the **second pass** sees a stale buffer. Here is the part that matters:\n\n```rust\nfn \
         handle_{i}(input: &str) -> usize {{\n    let first = input.lines().count();\n    let \
         second = input.len();\n    first + second\n}}\n```\n\n- It is called from `dispatch` on \
         every request.\n- The fix is to read once and pass the result down.\n- A test should pin \
         the [behaviour](https://example.com/{i}).\n\n> The same pattern appears in three other \
         modules.\n\n{}\n",
        "Reading further, the caller holds the lock across the whole pass, so the second read \
         cannot race anything; the staleness is the cache, not a writer. "
            .repeat(4)
    )
}

/// What a tool call prints: forty lines of search results.
fn tool_output(i: usize) -> String {
    (0..40)
        .map(|l| format!("src/module_{i}.rs:{l}:    let value_{l} = compute({l}, &input);\n"))
        .collect()
}

async fn agent_streaming(turns: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("bench-agent", vec![root]).await.unwrap();
    server.keep_alive(());

    let mut steps = Vec::new();
    for i in 0..turns {
        let id = leak(format!("t{i}"));
        steps.push(Step::Say {
            message_id: Some(leak(format!("m{i}"))),
            text: leak(reply(i)),
        });
        steps.push(Step::Call {
            id,
            title: leak(format!("Searching module {i}")),
            kind: ToolKind::Search,
        });
        steps.push(Step::Update {
            id,
            status: Some(ToolCallStatus::Completed),
            text: Some(leak(tool_output(i))),
        });
    }
    steps.push(Step::Say {
        message_id: Some("last"),
        text: "## Summary\n\nPutting it together:",
    });
    let started = dummy::start(Script {
        steps,
        ..Script::default()
    });
    let slot = Mutex::new(Some(started.transport));
    server.state.lock().await.agent_launcher =
        aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
            slot.lock().unwrap().take().expect("one conversation")
        }));

    let mut ws = activate(&server, "bench-agent").await;
    let open: aether_protocol::agent::AgentStartResult =
        send_request::<AgentStart>(&mut ws, &AgentStartParams { agent: None }).await;
    let view_id = open.opened.view_id;
    let input = {
        let s = server.state.lock().await;
        s.try_view(view_id)
            .unwrap()
            .elements
            .iter()
            .find(|e| e.role.is_input())
            .unwrap()
            .buffer_id
    };
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        &InputTextParams {
            buffer_id: input,
            text: "go".into(),
            select_pasted: false,
            at: None,
            replace_selection: false,
        },
    )
    .await;
    let _: aether_protocol::agent::AgentPromptResult =
        send_request::<AgentPrompt>(&mut ws, &AgentPromptParams { view_id }).await;
    loop {
        let running = {
            let s = server.state.lock().await;
            s.try_presenting_buffer(view_id)
                .and_then(|b| s.try_doc_of(b))
                .and_then(|d| d.conversation())
                .is_some_and(|c| c.is_running())
        };
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Read at the bottom, where a conversation is read while it streams.
    let last_element = {
        let s = server.state.lock().await;
        s.try_view(view_id).unwrap().elements.len() as u32 - 1
    };
    let sub: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &subscribe_params(view_id, last_element)).await;

    let mut s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(view_id).unwrap();
    let (blocks, reply_buffer, text_bytes) = {
        let c = s.try_doc_of(view_buffer).unwrap().conversation().unwrap();
        let blocks: Vec<u64> = c.blocks.iter().map(|b| b.buffer).collect();
        let reply = c
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(b.kind, aether_server::agent::BlockKind::AgentMessage))
            .unwrap()
            .buffer;
        let bytes: usize = blocks.iter().map(|b| s.doc_of(*b).text.len_bytes()).sum();
        (blocks, reply, bytes)
    };
    // What every push paid before the parse cache: every reply parsed again.
    let t = Instant::now();
    let mut prose = 0;
    for b in &blocks {
        let block = s
            .try_doc_of(view_buffer)
            .unwrap()
            .conversation()
            .unwrap()
            .blocks
            .iter()
            .find(|x| x.buffer == *b)
            .unwrap();
        if aether_server::agent::is_rendered(&block.kind) {
            prose += 1;
            let _ = aether_markdown::parse_marked(&s.doc_of(*b).text.to_string(), &[]);
        }
    }
    let parse_all = t.elapsed();

    let mut clients = Clients::holding(sub.viewport_id, baseline(&s, sub.viewport_id));
    let mut costs = Costs::default();
    for k in 0..50 {
        let t = Instant::now();
        s.extend_block(
            view_buffer,
            reply_buffer,
            &format!(" and then step {k} follows,"),
        );
        costs.mutate += t.elapsed();
        let buffers = std::iter::once(view_buffer)
            .chain(blocks.iter().copied())
            .collect();
        measure_push(&s, sub.viewport_id, buffers, &mut costs, &mut clients);
    }
    let pushes_before = blocks.len() + 1;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    costs.report(
        &format!(
            "agent: {turns} turns, {} blocks, {:.0} KB of text — a reply streaming in",
            blocks.len(),
            text_bytes as f64 / 1024.0
        ),
        &format!(
            "\n  before the cache: + {:.1} ms parsing {prose} replies per render\
             \n  before per-viewport pushes: × {pushes_before} renders and pushes per chunk",
            ms(parse_all)
        ),
    );
}

#[tokio::test]
#[ignore = "a measurement; see the module docs"]
async fn agent_conversation_push_cost() {
    for turns in [50, 200, 500] {
        agent_streaming(turns).await;
    }
}

// ---- a shell -----------------------------------------------------------------------------------

async fn finished_run(ws: &mut Ws) {
    loop {
        let p = expect_notification_or_backlog::<ShellRunChanged>(ws).await;
        if p.run.is_some_and(|r| !r.is_running()) {
            return;
        }
    }
}

async fn submit(ws: &mut Ws, view_id: aether_protocol::ViewId, input: u64, command: &str) {
    let _: EditResult = send_request::<InputText>(
        ws,
        &InputTextParams {
            buffer_id: input,
            text: command.into(),
            select_pasted: false,
            at: None,
            replace_selection: false,
        },
    )
    .await;
    let _: aether_protocol::shell::ShellRunResult =
        send_request::<ShellRun>(ws, &ShellRunParams { view_id }).await;
}

async fn shell_streaming(runs: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("bench-shell", vec![root]).await.unwrap();
    server.keep_alive(());
    let mut ws = activate(&server, "bench-shell").await;
    let open: aether_protocol::shell::ShellStartResult =
        send_request::<ShellStart>(&mut ws, &ShellStartParams::default()).await;
    let view_id = open.opened.view_id;
    let input = {
        let s = server.state.lock().await;
        s.try_view(view_id)
            .unwrap()
            .elements
            .iter()
            .find(|e| e.role.is_input())
            .unwrap()
            .buffer_id
    };
    for _ in 0..runs {
        submit(&mut ws, view_id, input, "seq 1 40").await;
        finished_run(&mut ws).await;
    }
    // One still printing: its output is written in below, where the runner would write it.
    submit(&mut ws, view_id, input, "sleep 60").await;
    loop {
        let busy = {
            let s = server.state.lock().await;
            s.try_presenting_buffer(view_id)
                .and_then(|b| s.try_doc_of(b))
                .and_then(|d| d.transcript())
                .is_some_and(|t| t.is_busy())
        };
        if busy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let input_element = {
        let s = server.state.lock().await;
        s.try_view(view_id).unwrap().elements.len() as u32 - 1
    };
    let sub: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &subscribe_params(view_id, input_element)).await;

    {
        let mut s = server.state.lock().await;
        let transcript = s.try_presenting_buffer(view_id).unwrap();
        let text_bytes = s.doc_of(transcript).text.len_bytes();
        let mut clients = Clients::holding(sub.viewport_id, baseline(&s, sub.viewport_id));
        let mut costs = Costs::default();
        for k in 0..50 {
            let t = Instant::now();
            let end = s.doc_of(transcript).text.len_chars();
            s.extend_transcript(transcript, end, &format!("compiling crate_{k} v0.1.0\n"));
            costs.mutate += t.elapsed();
            measure_push(
                &s,
                sub.viewport_id,
                vec![transcript],
                &mut costs,
                &mut clients,
            );
        }
        costs.report(
            &format!(
                "shell: {runs} runs, {:.0} KB of transcript — a run printing",
                text_bytes as f64 / 1024.0
            ),
            "",
        );
    }
    let _: aether_protocol::shell::ShellCancelResult =
        send_request::<ShellCancel>(&mut ws, &ShellCancelParams { view_id }).await;
}

#[tokio::test]
#[ignore = "a measurement; see the module docs"]
async fn shell_push_cost() {
    for runs in [50, 200, 500] {
        shell_streaming(runs).await;
    }
}

// ---- working changes ---------------------------------------------------------------------------

fn source(i: usize, edits: usize) -> String {
    let mut out = String::new();
    for l in 0..120 {
        if l == 60 {
            for e in 0..edits {
                out.push_str(&format!("    // external edit {e}\n"));
            }
        }
        out.push_str(&format!("fn item_{i}_{l}() -> u32 {{ {l} }}\n"));
    }
    out
}

/// Let every push the last change caused arrive: the watcher reloads the file, and a rebuild
/// follows its debounce.
async fn quiet(ws: &mut Ws, server: &aether_server::ServerHandle) {
    let mut seen = usize::MAX;
    let mut still = 0;
    while still < 4 {
        settled(server).await;
        let _: aether_protocol::app::AppInfo =
            send_request::<aether_protocol::app::AppInfoGet>(ws, &Default::default()).await;
        let now = ws.seen_all().len();
        still = if now == seen { still + 1 } else { 0 };
        seen = now;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn working_changes(files: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = init_repo_at(&root);
    for i in 0..files {
        commit_file(&repo, &format!("src/file_{i}.rs"), &source(i, 0));
    }
    for i in 0..files {
        std::fs::write(root.join(format!("src/file_{i}.rs")), source(i, 1)).unwrap();
    }
    let (server, mut ws) = setup_repos_workspace(vec![root.clone()]).await;
    let open = show_buffer(
        &mut ws,
        &aether_protocol::git::GitShowParams {
            repo_id: Some(root.to_string_lossy().into_owned()),
            buffer_id: None,
            target: aether_protocol::git::ShowTarget::WorkingChanges,
            focus_path: None,
            record_nav_from: None,
        },
    )
    .await;
    let sub: ViewportSubscribeResult =
        send_request::<ViewportSubscribe>(&mut ws, &subscribe_params(open.view_id, 0)).await;
    quiet(&mut ws, &server).await;

    let workdirs: std::collections::HashSet<_> = std::iter::once(root.clone()).collect();
    let mut clients = {
        let s = server.state.lock().await;
        Clients::holding(sub.viewport_id, baseline(&s, sub.viewport_id))
    };
    ws.clear_seen();
    let mut costs = Costs::default();
    let mut rebuild = Duration::ZERO;
    let steps = 10;
    for k in 0..steps {
        // A file a long way down, then one at the top — what an agent editing the tree does.
        let i = if k % 2 == 0 { files - 1 - k } else { k };
        std::fs::write(root.join(format!("src/file_{i}.rs")), source(i, 2 + k)).unwrap();
        let t = Instant::now();
        aether_server::bench::refresh_working_changes_views(&server.state, &workdirs).await;
        rebuild += t.elapsed();
        quiet(&mut ws, &server).await;
    }

    // Every push the steps caused — the watcher's reloads and the rebuilds — as the client got them.
    let frames: Vec<String> = ws
        .seen_all()
        .iter()
        .filter(|(m, p)| {
            (m == aether_protocol::viewport::ViewportLinesChanged::NAME
                || m == ViewportElementsChanged::NAME)
                && p["viewport_id"] == serde_json::json!(sub.viewport_id)
        })
        .map(|(m, p)| {
            serde_json::to_string(&serde_json::json!({"jsonrpc": "2.0", "method": m, "params": p}))
                .unwrap()
        })
        .collect();
    for frame in &frames {
        costs.pushes += 1;
        costs.sent_bytes += frame.len();
        if frame.contains(ViewportElementsChanged::NAME) {
            costs.patches += 1;
        }
        costs.client += client_handles(&mut clients.patched, frame);
    }
    let s = server.state.lock().await;
    let vp = &s.viewports[&sub.viewport_id];
    let t = Instant::now();
    let whole = render_window(&s, vp, SneakLabels::Hidden);
    let render = t.elapsed();
    let whole_frame = serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": aether_protocol::viewport::ViewportLinesChanged::NAME,
        "params": {"viewport_id": sub.viewport_id, "buffer": 0, "revision": 0, "window": whole},
    }))
    .unwrap();
    costs.render = render * costs.pushes;
    costs.whole_bytes = whole_frame.len() * costs.pushes as usize;
    costs.client_whole = client_handles(&mut clients.whole, &whole_frame) * costs.pushes;
    let elements = whole.root.content().len();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    costs.report(
        &format!(
            "working changes: {files} files, {elements} elements — {steps} files changed on disk"
        ),
        &format!(
            "\n  rebuild (git diff, regenerate, its push): {:.1} ms per change\
             \n  {} pushes for {steps} changes",
            ms(rebuild) / steps as f64,
            costs.pushes
        ),
    );
}

#[tokio::test]
#[ignore = "a measurement; see the module docs"]
async fn working_changes_push_cost() {
    for files in [50, 200, 500] {
        working_changes(files).await;
    }
}
