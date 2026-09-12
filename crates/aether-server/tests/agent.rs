//! `agent/*` end to end: opening an agent view, prompting it, and everything the agent sends back.
//!
//! Everything here goes through the real WebSocket, the real dispatch and the real ACP wire — the
//! agent on the other end is [`aether_server::agent::dummy`], an in-process fake speaking protocol
//! v1 over the SDK's own channel transport. **No test spawns a real agent**: no subprocess, no
//! `npx`, no network, nothing billable. The content assertions read the blocks the way a client
//! does, through a viewport's window, so a test cannot pass on something the shells would never
//! render.

mod common;
use common::*;

use aether_protocol::agent::{
    AgentCancel, AgentCancelParams, AgentOpen, AgentOpenParams, AgentOpenResult, AgentPrompt,
    AgentPromptParams, AgentPromptResult, AgentRespond, AgentRespondParams,
};
use aether_server::agent::dummy::{self, Script, Step, ALLOW_OR_REJECT};
use agent_client_protocol::schema::v1::{ToolCallStatus, ToolKind};
use std::sync::{Arc, Mutex};

// ---- fixtures ----------------------------------------------------------------------------------

/// A workspace with one file, an activated client, and a dummy agent installed behind the seam.
async fn setup(
    script: Script,
) -> (
    aether_server::ServerHandle,
    Ws,
    tempfile::TempDir,
    Arc<Mutex<dummy::Transcript>>,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("agent-proj", vec![root]).await.unwrap();
    server.keep_alive(());

    let started = dummy::start(script);
    let transcript = started.transcript.clone();
    // One connection per test: the transport is taken the first time `agent/open` reaches for it.
    let slot = Mutex::new(Some(started.transport));
    {
        let mut s = server.state.lock().await;
        s.agent_launcher = aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
            slot.lock().expect("dummy slot").take().expect(
                "a test opened two agent views but installed one dummy — install a second script",
            )
        }));
    }

    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "agent-proj".into(),
            open_last: false,
        },
    )
    .await;
    (server, ws, dir, transcript)
}

/// Open a conversation. Always a new one: `agent/open` takes no "reuse" question any more —
/// returning to a conversation you have is the agents picker's job.
async fn open_agent(ws: &mut Ws) -> AgentOpenResult {
    send_request::<AgentOpen>(ws, &AgentOpenParams { agent: None }).await
}

/// The buffer the view's input element windows, found by its **role** rather than by the index the
/// open reported — that index moves as blocks are appended above it.
async fn input_buffer_of(server: &aether_server::ServerHandle, open: &AgentOpenResult) -> u64 {
    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the agent's view");
    view.elements
        .iter()
        .find(|e| e.role.is_input())
        .expect("an agent view has an input")
        .buffer_id
}

async fn type_prompt(ws: &mut Ws, input_buffer: u64, text: &str) {
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
}

async fn input_text(ws: &mut Ws, input: u64) -> String {
    send_request::<BufferContent>(ws, &BufferContentParams { buffer_id: input })
        .await
        .text
}

/// Send a prompt and wait for the turn to end.
async fn prompt_and_wait(
    ws: &mut Ws,
    server: &aether_server::ServerHandle,
    open: &AgentOpenResult,
    text: &str,
) {
    let input = input_buffer_of(server, open).await;
    type_prompt(ws, input, text).await;
    let sent: AgentPromptResult = send_request::<AgentPrompt>(
        ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(sent.sent, "the prompt was not sent");
    wait_for_idle(server, open.opened.view_id).await;
}

/// Wait until the conversation is idle again.
///
/// Read from server state rather than from a push: `send_request` discards notifications while it
/// waits for its own response, so a `turn_changed` that lands during an unrelated request is gone.
/// The state is the thing being asserted on anyway.
async fn wait_for_idle(server: &aether_server::ServerHandle, view_id: aether_protocol::ViewId) {
    loop {
        {
            let s = server.state.lock().await;
            let running = s
                .try_presenting_buffer(view_id)
                .and_then(|b| s.try_doc_of(b))
                .and_then(|d| d.conversation())
                .is_some_and(|c| c.is_running());
            if !running {
                return;
            }
        }
        tokio::task::yield_now().await;
    }
}

/// The conversation's blocks, as (title, text) pairs read from server state — the same values the
/// window is built from, without needing a viewport for the assertions that are about structure.
async fn blocks(
    server: &aether_server::ServerHandle,
    open: &AgentOpenResult,
) -> Vec<(String, String)> {
    let s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).expect("view");
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .expect("an agent view");
    c.blocks
        .iter()
        .map(|b| {
            (
                aether_server::agent::outline_label(b),
                s.try_doc_of(b.buffer)
                    .map(|d| d.text.to_string())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

// ---- opening -----------------------------------------------------------------------------------

#[tokio::test]
async fn open_lands_the_caret_in_the_input() {
    let (server, mut ws, _dir, _t) = setup(Script::default()).await;
    let open = open_agent(&mut ws).await;

    // The open reports which element is the input, and the scroll it hands back names the same
    // one — a client that simply obeys the scroll lands in the right place.
    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("view");
    assert!(
        view.elements[open.input as usize].role.is_input(),
        "agent/open pointed at an element that is not the input"
    );
    assert_eq!(open.opened.scroll.expect("a scroll").element, open.input);
    // A fresh conversation is the input and nothing else.
    assert_eq!(view.elements.len(), 1);
}

// ---- prompting ---------------------------------------------------------------------------------

#[tokio::test]
async fn prompting_reads_and_clears_the_input() {
    let (server, mut ws, _dir, transcript) = setup(Script::default()).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;

    type_prompt(&mut ws, input, "  hello agent  ").await;
    let sent: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(sent.sent);
    wait_for_idle(&server, open.opened.view_id).await;

    // Trimmed on the way out, and the input is empty again.
    assert_eq!(transcript.lock().unwrap().prompts, vec!["hello agent"]);
    assert_eq!(input_text(&mut ws, input).await, "");
    // The prompt became a block of its own, so the conversation reads as one.
    let blocks = blocks(&server, &open).await;
    assert_eq!(blocks[0].0, "You");
    assert_eq!(blocks[0].1, "hello agent\n");
}

#[tokio::test]
async fn an_empty_prompt_is_a_no_op_not_an_error() {
    let (server, mut ws, _dir, transcript) = setup(Script::default()).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "   \n  ").await;

    let sent: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(!sent.sent, "whitespace was sent to the agent");
    assert!(transcript.lock().unwrap().prompts.is_empty());
}

// ---- what the agent sends back -----------------------------------------------------------------

#[tokio::test]
async fn message_chunks_with_one_id_become_one_block() {
    let script = Script {
        steps: vec![
            Step::Say {
                message_id: Some("m1"),
                text: "Hello, ",
            },
            Step::Say {
                message_id: Some("m1"),
                text: "world.\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let blocks = blocks(&server, &open).await;
    let agent: Vec<_> = blocks.iter().filter(|(t, _)| t == "Agent").collect();
    assert_eq!(
        agent.len(),
        1,
        "chunks sharing a messageId split into blocks"
    );
    assert_eq!(agent[0].1, "Hello, world.\n");
}

#[tokio::test]
async fn chunks_without_an_id_append_to_the_open_block() {
    // `ContentChunk::message_id` is optional, and an agent that never sets it is the common case.
    // Without the fallback every chunk would become a block of its own.
    let script = Script {
        steps: vec![
            Step::Say {
                message_id: None,
                text: "one ",
            },
            Step::Say {
                message_id: None,
                text: "two\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let blocks = blocks(&server, &open).await;
    let agent: Vec<_> = blocks.iter().filter(|(t, _)| t == "Agent").collect();
    assert_eq!(
        agent.len(),
        1,
        "anonymous chunks split into separate blocks"
    );
    assert_eq!(agent[0].1, "one two\n");
}

#[tokio::test]
async fn a_thought_and_a_message_are_different_blocks() {
    let script = Script {
        steps: vec![
            Step::Think { text: "hmm\n" },
            Step::Say {
                message_id: None,
                text: "answer\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let kinds: Vec<_> = blocks(&server, &open)
        .await
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(kinds, vec!["You", "Thinking", "Agent"]);
}

#[tokio::test]
async fn a_tool_call_update_merges_rather_than_replacing() {
    // The protocol sends only the fields that changed. Assigning the whole struct would blank the
    // title the agent set when it announced the call.
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Reading a.txt",
                kind: ToolKind::Read,
            },
            Step::Update {
                id: "t1",
                status: Some(ToolCallStatus::Completed),
                text: None,
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let blocks = blocks(&server, &open).await;
    assert!(
        blocks.iter().any(|(t, _)| t == "read Reading a.txt"),
        "the title was lost by a status-only update: {blocks:?}"
    );
}

#[tokio::test]
async fn a_tool_call_grows_after_a_later_block_exists() {
    // The reason a conversation is one document per block: ACP updates a call by its id long after
    // later blocks have been added, and that must be an append to *its* document, not a splice
    // into a shared one.
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "first",
                kind: ToolKind::Execute,
            },
            Step::Say {
                message_id: Some("m1"),
                text: "in between\n",
            },
            Step::Update {
                id: "t1",
                status: Some(ToolCallStatus::Completed),
                text: Some("late output\n"),
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let blocks = blocks(&server, &open).await;
    let call = blocks
        .iter()
        .find(|(t, _)| t.starts_with("run first"))
        .expect("the tool call block");
    assert_eq!(call.1, "late output\n", "the late output missed its block");
    let message = blocks
        .iter()
        .find(|(t, _)| t == "Agent")
        .expect("the message block");
    assert_eq!(
        message.1, "in between\n",
        "the later block was disturbed by the earlier one growing"
    );
}

#[tokio::test]
async fn a_diff_renders_as_a_patch() {
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Editing",
                kind: ToolKind::Edit,
            },
            Step::Propose {
                id: "t1",
                path: "/tmp/a.txt",
                old_text: Some("one\ntwo\n"),
                new_text: "one\nTWO\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let blocks = blocks(&server, &open).await;
    let diff = blocks
        .iter()
        .find(|(t, _)| t.starts_with("diff "))
        .expect("a diff block");
    // Rendered through libgit2, so it is a real unified patch rather than the new text.
    assert!(diff.1.contains("-two"), "no removal line: {}", diff.1);
    assert!(diff.1.contains("+TWO"), "no addition line: {}", diff.1);
}

#[tokio::test]
async fn a_new_file_diff_says_so() {
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Creating",
                kind: ToolKind::Edit,
            },
            Step::Propose {
                id: "t1",
                path: "/tmp/new.txt",
                old_text: None,
                new_text: "fresh\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();
    let is_new = c.blocks.iter().any(|b| match &b.kind {
        aether_server::agent::BlockKind::Diff(d) => d.is_new,
        _ => false,
    });
    assert!(
        is_new,
        "a diff with no old text was not marked as a new file"
    );
}

// ---- permissions -------------------------------------------------------------------------------

#[tokio::test]
async fn a_permission_request_blocks_the_turn_until_it_is_answered() {
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Deleting things",
                kind: ToolKind::Delete,
            },
            Step::Ask {
                id: "t1",
                options: ALLOW_OR_REJECT,
            },
            Step::Say {
                message_id: Some("m1"),
                text: "done\n",
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, transcript) = setup(script).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "go").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;

    // Wait for the question to reach the block, and check the turn has not ended meanwhile.
    let block = loop {
        let s = server.state.lock().await;
        let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
        let c = s
            .try_doc_of(view_buffer)
            .and_then(|d| d.conversation())
            .unwrap();
        if let Some((id, pending)) = c.pending_permission() {
            // The agent's own options, in its own words, with their kinds preserved so a shell can
            // bind accept and reject without reading labels.
            assert_eq!(pending.options.len(), 2);
            assert!(pending.accept().is_some());
            assert!(pending.reject().is_some());
            assert!(c.is_running(), "the turn ended while a question was open");
            break id;
        }
        drop(s);
        tokio::task::yield_now().await;
    };

    let answered = send_request::<AgentRespond>(
        &mut ws,
        &AgentRespondParams {
            view_id: open.opened.view_id,
            block: Some(block),
            answer: aether_protocol::agent::Answer::Allow,
        },
    )
    .await;
    assert!(answered.answered);
    wait_for_idle(&server, open.opened.view_id).await;

    assert_eq!(
        transcript.lock().unwrap().answers,
        vec![Some("allow".to_string())],
        "the agent did not receive the option the user chose"
    );
    // The turn carried on afterwards, which is what "blocks the turn" has to mean.
    let blocks = blocks(&server, &open).await;
    assert!(blocks.iter().any(|(_, text)| text == "done\n"));
}

#[tokio::test]
async fn answering_twice_answers_once() {
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Asking",
                kind: ToolKind::Other,
            },
            Step::Ask {
                id: "t1",
                options: ALLOW_OR_REJECT,
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "go").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;

    let block = loop {
        let s = server.state.lock().await;
        let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
        let c = s
            .try_doc_of(view_buffer)
            .and_then(|d| d.conversation())
            .unwrap();
        if let Some((id, _)) = c.pending_permission() {
            break id;
        }
        drop(s);
        tokio::task::yield_now().await;
    };

    let params = AgentRespondParams {
        view_id: open.opened.view_id,
        block: Some(block),
        answer: aether_protocol::agent::Answer::Allow,
    };
    let first = send_request::<AgentRespond>(&mut ws, &params).await;
    let second = send_request::<AgentRespond>(&mut ws, &params).await;
    assert!(first.answered);
    assert!(
        !second.answered,
        "a second answer was accepted for a question already gone"
    );
    wait_for_idle(&server, open.opened.view_id).await;
}

// ---- the filesystem bridge ---------------------------------------------------------------------

#[tokio::test]
async fn the_agent_reads_unsaved_buffer_text() {
    // The whole reason this view speaks protocol v1: `fs/read_text_file` is specified to include
    // unsaved editor state, and an agent reading stale text off the disk is the failure this
    // prevents.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "on disk\n").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path: &'static str = Box::leak(
        root.join("a.txt")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );

    let mut server = spawn_for_test("agent-proj", vec![root]).await.unwrap();
    server.keep_alive(());
    let started = dummy::start(Script {
        steps: vec![Step::Read { path }],
        ..Script::default()
    });
    let transcript = started.transcript.clone();
    let slot = Mutex::new(Some(started.transport));
    {
        let mut s = server.state.lock().await;
        s.agent_launcher = aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
            slot.lock().unwrap().take().unwrap()
        }));
    }
    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "agent-proj".into(),
            open_last: false,
        },
    )
    .await;

    // Open the file and type into it without saving.
    let opened: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            ..Default::default()
        },
    )
    .await;
    let _: EditResult = send_request::<InputText>(
        &mut ws,
        &InputTextParams {
            buffer_id: opened.buffer_id,
            text: "unsaved ".into(),
            select_pasted: false,
            at: None,
            replace_selection: false,
        },
    )
    .await;

    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "read it").await;

    let read = transcript.lock().unwrap().reads.first().cloned();
    let content = read
        .expect("the agent read a file")
        .expect("the read succeeded");
    assert!(
        content.starts_with("unsaved "),
        "the agent got disk text, not the live buffer: {content:?}"
    );
}

#[tokio::test]
async fn an_agent_write_lands_in_the_open_buffer_and_is_undoable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path: &'static str = Box::leak(
        root.join("a.txt")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    );

    let mut server = spawn_for_test("agent-proj", vec![root]).await.unwrap();
    server.keep_alive(());
    let started = dummy::start(Script {
        steps: vec![Step::Write {
            path,
            content: "after\n",
        }],
        ..Script::default()
    });
    let transcript = started.transcript.clone();
    let slot = Mutex::new(Some(started.transport));
    {
        let mut s = server.state.lock().await;
        s.agent_launcher = aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
            slot.lock().unwrap().take().unwrap()
        }));
    }
    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "agent-proj".into(),
            open_last: false,
        },
    )
    .await;

    let opened: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            path_index: Some(0),
            relative_path: Some("a.txt".into()),
            ..Default::default()
        },
    )
    .await;

    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "write it").await;

    assert_eq!(transcript.lock().unwrap().writes, vec![Ok(())]);
    // In the buffer, not just on disk — and dirty, because it has not been saved.
    let content = send_request::<BufferContent>(
        &mut ws,
        &BufferContentParams {
            buffer_id: opened.buffer_id,
        },
    )
    .await;
    assert_eq!(content.text, "after\n");
    {
        let s = server.state.lock().await;
        assert!(
            s.doc_of(opened.buffer_id).dirty,
            "an agent write left the buffer looking saved"
        );
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "before\n",
        "an agent write went to disk behind the buffer"
    );
}

// ---- hygiene -----------------------------------------------------------------------------------

#[tokio::test]
async fn block_documents_are_internal() {
    // Blocks and the input are parts of a view, not documents of the user's: they must never be
    // listed, and must never make the view look modified while the agent is writing into it.
    let script = Script {
        steps: vec![Step::Say {
            message_id: Some("m1"),
            text: "text\n",
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();
    for buffer in c.documents() {
        let doc = s.try_doc_of(buffer).expect("a live document");
        assert!(doc.internal, "a conversation document was not internal");
        assert!(!doc.dirty, "a conversation document counted as dirty");
    }
    // Every block is read-only by construction, so no edit path can reach one.
    for block in &c.blocks {
        assert!(
            s.try_doc_of(block.buffer).unwrap().read_only(),
            "a block was editable"
        );
    }
}

/// `agent/open` **always creates** — from anywhere, an idle conversation included.
///
/// It used to hand back the idle one, a rule that existed only because there was no way to *list*
/// the conversations: the same key opened a new one or an old one depending on state the user
/// could not see. `Space a` is that list now, so `Space Alt-a` has exactly one meaning and the
/// `from_view` parameter the rule was decided from is gone.
#[tokio::test]
async fn every_open_mints_the_next_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("agent-proj", vec![root]).await.unwrap();
    server.keep_alive(());
    // Two dummies, because two conversations means two connections.
    let slots = Mutex::new(vec![
        dummy::start(Script::default()).transport,
        dummy::start(Script::default()).transport,
    ]);
    {
        let mut s = server.state.lock().await;
        s.agent_launcher = aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
            slots.lock().expect("slots").pop().expect("a third agent")
        }));
    }
    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "agent-proj".into(),
            open_last: false,
        },
    )
    .await;

    let first = open_agent(&mut ws).await;
    // A second open: a second conversation, numbered after the first.
    let second = open_agent(&mut ws).await;
    assert_ne!(
        first.opened.view_id, second.opened.view_id,
        "the second open returned the same conversation"
    );

    let titles = {
        let s = server.state.lock().await;
        [first.opened.view_id, second.opened.view_id]
            .into_iter()
            .filter_map(|v| s.try_presenting_buffer(v))
            .filter_map(|b| s.try_doc_of(b))
            .filter_map(|d| d.conversation().map(|c| c.title.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(titles, vec!["Agent 1".to_string(), "Agent 2".to_string()]);
}

#[tokio::test]
async fn a_second_prompt_during_a_turn_is_refused_and_keeps_the_text() {
    // Typing ahead of a working agent is a reasonable thing to do, so the refusal must not eat
    // what was typed.
    let script = Script {
        steps: vec![Step::Ask {
            id: "t1",
            options: ALLOW_OR_REJECT,
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "first").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;

    // The turn is held open by the unanswered question.
    loop {
        let s = server.state.lock().await;
        let blocked = s
            .try_presenting_buffer(open.opened.view_id)
            .and_then(|b| s.try_doc_of(b))
            .and_then(|d| d.conversation())
            .is_some_and(|c| c.pending_permission().is_some());
        if blocked {
            break;
        }
        drop(s);
        tokio::task::yield_now().await;
    }

    type_prompt(&mut ws, input, "second").await;
    let refused = send_request_result::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    let err = refused.expect_err("a second prompt during a turn was accepted");
    assert_eq!(
        err.get("code").and_then(|c| c.as_i64()),
        Some(aether_protocol::error::ErrorCode::AGENT_BUSY.code() as i64)
    );
    assert_eq!(
        input_text(&mut ws, input).await,
        "second",
        "the refusal ate what was typed ahead"
    );

    // Let it finish so the test does not leave a turn hanging.
    let _ = send_request::<AgentRespond>(
        &mut ws,
        &AgentRespondParams {
            view_id: open.opened.view_id,
            block: None,
            answer: aether_protocol::agent::Answer::Decline,
        },
    )
    .await;
    wait_for_idle(&server, open.opened.view_id).await;
}

#[tokio::test]
async fn cancelling_ends_the_turn_and_answers_the_open_question() {
    // The protocol requires a cancelling client to answer outstanding permission requests rather
    // than leave the agent blocked on one.
    let script = Script {
        steps: vec![Step::Ask {
            id: "t1",
            options: ALLOW_OR_REJECT,
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, transcript) = setup(script).await;
    let open = open_agent(&mut ws).await;
    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "go").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    loop {
        let s = server.state.lock().await;
        let blocked = s
            .try_presenting_buffer(open.opened.view_id)
            .and_then(|b| s.try_doc_of(b))
            .and_then(|d| d.conversation())
            .is_some_and(|c| c.pending_permission().is_some());
        if blocked {
            break;
        }
        drop(s);
        tokio::task::yield_now().await;
    }

    let cancelled = send_request::<AgentCancel>(
        &mut ws,
        &AgentCancelParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(cancelled.cancelled);
    wait_for_idle(&server, open.opened.view_id).await;

    assert_eq!(
        transcript.lock().unwrap().answers,
        vec![None],
        "cancelling left the agent waiting on a question"
    );
    // Nothing is left marked as still going.
    let s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();
    assert!(c.pending_permission().is_none());
    assert!(!c.is_running());
}

/// A permission request re-pushes the open agents picker with `awaiting permission` on the row.
///
/// The badge changes whether or not a turn is in flight, so it goes out on its own rather than
/// through the turn-state push — and answering it puts the row back to `thinking`. Recency
/// ordering means the row re-paints where it is.
#[tokio::test]
async fn a_permission_request_repushes_the_agents_picker() {
    use aether_protocol::picker::{
        AgentRowState, PickerItem, PickerKind, PickerUpdate, PickerUpdateParams, PickerView,
    };
    let script = Script {
        steps: vec![Step::Ask {
            id: "t1",
            options: ALLOW_OR_REJECT,
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;

    // The resting row: connected, nothing in flight.
    let view = send_request::<PickerView>(&mut ws, &view_params(PickerKind::Agents)).await;
    let rows = view.update.and_then(|u| u.items).expect("a window");
    let PickerItem::Agent { state, title, .. } = &rows[0] else {
        panic!("an agent row");
    };
    assert_eq!(*state, AgentRowState::Idle);
    assert_eq!(title, "Agent 1");

    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "go").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;

    let mut saw_thinking = false;
    loop {
        let update: PickerUpdateParams =
            expect_notification_within::<PickerUpdate>(&mut ws, std::time::Duration::from_secs(10))
                .await;
        if update.kind != PickerKind::Agents {
            continue;
        }
        let Some(PickerItem::Agent {
            state, last_prompt, ..
        }) = update.items().first().cloned()
        else {
            continue;
        };
        match state {
            AgentRowState::Thinking { .. } => saw_thinking = true,
            AgentRowState::AwaitingPermission => {
                assert_eq!(
                    last_prompt.as_deref(),
                    Some("go"),
                    "the row carries the last thing said to it"
                );
                break;
            }
            other => panic!("unexpected row state {other:?}"),
        }
    }
    assert!(
        saw_thinking,
        "the turn starting pushed a `thinking` row first"
    );

    // Answering puts it back to working.
    let _ = send_request::<AgentRespond>(
        &mut ws,
        &AgentRespondParams {
            view_id: open.opened.view_id,
            block: None,
            answer: aether_protocol::agent::Answer::Allow,
        },
    )
    .await;
    loop {
        let update: PickerUpdateParams =
            expect_notification_within::<PickerUpdate>(&mut ws, std::time::Duration::from_secs(10))
                .await;
        if update.kind != PickerKind::Agents {
            continue;
        }
        let Some(PickerItem::Agent { state, .. }) = update.items().first().cloned() else {
            continue;
        };
        if !matches!(state, AgentRowState::AwaitingPermission) {
            break;
        }
    }
    wait_for_idle(&server, open.opened.view_id).await;
}

/// `view/interrupt` — `Space v c` — stops a turn without naming an agent.
///
/// The same key stops a shell's run; the client cannot tell the two apart and does not need to.
/// A conversation with nothing in flight answers `interrupted: false`, which is what produces the
/// client's one "Nothing is running here".
#[tokio::test]
async fn view_interrupt_stops_a_turn() {
    use aether_protocol::view::{ViewInterrupt, ViewInterruptParams, ViewInterruptResult};
    let script = Script {
        steps: vec![Step::Ask {
            id: "t1",
            options: ALLOW_OR_REJECT,
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;

    // Idle: nothing to stop, and not an error.
    let idle: ViewInterruptResult = send_request::<ViewInterrupt>(
        &mut ws,
        &ViewInterruptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(!idle.interrupted);

    let input = input_buffer_of(&server, &open).await;
    type_prompt(&mut ws, input, "go").await;
    let _: AgentPromptResult = send_request::<AgentPrompt>(
        &mut ws,
        &AgentPromptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    loop {
        let s = server.state.lock().await;
        let running = s
            .try_presenting_buffer(open.opened.view_id)
            .and_then(|b| s.try_doc_of(b))
            .and_then(|d| d.conversation())
            .is_some_and(|c| c.is_running());
        if running {
            break;
        }
        drop(s);
        tokio::task::yield_now().await;
    }

    let stopped: ViewInterruptResult = send_request::<ViewInterrupt>(
        &mut ws,
        &ViewInterruptParams {
            view_id: open.opened.view_id,
        },
    )
    .await;
    assert!(stopped.interrupted);
    wait_for_idle(&server, open.opened.view_id).await;
}

#[tokio::test]
async fn prose_is_bare_and_the_machinery_is_boxed() {
    // What you typed and what the agent said back carry no box; a tool call does, because it is a
    // named thing that happened rather than something written to be read.
    let script = Script {
        steps: vec![
            Step::Say {
                message_id: Some("m1"),
                text: "Looking at it.\n",
            },
            Step::Call {
                id: "t1",
                title: "Reading a.txt",
                kind: ToolKind::Read,
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "fix it").await;

    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the agent's view");
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();

    for (block, element) in c.blocks.iter().zip(view.elements.iter()) {
        let bare = aether_server::agent::is_prose(&block.kind);
        assert_eq!(
            element.edges.border.top == 0,
            bare,
            "{:?} is boxed the wrong way round",
            aether_server::agent::outline_label(block)
        );
        // A bare block has no title to hang on a border it does not have.
        assert_eq!(element.title.is_empty(), bare);
        assert_eq!(element.box_group.is_none(), bare);
    }
    // The input is bare too: everything its box used to say — which agent, where, doing what, and
    // the key that stops it — the status bar says, or the tool call's own box does.
    let input = view.elements.last().expect("the input");
    assert!(input.role.is_input());
    assert_eq!(input.edges.border.top, 0, "the input still has a box");
    assert!(input.title.is_empty(), "the input still has a label");
}

#[tokio::test]
async fn a_test_server_with_no_dummy_launches_nothing() {
    // The guard that matters most in this file. Every other test installs an in-process dummy; if
    // forgetting to were merely a mistake, the fallback would be `npx @agentclientprotocol/
    // claude-agent-acp` — a real coding agent, with the developer's credentials, doing real work,
    // once per forgetful test. `AgentLauncher::Refuse` makes that unreachable rather than
    // unlikely, and this is the test that says so.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut server = spawn_for_test("agent-proj", vec![root]).await.unwrap();
    server.keep_alive(());
    // Deliberately no dummy installed.
    let mut ws = Ws::connect(&server).await;
    let _: WorkspaceActivateResult = send_request::<WorkspaceActivate>(
        &mut ws,
        &WorkspaceActivateParams {
            worktrees: None,
            name: "agent-proj".into(),
            open_last: false,
        },
    )
    .await;

    let refused = send_request_result::<AgentOpen>(&mut ws, &AgentOpenParams { agent: None }).await;
    let err = refused.expect_err("a test server launched an agent");
    assert_eq!(
        err.get("code").and_then(|c| c.as_i64()),
        Some(aether_protocol::error::ErrorCode::AGENT_UNAVAILABLE.code() as i64)
    );
}

/// CPU burnt by this process over `ms`, in jiffies (100 to a core-second).
///
/// Process-wide, which is the catch: the suite runs tests in parallel in one process, so an
/// absolute reading picks up whatever else is running. Both detectors below therefore measure a
/// **baseline first** and assert on the increase — the neighbours' noise lands in both windows and
/// largely cancels, while a spin (a core and a half, ~150 jiffies a second) does not.
async fn cpu_over(ms: u64) -> u64 {
    fn jiffies() -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").expect("procfs");
        let tail = stat.rsplit(')').next().expect("stat tail");
        let fields: Vec<&str> = tail.split_whitespace().collect();
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        utime + stime
    }
    let before = jiffies();
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    jiffies().saturating_sub(before)
}

/// How much more CPU a second of idling may cost with the thing running than without it. A real
/// spin was ~150 jiffies a second; parallel-test noise moves the two windows by rather less.
const SPIN_JIFFIES: u64 = 60;

/// A spin detector, not a behaviour test: an **idle** conversation must cost no CPU.
///
/// A view sitting there with an agent attached and nothing happening should be entirely asleep —
/// every loop involved parks on a channel. Anything that busy-polls shows up here as burnt jiffies
/// while the test does nothing at all.
#[tokio::test]
async fn an_idle_conversation_burns_no_cpu() {
    let (server, mut ws, _dir, _t) = setup(Script::default()).await;
    let baseline = cpu_over(1000).await;

    let open = open_agent(&mut ws).await;
    // Let the handshake settle, so what we measure is the resting state.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let burnt = cpu_over(1000).await;

    assert!(
        burnt.saturating_sub(baseline) < SPIN_JIFFIES,
        "an idle conversation added {} jiffies a second over a {baseline}-jiffy baseline — \
         something is busy-polling",
        burnt.saturating_sub(baseline)
    );
    let _ = (open, server);
}

/// The same spin detector, but over a **real subprocess** transport instead of the in-process
/// channel every other test uses.
///
/// This is the shape a real agent runs in, and the one shape the dummy cannot reproduce. It exists
/// because the SDK's own process transport is built on `async_process`, whose futures belong to the
/// `async-io` reactor: driven by a tokio worker they busy-poll, and an idle conversation burnt a
/// core and a half in `read`s returning `EAGAIN`. The process is spawned by us now
/// (`agent::connection::launch`), and this says so in the only terms that matter.
///
/// `cat` stands in for an agent: a live stdio peer that never answers, so the handshake stays
/// outstanding and nothing should be doing anything at all.
#[tokio::test]
async fn a_subprocess_transport_burns_no_cpu_while_idle() {
    static CAT: aether_server::agent::AgentSpec = aether_server::agent::AgentSpec {
        id: "cat",
        name: "cat",
        program: "cat",
        args: &[],
    };
    let baseline = cpu_over(1000).await;

    let (_handle, _events) = aether_server::agent::connection::spawn(
        &CAT,
        std::path::PathBuf::from("/tmp"),
        std::collections::HashMap::new(),
        None,
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let burnt = cpu_over(1000).await;

    assert!(
        burnt.saturating_sub(baseline) < SPIN_JIFFIES,
        "an idle subprocess transport added {} jiffies a second over a {baseline}-jiffy baseline",
        burnt.saturating_sub(baseline)
    );
}

#[tokio::test]
async fn the_agents_reply_is_prose_on_the_wire() {
    // Markdown is rendered, not shown as source: the reply rides the window as `Element::Prose` —
    // the parse, not the lines — while everything else in the view stays server-wrapped text.
    // Both halves matter. A reply sent as an editor would be painted as its own source, and a tool
    // call sent as prose would lose its box.
    let script = Script {
        steps: vec![
            Step::Say {
                message_id: Some("m1"),
                text: "# Heading\n\nSome prose.\n",
            },
            Step::Call {
                id: "t1",
                title: "Reading a.txt",
                kind: ToolKind::Read,
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "hi").await;

    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the agent's view");
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();

    for (block, element) in c.blocks.iter().zip(view.elements.iter()) {
        let rendered = aether_server::agent::is_rendered(&block.kind);
        assert_eq!(
            element.prose,
            rendered,
            "{:?} is bound as the wrong kind of content",
            aether_server::agent::outline_label(block)
        );
        // Prose implies the client's arithmetic: proportional type has no height in rows.
        assert!(
            !element.prose || element.laid_out_by == aether_protocol::ui::LayoutOwner::Client,
            "{:?} is prose the server claims to lay out",
            aether_server::agent::outline_label(block)
        );
    }
    // The input is typed into, so it is never prose and never the client's to lay out.
    let input = view.elements.last().expect("the input");
    assert!(!input.prose);
    assert_eq!(input.laid_out_by, aether_protocol::ui::LayoutOwner::Server);
    drop(s);

    // And what actually goes out: the reply is a `Prose` node carrying the *parse* — a heading is
    // a heading, with no `#` anywhere — and it carries no lines at all.
    let sub: ViewportSubscribeResult = send_request::<ViewportSubscribe>(
        &mut ws,
        &ViewportSubscribeParams {
            view_id: open.opened.view_id,
            cols: 100,
            rows: 60,
            overscan_rows: 0,
            scroll: ScrollPosition {
                element: 0,
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
    .await;
    let prose: Vec<&aether_protocol::viewport::Element> = sub
        .window
        .root
        .content()
        .into_iter()
        .filter(|e| matches!(e, aether_protocol::viewport::Element::Prose { .. }))
        .collect();
    let [reply] = prose[..] else {
        panic!("expected exactly one prose element, got {}", prose.len());
    };
    let aether_protocol::viewport::Element::Prose { blocks, .. } = reply else {
        unreachable!()
    };
    assert!(
        matches!(
            blocks.first(),
            Some(aether_markdown::Block::Heading { level: 1, .. })
        ),
        "the reply's markdown was not parsed: {blocks:?}"
    );
    // The tool call beside it is still an editor with lines — the boxed machinery, not prose.
    assert!(
        sub.window.root.lines().iter().all(|l| !l
            .visual_rows
            .iter()
            .any(|r| r.segments.iter().any(|s| s.text.contains('#')))),
        "markdown source reached the wire"
    );
}

#[tokio::test]
async fn a_plan_keeps_each_entrys_state() {
    // The plan is the most useful thing on screen during a long turn, and its value is per entry:
    // ACP sends a status with each one, and dropping it (as the first cut did) leaves a flat list
    // that says nothing about progress.
    use agent_client_protocol::schema::v1::PlanEntryStatus;
    let script = Script {
        steps: vec![Step::Plan {
            entries: &[
                ("Read the parser", PlanEntryStatus::Completed),
                ("Fix the semicolon", PlanEntryStatus::InProgress),
                ("Add a test", PlanEntryStatus::Pending),
            ],
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "plan it").await;

    let blocks = blocks(&server, &open).await;
    let plan = blocks
        .iter()
        .find(|(title, _)| title == "Plan")
        .expect("a plan block");
    // One line per entry, each marked with its own state — done, running, still to do.
    assert_eq!(
        plan.1, "● Read the parser\n◐ Fix the semicolon\n○ Add a test\n",
        "the plan lost its per-entry state"
    );
}

#[tokio::test]
async fn a_tool_calls_box_is_named_by_the_agent_not_by_us() {
    // Every call Claude Code sends is `execute`, so printing the kind first made every box wear
    // the word "run" — which read as the name of the block rather than a description of it. The
    // agent's own title says what it is doing; the kind only stands in when there is no title.
    let script = Script {
        steps: vec![
            Step::Call {
                id: "t1",
                title: "Running cargo test",
                kind: ToolKind::Execute,
            },
            Step::Update {
                id: "t1",
                status: Some(ToolCallStatus::Completed),
                text: None,
            },
        ],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "test it").await;

    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the view");
    let titles: Vec<String> = view
        .elements
        .iter()
        .map(|e| {
            e.title
                .iter()
                .filter_map(|n| match n {
                    aether_protocol::viewport::Element::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect();
    let call = titles
        .iter()
        .find(|t| t.contains("cargo test"))
        .expect("the tool call's box");
    // Marked by state, named by the agent, and the word "run" is nowhere in it.
    assert!(call.starts_with("● "), "no completed mark: {call:?}");
    assert!(
        !call.contains("run"),
        "the kind label is still there: {call:?}"
    );
}

#[tokio::test]
async fn a_block_has_no_trailing_blank_line() {
    // A block's text is a record of something that happened and almost always ends in a newline.
    // Bound to its buffer's *line* count that terminator becomes a row of its own, and every block
    // — most visibly your own prompt — trails a blank line. A file's trailing empty line is a real
    // place to put the cursor, which is why the layout counts it in general; a block has no cursor
    // to put there.
    let script = Script {
        steps: vec![Step::Say {
            message_id: Some("m1"),
            text: "One line.\n",
        }],
        ..Script::default()
    };
    let (server, mut ws, _dir, _t) = setup(script).await;
    let open = open_agent(&mut ws).await;
    prompt_and_wait(&mut ws, &server, &open, "a prompt").await;

    let s = server.state.lock().await;
    let view = s.try_view(open.opened.view_id).expect("the view");
    let view_buffer = s.try_presenting_buffer(open.opened.view_id).unwrap();
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .unwrap();

    for (block, element) in c.blocks.iter().zip(view.elements.iter()) {
        let doc = s.try_doc_of(block.buffer).expect("the block's document");
        let shown = element.lines_in(doc.line_count());
        assert_eq!(
            shown.end - shown.start,
            doc.content_lines(),
            "{:?} shows {} rows for {} lines of content",
            aether_server::agent::outline_label(block),
            shown.end - shown.start,
            doc.content_lines()
        );
    }
    // Concretely: a one-line prompt is one row, not two.
    let prompt = &c.blocks[0];
    assert_eq!(
        s.try_doc_of(prompt.buffer).unwrap().text.to_string(),
        "a prompt\n"
    );
    assert_eq!(
        view.elements[0].lines_in(2),
        0..1,
        "the prompt trails a blank row"
    );
}

/// A conversation survives a restart: its blocks and the text typed but not sent come back from a
/// snapshot, as a **dormant row that launches nothing** — the difference from a shell, and the
/// reason for it, is that an agent is a subprocess that costs real money to run.
#[tokio::test]
async fn a_conversation_survives_a_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sessions_path = root.join("sessions.json");
    let backups = root.join("backups");

    {
        let mut server = aether_server::spawn_for_test_multi_with_persistence(
            vec![("p".to_string(), vec![root.clone()])],
            Some(sessions_path.clone()),
            Some(backups.clone()),
        )
        .await
        .unwrap();
        let started = dummy::start(Script {
            steps: vec![
                Step::Say {
                    message_id: Some("m1"),
                    text: "It drops them in the parser.\n",
                },
                Step::Call {
                    id: "t1",
                    title: "Reading parse.rs",
                    kind: ToolKind::Read,
                },
            ],
            ..Script::default()
        });
        let slot = Mutex::new(Some(started.transport));
        {
            let mut s = server.state.lock().await;
            s.agent_launcher = aether_server::state::AgentLauncher::Dummy(Arc::new(move || {
                slot.lock().unwrap().take().unwrap()
            }));
        }
        let mut ws = Ws::connect(&server).await;
        activate_p(&mut ws).await;
        let open = open_agent(&mut ws).await;
        prompt_and_wait(&mut ws, &server, &open, "why are semicolons dropped?").await;
        let input = input_buffer_of(&server, &open).await;
        type_prompt(&mut ws, input, "typed ahead").await;

        // The periodic flush writes it; wait for the text that proves it is this conversation.
        let path = backups.join("agent").join("p").join("1");
        let mut written = None;
        for _ in 0..200 {
            if let Ok(json) = std::fs::read_to_string(&path) {
                if json.contains("typed ahead") && json.contains("Reading parse.rs") {
                    written = Some(json);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            written.is_some(),
            "no snapshot at {}: {:?}",
            path.display(),
            std::fs::read_to_string(&path)
        );
        server.keep_alive(());
        drop(ws);
        drop(server);
    }

    // Second life: the workspace cold-loads from its on-disk config, so the session's agent entry
    // comes back as a dormant row over its snapshot.
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

    let dormant_view = {
        let s = server.state.lock().await;
        let entry = s.workspaces.get("p").expect("the workspace");
        entry
            .dormant_views
            .iter()
            .find(|d| format!("{:?}", d.source) == "Agent { number: 1 }")
            .map(|d| d.view)
            .unwrap_or_else(|| {
                panic!(
                    "Agent 1 is dormant: session={:?} dormant={:?}",
                    std::fs::read_to_string(&sessions_path),
                    entry
                        .dormant_views
                        .iter()
                        .map(|d| format!("{:?}", d.source))
                        .collect::<Vec<_>>()
                )
            })
    };

    // Opening it launches **nothing** — no dummy is installed on this server, and
    // `AgentLauncher::Refuse` would fail the open if it tried.
    let restored: ViewOpenResult = send_request::<ViewOpen>(
        &mut ws,
        &ViewOpenParams {
            view_id: Some(dormant_view),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(restored.title.as_deref(), Some("Agent 1"));

    let s = server.state.lock().await;
    let view_buffer = s.try_presenting_buffer(restored.view_id).expect("the view");
    let c = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .expect("a restored conversation");
    assert!(c.handle.is_none(), "opening a record started an agent");

    // What was on screen is back — including the tool call, which a `session/load` replay is under
    // no obligation to carry.
    let labels: Vec<String> = c
        .blocks
        .iter()
        .map(aether_server::agent::outline_label)
        .collect();
    assert_eq!(labels, vec!["You", "Agent", "read Reading parse.rs"]);
    assert_eq!(
        s.try_doc_of(c.input).unwrap().text.to_string(),
        "typed ahead",
        "what was typed but not sent"
    );
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

/// **`Space k` cannot arm a conversation to close itself**, for the reason a shell cannot: an
/// agent view is composed and created kept, so `view/set_transient { transient: true }` answers the
/// flag as it stands (`false`) instead of changing it.
#[tokio::test]
async fn an_agent_view_cannot_be_made_transient() {
    use aether_protocol::view::{ViewSetTransient, ViewSetTransientParams, ViewSetTransientResult};
    let (server, mut ws, _dir, _t) = setup(Script::default()).await;
    let agent = open_agent(&mut ws).await;

    let answered: ViewSetTransientResult = send_request::<ViewSetTransient>(
        &mut ws,
        &ViewSetTransientParams {
            view_id: agent.opened.view_id,
            transient: true,
        },
    )
    .await;
    assert!(
        !answered.transient,
        "the answer is the actual flag: a conversation stays kept"
    );
    assert!(
        !server
            .state
            .lock()
            .await
            .view(agent.opened.view_id)
            .transient,
        "and nothing moved server-side"
    );

    drop(server);
}
