//! The ACP connection: one agent subprocess, one session, one task.
//!
//! Shaped like [`crate::lsp`]'s: the protocol work runs in a task of its own, the editor talks to
//! it through a channel, and nothing that holds the state lock ever awaits the agent. What arrives
//! back is a stream of [`AgentEvent`]s, which [`crate::handlers::agent`] turns into blocks.
//!
//! The subprocess is the SDK's `AcpAgent`, which already spawns into its own process group and
//! kills the group when it goes — so an agent that started a build does not leave it orphaned, and
//! we do not re-implement what [`crate::process`] does for shells.
//!
//! Two things the agent asks *us* for are the reason this view is worth having, and both are
//! answered from editor state rather than the disk: `fs/read_text_file` serves the live buffer,
//! unsaved edits included, which is exactly what the protocol asks for; `fs/write_text_file` goes
//! through the ordinary edit path, so the agent's changes are undoable. Neither is answered here —
//! both become an event with a reply channel, because the state lock lives on the other side.

use aether_protocol::agent::{PermissionKind, PermissionOption, StopReason};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, FileSystemCapabilities, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PermissionOptionId, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    StopReason as AcpStopReason, TextContent, ToolCallContent, ToolCallStatus as AcpToolCallStatus,
    ToolKind as AcpToolKind, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::schema::ProtocolVersion;

use super::{AgentSpec, Location, ToolKind, ToolStatus};

/// What the editor asks of a running agent.
#[derive(Debug)]
pub enum AgentCommand {
    /// Start a turn. Refused upstream if one is already running, so this never queues.
    Prompt(String),
    /// Ask the agent to stop the running turn.
    Cancel,
    /// Answer a parked permission request. `None` cancels it, which is what a cancelled turn and
    /// a closing view do on the user's behalf.
    Respond {
        request: u64,
        option: Option<String>,
    },
}

/// What a running agent tells the editor. Every variant names something a block is made of, or
/// something the agent needs from editor state.
#[derive(Debug)]
pub enum AgentEvent {
    /// The handshake finished and a session exists. Nothing can be sent before this.
    ///
    /// Carries the ACP session id, which is what a snapshot records and what `session/load` names
    /// to bring the agent's own context back after a restart. `resumed` says whether that is what
    /// happened: `false` means this agent has never seen the conversation on screen, which is a
    /// thing the user has to be told rather than left to infer.
    Ready { session: String, resumed: bool },
    /// Streamed prose. `message_id` is the protocol's, and is **optional**: without one, the
    /// content appends to whichever block of this kind is currently open.
    Chunk {
        kind: ChunkKind,
        message_id: Option<String>,
        text: String,
    },
    /// A tool call, created or updated. Every field but the id is optional because the protocol
    /// sends only what changed — merging rather than replacing is the whole contract.
    ToolCall {
        tool_call_id: String,
        title: Option<String>,
        kind: Option<ToolKind>,
        status: Option<ToolStatus>,
        locations: Option<Vec<Location>>,
        text: Option<String>,
    },
    /// A file the agent proposes to change, as the n-th diff of one tool call's content.
    Diff {
        tool_call_id: String,
        index: usize,
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    },
    /// The agent's plan for the turn, whole — a new plan replaces the old. Each entry carries its
    /// own state, which is what the plan is *for* while a long turn runs.
    Plan { entries: Vec<(String, ToolStatus)> },
    /// The agent is blocked on the user. Answer with [`AgentCommand::Respond`].
    Permission {
        request: u64,
        tool_call_id: Option<String>,
        options: Vec<PermissionOption>,
    },
    /// `fs/read_text_file`: serve from the live buffer, unsaved edits included.
    ReadFile {
        path: PathBuf,
        /// 1-based, as the protocol sends it.
        line: Option<u32>,
        limit: Option<u32>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// `fs/write_text_file`: apply through the ordinary edit path, creating the file if needed.
    WriteFile {
        path: PathBuf,
        content: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// The turn ended. Always arrives exactly once per [`AgentCommand::Prompt`] that was accepted.
    TurnEnded(StopReason),
    /// The connection is gone: launch failed, handshake failed, or the agent exited. Terminal —
    /// nothing follows it.
    Failed(String),
}

/// Which kind of prose a chunk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    User,
    Agent,
    Thought,
}

/// The agent's stdio as the SDK wants it: futures-io byte streams over tokio's child pipes.
type AgentPipes = agent_client_protocol::ByteStreams<
    tokio_util::compat::Compat<tokio::process::ChildStdin>,
    tokio_util::compat::Compat<tokio::process::ChildStdout>,
>;

/// The editor's end of a running agent. Dropping it ends the task, which drops the transport,
/// which kills the subprocess group.
#[derive(Debug)]
pub struct AgentHandle {
    commands: mpsc::UnboundedSender<AgentCommand>,
}

impl AgentHandle {
    /// Send a command. `false` when the connection task has gone — the caller turns that into
    /// [`aether_protocol::error::ErrorCode::AGENT_UNAVAILABLE`] rather than panicking, because an
    /// agent exiting is ordinary.
    pub fn send(&self, command: AgentCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// Permission requests parked while the user decides. Shared between the request handler (which
/// parks and awaits) and the command loop (which answers).
#[derive(Default)]
struct Parked {
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Option<String>>>>,
}

impl Parked {
    fn park(&self) -> (u64, oneshot::Receiver<Option<String>>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().expect("parked lock").insert(id, tx);
        (id, rx)
    }

    fn answer(&self, request: u64, option: Option<String>) {
        if let Some(tx) = self.waiting.lock().expect("parked lock").remove(&request) {
            let _ = tx.send(option);
        }
    }

    /// Cancel everything outstanding — what a cancelled turn and a closing view do.
    fn cancel_all(&self) {
        let waiting: Vec<_> = self
            .waiting
            .lock()
            .expect("parked lock")
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in waiting {
            let _ = tx.send(None);
        }
    }
}

/// Launch `spec` in `cwd` and connect to it.
///
/// Returns immediately: the handshake happens in the task, and [`AgentEvent::Ready`] or
/// [`AgentEvent::Failed`] says how it went. That is deliberate — opening the view must not block
/// on `npx` fetching a package.
pub fn spawn(
    spec: &'static AgentSpec,
    cwd: PathBuf,
    env: HashMap<String, String>,
    resume: Option<String>,
) -> (AgentHandle, mpsc::UnboundedReceiver<AgentEvent>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        match launch(spec, &cwd, env) {
            Err(message) => {
                let _ = event_tx.send(AgentEvent::Failed(message));
            }
            Ok((transport, mut child)) => {
                let pid = child.id();
                let failure = run(transport, cwd, resume, command_rx, event_tx.clone()).await;
                // The whole group, not the child: agents ship behind `npx`, and killing the
                // wrapper leaves the real agent re-parented to init and running. `kill_on_drop`
                // only reaches the direct child, which is why this is explicit.
                if let Some(pid) = pid {
                    crate::process::kill_group(pid, crate::process::libc_sigterm());
                }
                let _ = child.start_kill();
                if let Err(message) = failure {
                    let _ = event_tx.send(AgentEvent::Failed(message));
                }
            }
        }
    });

    (
        AgentHandle {
            commands: command_tx,
        },
        event_rx,
    )
}

/// Start the agent process and wrap its pipes as a transport.
///
/// **Spawned with tokio, deliberately.** The SDK ships an `AcpAgent` that spawns the process
/// itself, and using it was the obvious thing — it even puts the child in its own process group.
/// But it is built on `async_process`, whose futures belong to the `async-io` reactor; polled by a
/// tokio worker they busy-poll, and an idle conversation burns a core and a half in `read`s that
/// return `EAGAIN`. So the process is ours: [`crate::process::command`] already gives a tokio child
/// in its own group, and the pipes reach the SDK through `ByteStreams` and the compat shims.
///
/// The same shape [`crate::lsp::process`] uses for a language server, for the same reasons, down to
/// draining stderr into the log rather than letting it fill and block the child.
fn launch(
    spec: &'static AgentSpec,
    cwd: &Path,
    env: HashMap<String, String>,
) -> Result<(AgentPipes, tokio::process::Child), String> {
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let mut command = crate::process::command(spec.program);
    command
        .args(spec.args)
        .current_dir(cwd)
        .envs(env)
        // `crate::process::command` nulls stdin — a shell's command has nothing to say to it. An
        // agent's stdin is half the protocol.
        .stdin(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", spec.program))?;
    let stdin = child.stdin.take().ok_or("the agent has no stdin")?;
    let stdout = child.stdout.take().ok_or("the agent has no stdout")?;
    if let Some(stderr) = child.stderr.take() {
        let name = spec.name;
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(agent = name, "{line}");
            }
        });
    }

    Ok((
        agent_client_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat()),
        child,
    ))
}

/// Connect over an already-built transport. The subprocess case goes through [`spawn`]; tests use
/// this with an in-process channel so nothing is launched.
pub fn connect<T>(
    transport: T,
    cwd: PathBuf,
    resume: Option<String>,
) -> (AgentHandle, mpsc::UnboundedReceiver<AgentEvent>)
where
    T: agent_client_protocol::ConnectTo<agent_client_protocol::Client> + Send + 'static,
{
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let failure = run(transport, cwd, resume, command_rx, event_tx.clone()).await;
        if let Err(message) = failure {
            let _ = event_tx.send(AgentEvent::Failed(message));
        }
    });

    (
        AgentHandle {
            commands: command_tx,
        },
        event_rx,
    )
}

async fn run<T>(
    transport: T,
    cwd: PathBuf,
    resume: Option<String>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
    events: mpsc::UnboundedSender<AgentEvent>,
) -> Result<(), String>
where
    T: agent_client_protocol::ConnectTo<agent_client_protocol::Client> + Send + 'static,
{
    let parked = Arc::new(Parked::default());
    // A `session/load` replays the whole conversation as ordinary `session/update` notifications.
    // We already have that conversation on screen, restored from our own snapshot — which is the
    // *richer* record, since an agent is under no obligation to replay tool calls at all. So the
    // replay is counted and dropped rather than rendered: rendering it would duplicate every
    // message, and `messageId` is documented as "an opaque, unique identifier for the replayed
    // message", so the ids cannot be relied on to merge.
    let replaying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let replay_counts = Arc::new(Mutex::new((0usize, 0usize)));

    let notify_replaying = replaying.clone();
    let notify_counts = replay_counts.clone();
    let notify_events = events.clone();
    let permission_events = events.clone();
    let permission_parked = parked.clone();
    let read_events = events.clone();
    let write_events = events.clone();

    agent_client_protocol::Client
        .builder()
        .name("aether")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                let events = translate(notification.update);
                if notify_replaying.load(Ordering::Relaxed) {
                    // Counted so a real run can answer the question the spec does not: whether a
                    // replay carries tool calls, or only the messages its examples show.
                    let mut counts = notify_counts.lock().expect("replay counts");
                    for event in &events {
                        match event {
                            AgentEvent::Chunk { .. } => counts.0 += 1,
                            AgentEvent::ToolCall { .. } | AgentEvent::Diff { .. } => counts.1 += 1,
                            _ => {}
                        }
                    }
                    return Ok(());
                }
                for event in events {
                    let _ = notify_events.send(event);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let events = permission_events.clone();
                let parked = permission_parked.clone();
                let options: Vec<PermissionOption> = request
                    .options
                    .iter()
                    .map(|o| PermissionOption {
                        id: o.option_id.0.to_string(),
                        label: o.name.clone(),
                        kind: permission_kind(&o.kind),
                    })
                    .collect();
                // An empty option list is nothing the user could resolve, so it is answered
                // immediately rather than parked forever.
                if options.is_empty() {
                    return responder
                        .respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                }
                let (id, wait) = parked.park();
                let _ = events.send(AgentEvent::Permission {
                    request: id,
                    tool_call_id: Some(request.tool_call.tool_call_id.0.to_string()),
                    options,
                });
                // **Answered from a spawned task, never from here.** This callback runs on the
                // connection's dispatch loop, and a permission request waits on a person: parking
                // the loop on it would stop every other message — including the session updates an
                // agent keeps sending while it waits, and the response that ends the turn.
                connection.spawn(async move {
                    let chosen = wait.await.ok().flatten();
                    responder.respond(RequestPermissionResponse::new(match chosen {
                        Some(option) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(PermissionOptionId::new(option)),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    }))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReadTextFileRequest, responder, _connection| {
                let events = read_events.clone();
                {
                    let (tx, rx) = oneshot::channel();
                    let _ = events.send(AgentEvent::ReadFile {
                        path: request.path,
                        line: request.line,
                        limit: request.limit,
                        reply: tx,
                    });
                    match rx.await {
                        Ok(Ok(content)) => {
                            responder.respond(ReadTextFileResponse::new(content))
                        }
                        Ok(Err(message)) => responder.respond_with_internal_error(message),
                        Err(_) => responder.respond_with_internal_error("editor is shutting down"),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WriteTextFileRequest, responder, _connection| {
                let events = write_events.clone();
                {
                    let (tx, rx) = oneshot::channel();
                    let _ = events.send(AgentEvent::WriteFile {
                        path: request.path,
                        content: request.content,
                        reply: tx,
                    });
                    match rx.await {
                        Ok(Ok(())) => responder.respond(WriteTextFileResponse::new()),
                        Ok(Err(message)) => responder.respond_with_internal_error(message),
                        Err(_) => responder.respond_with_internal_error("editor is shutting down"),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, move |connection: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>| async move {
            let init = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(our_capabilities()),
                )
                .block_task()
                .await?;

            // Resume only where the agent says it can: `loadSession` absent or false means it does
            // not support loading, and asking anyway would fail the whole connection.
            let can_load = init.agent_capabilities.load_session;
            let (session, resumed) = match resume.filter(|_| can_load) {
                Some(id) => {
                    replaying.store(true, Ordering::Relaxed);
                    let id = agent_client_protocol::schema::v1::SessionId::new(id);
                    // The response comes back only once the entire replay has been streamed, so
                    // this await is exactly the window the gate above covers.
                    let loaded = connection
                        .send_request(LoadSessionRequest::new(id.clone(), cwd.clone()))
                        .block_task()
                        .await;
                    replaying.store(false, Ordering::Relaxed);
                    let (messages, tools) = *replay_counts.lock().expect("replay counts");
                    match loaded {
                        Ok(_) => {
                            tracing::info!(
                                messages,
                                tool_calls = tools,
                                "resumed an agent session; replay contents"
                            );
                            (id, true)
                        }
                        // A session the agent no longer has: carry on with a fresh one rather than
                        // failing the open, and let the caller say so.
                        Err(e) => {
                            tracing::info!(error = %e, "could not resume the agent session");
                            (
                                connection
                                    .send_request(NewSessionRequest::new(cwd))
                                    .block_task()
                                    .await?
                                    .session_id,
                                false,
                            )
                        }
                    }
                }
                None => (
                    connection
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id,
                    false,
                ),
            };

            let _ = events.send(AgentEvent::Ready {
                session: session.0.to_string(),
                resumed,
            });

            let mut turn: Option<_> = None;
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
                        // The editor dropped the handle: the view closed. Leaving the closure
                        // drops the transport, which kills the process group.
                        None => break,
                        Some(AgentCommand::Prompt(text)) => {
                            turn = Some(Box::pin(
                                connection
                                    .send_request(PromptRequest::new(
                                        session.clone(),
                                        vec![ContentBlock::Text(TextContent::new(text))],
                                    ))
                                    .block_task(),
                            ));
                        }
                        Some(AgentCommand::Cancel) => {
                            // The protocol requires a cancelling client to answer outstanding
                            // permission requests rather than leave the agent blocked on one.
                            parked.cancel_all();
                            connection.send_notification(
                                agent_client_protocol::schema::v1::CancelNotification::new(
                                    session.clone(),
                                ),
                            )?;
                        }
                        Some(AgentCommand::Respond { request, option }) => {
                            parked.answer(request, option);
                        }
                    },
                    result = async { turn.as_mut().expect("guarded").await }, if turn.is_some() => {
                        turn = None;
                        let reason = match result {
                            Ok(response) => stop_reason(&response.stop_reason),
                            Err(error) => StopReason::Failed { message: error.to_string() },
                        };
                        // A turn that ended cannot still be waiting on a question.
                        parked.cancel_all();
                        let _ = events.send(AgentEvent::TurnEnded(reason));
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
}

/// What we tell the agent we can do. `fs` is the whole point of being on protocol v1; `terminal`
/// is deliberately false — declaring a surface we have not built would have the agent route work
/// through it and hang.
fn our_capabilities() -> ClientCapabilities {
    ClientCapabilities::new().fs(FileSystemCapabilities::new()
        .read_text_file(true)
        .write_text_file(true))
}

/// One `session/update` becomes zero or more editor events. A tool call carrying content produces
/// several: the call itself, then a block per diff.
fn translate(update: SessionUpdate) -> Vec<AgentEvent> {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => chunk_event(ChunkKind::User, chunk),
        SessionUpdate::AgentMessageChunk(chunk) => chunk_event(ChunkKind::Agent, chunk),
        SessionUpdate::AgentThoughtChunk(chunk) => chunk_event(ChunkKind::Thought, chunk),
        SessionUpdate::ToolCall(call) => {
            let id = call.tool_call_id.0.to_string();
            let mut events = vec![AgentEvent::ToolCall {
                tool_call_id: id.clone(),
                title: Some(call.title),
                kind: Some(tool_kind(&call.kind)),
                status: Some(tool_status(&call.status)),
                locations: Some(call.locations.iter().map(location).collect()),
                text: None,
            }];
            events.extend(content_events(&id, &call.content));
            events
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let fields = update.fields;
            let mut events = vec![AgentEvent::ToolCall {
                tool_call_id: id.clone(),
                title: fields.title,
                kind: fields.kind.as_ref().map(tool_kind),
                status: fields.status.as_ref().map(tool_status),
                locations: fields
                    .locations
                    .as_ref()
                    .map(|l| l.iter().map(location).collect()),
                text: None,
            }];
            if let Some(content) = &fields.content {
                events.extend(content_events(&id, content));
            }
            events
        }
        SessionUpdate::Plan(plan) => vec![AgentEvent::Plan {
            entries: plan_entries(&plan),
        }],
        // Real protocol vocabulary this view does not present yet — modes, slash commands, token
        // usage. Named rather than folded into the wildcard so that what we are choosing to drop
        // is written down; `SessionUpdate` is `#[non_exhaustive]`, so the wildcard has to be here
        // regardless and cannot be relied on to flag a new variant.
        SessionUpdate::AvailableCommandsUpdate(_)
        | SessionUpdate::CurrentModeUpdate(_)
        | SessionUpdate::ConfigOptionUpdate(_)
        | SessionUpdate::SessionInfoUpdate(_)
        | SessionUpdate::UsageUpdate(_) => Vec::new(),
        _ => Vec::new(),
    }
}

fn chunk_event(
    kind: ChunkKind,
    chunk: agent_client_protocol::schema::v1::ContentChunk,
) -> Vec<AgentEvent> {
    let Some(text) = block_text(&chunk.content) else {
        return Vec::new();
    };
    vec![AgentEvent::Chunk {
        kind,
        message_id: chunk.message_id.map(|id| id.0.to_string()),
        text,
    }]
}

fn content_events(tool_call_id: &str, content: &[ToolCallContent]) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut diffs = 0usize;
    for item in content {
        match item {
            ToolCallContent::Content(c) => {
                if let Some(text) = block_text(&c.content) {
                    events.push(AgentEvent::ToolCall {
                        tool_call_id: tool_call_id.to_string(),
                        title: None,
                        kind: None,
                        status: None,
                        locations: None,
                        text: Some(text),
                    });
                }
            }
            ToolCallContent::Diff(d) => {
                events.push(AgentEvent::Diff {
                    tool_call_id: tool_call_id.to_string(),
                    index: diffs,
                    path: d.path.clone(),
                    old_text: d.old_text.clone(),
                    new_text: d.new_text.clone(),
                });
                diffs += 1;
            }
            // We declare no terminal capability, so an agent should not be sending these. Say so
            // in the transcript rather than dropping it silently: a blank where output should be
            // is the kind of thing that costs an hour.
            ToolCallContent::Terminal(t) => {
                events.push(AgentEvent::ToolCall {
                    tool_call_id: tool_call_id.to_string(),
                    title: None,
                    kind: None,
                    status: None,
                    locations: None,
                    text: Some(format!("[terminal {} — not shown]\n", t.terminal_id.0)),
                });
            }
            _ => {}
        }
    }
    events
}

/// The readable text of a content block. Images and audio have none; a resource link is its URI.
fn block_text(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(t) => Some(t.text.clone()),
        ContentBlock::ResourceLink(l) => Some(format!("{}\n", l.uri)),
        ContentBlock::Resource(_) | ContentBlock::Image(_) | ContentBlock::Audio(_) => None,
        _ => None,
    }
}

/// A plan's entries with their states.
///
/// `PlanEntryStatus` has no failure case — a plan item is pending, running or done — so the
/// mapping into [`ToolStatus`] is total and never produces `Failed`. Sharing the type is what lets
/// one glyph vocabulary cover both a tool call and a plan item.
fn plan_entries(plan: &agent_client_protocol::schema::v1::Plan) -> Vec<(String, ToolStatus)> {
    use agent_client_protocol::schema::v1::PlanEntryStatus as S;
    plan.entries
        .iter()
        .map(|e| {
            let status = match e.status {
                S::Pending => ToolStatus::Pending,
                S::InProgress => ToolStatus::InProgress,
                S::Completed => ToolStatus::Completed,
                _ => ToolStatus::Pending,
            };
            (e.content.clone(), status)
        })
        .collect()
}

fn location(l: &agent_client_protocol::schema::v1::ToolCallLocation) -> Location {
    Location {
        path: l.path.clone(),
        // 1-based on the wire, 0-based everywhere in this editor. Converted once, here.
        line: l.line.map(|n| n.saturating_sub(1)),
    }
}

fn tool_kind(kind: &AcpToolKind) -> ToolKind {
    match kind {
        AcpToolKind::Read => ToolKind::Read,
        AcpToolKind::Edit => ToolKind::Edit,
        AcpToolKind::Delete => ToolKind::Delete,
        AcpToolKind::Move => ToolKind::Move,
        AcpToolKind::Search => ToolKind::Search,
        AcpToolKind::Execute => ToolKind::Execute,
        AcpToolKind::Think => ToolKind::Think,
        AcpToolKind::Fetch => ToolKind::Fetch,
        AcpToolKind::SwitchMode => ToolKind::SwitchMode,
        AcpToolKind::Other => ToolKind::Other,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: &AcpToolCallStatus) -> ToolStatus {
    match status {
        AcpToolCallStatus::Pending => ToolStatus::Pending,
        AcpToolCallStatus::InProgress => ToolStatus::InProgress,
        AcpToolCallStatus::Completed => ToolStatus::Completed,
        AcpToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

fn permission_kind(
    kind: &agent_client_protocol::schema::v1::PermissionOptionKind,
) -> PermissionKind {
    use agent_client_protocol::schema::v1::PermissionOptionKind as K;
    match kind {
        K::AllowOnce => PermissionKind::AllowOnce,
        K::AllowAlways => PermissionKind::AllowAlways,
        K::RejectOnce => PermissionKind::RejectOnce,
        K::RejectAlways => PermissionKind::RejectAlways,
        _ => PermissionKind::Other,
    }
}

fn stop_reason(reason: &AcpStopReason) -> StopReason {
    match reason {
        AcpStopReason::EndTurn => StopReason::EndTurn,
        AcpStopReason::MaxTokens => StopReason::MaxTokens,
        AcpStopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        AcpStopReason::Refusal => StopReason::Refusal,
        AcpStopReason::Cancelled => StopReason::Cancelled,
        other => StopReason::Other {
            reason: format!("{other:?}"),
        },
    }
}
