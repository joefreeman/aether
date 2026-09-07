//! An **in-process fake ACP agent** for tests.
//!
//! Speaks real protocol v1 over the SDK's in-memory [`Channel`] transport, so Aether's agent
//! handling is exercised deterministically — no subprocess, no `npx`, no network, no API key, and
//! nothing billable. The same idea as [`crate::lsp::dummy`], and for the same reason: an
//! integration suite that needed a real agent would be `#[ignore]`d and would rot.
//!
//! A test writes a [`Script`] — what the agent does when it is prompted — and gets back a
//! transport to hand to [`super::connection::connect`]. Everything the view can be asked to render
//! has a step here, including the two requests that come back *at* the editor.

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, Diff, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionId,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
    PromptResponse, ReadTextFileRequest, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, WriteTextFileRequest,
};
use agent_client_protocol::Channel;
use std::sync::{Arc, Mutex};

/// What the dummy agent does when it is prompted. Steps run in order, then the turn ends with
/// [`Script::stop_reason`].
#[derive(Debug, Clone, Default)]
pub struct Script {
    pub steps: Vec<Step>,
    pub stop_reason: Option<StopReason>,
}

/// One thing the agent does during a turn.
#[derive(Debug, Clone)]
pub enum Step {
    /// Stream prose. `message_id: None` is the case an agent that does not identify its messages
    /// produces, which is the fallback path the view has to get right.
    Say {
        message_id: Option<&'static str>,
        text: &'static str,
    },
    /// Think out loud.
    Think { text: &'static str },
    /// Announce a tool call.
    Call {
        id: &'static str,
        title: &'static str,
        kind: ToolKind,
    },
    /// Update one — only the fields given, which is how the real protocol sends them.
    Update {
        id: &'static str,
        status: Option<ToolCallStatus>,
        text: Option<&'static str>,
    },
    /// Attach a diff to a tool call.
    Propose {
        id: &'static str,
        path: &'static str,
        old_text: Option<&'static str>,
        new_text: &'static str,
    },
    /// Ask the user for permission, and block until the answer arrives. The chosen option id is
    /// recorded in [`Transcript::answers`].
    Ask {
        id: &'static str,
        options: &'static [(&'static str, PermissionOptionKind)],
    },
    /// Publish a plan, with each entry's state.
    Plan {
        entries: &'static [(&'static str, PlanEntryStatus)],
    },
    /// Read a file *through the editor*, recording what came back in [`Transcript::reads`].
    Read { path: &'static str },
    /// Write a file through the editor.
    Write {
        path: &'static str,
        content: &'static str,
    },
}

/// What the dummy saw and was told, for a test to assert on.
#[derive(Debug, Default)]
pub struct Transcript {
    /// Every prompt the agent was sent, in order.
    pub prompts: Vec<String>,
    /// What `fs/read_text_file` returned, in order — the assertion point for "the agent reads
    /// unsaved buffer text".
    pub reads: Vec<Result<String, String>>,
    /// Whether each `fs/write_text_file` succeeded.
    pub writes: Vec<Result<(), String>>,
    /// The option ids the user chose, in order. `None` is a cancelled request.
    pub answers: Vec<Option<String>>,
    /// Set when the agent was asked to cancel.
    pub cancelled: bool,
}

/// A running dummy agent: the transport to connect to, and what it recorded.
pub struct Dummy {
    pub transport: Channel,
    pub transcript: Arc<Mutex<Transcript>>,
}

/// Start a dummy agent running `script`, and hand back the transport its client should use.
pub fn start(script: Script) -> Dummy {
    let (ours, theirs) = Channel::duplex();
    let transcript = Arc::new(Mutex::new(Transcript::default()));
    let recorded = transcript.clone();

    tokio::spawn(async move {
        let session = SessionId::new("dummy-session");
        let prompt_session = session.clone();
        let prompt_script = script.clone();
        let prompt_transcript = recorded.clone();

        let _ = agent_client_protocol::Agent
            .builder()
            .name("dummy-agent")
            .on_receive_request(
                async move |request: InitializeRequest, responder, _connection| {
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: NewSessionRequest, responder, _connection| {
                    responder.respond(NewSessionResponse::new(session.clone()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection| {
                    let script = prompt_script.clone();
                    let transcript = prompt_transcript.clone();
                    let session = prompt_session.clone();
                    transcript.lock().expect("transcript").prompts.push(
                        request
                            .prompt
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    );
                    // The script runs in a task of its own, and the response is sent from there.
                    // A step that asks for permission sends a request and waits for the answer,
                    // and doing that from this callback would park the dispatch loop that has to
                    // deliver it — the deadlock a real agent would hit too.
                    let spawned = connection.clone();
                    connection.spawn(async move {
                        run_script(&script, &session, &spawned, &transcript).await;
                        responder.respond(PromptResponse::new(
                            script.stop_reason.unwrap_or(StopReason::EndTurn),
                        ))
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(theirs)
            .await;
    });

    Dummy {
        transport: ours,
        transcript,
    }
}

async fn run_script(
    script: &Script,
    session: &SessionId,
    connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
    transcript: &Arc<Mutex<Transcript>>,
) {
    for step in &script.steps {
        match step {
            Step::Say { message_id, text } => {
                let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(*text)));
                if let Some(id) = message_id {
                    chunk =
                        chunk.message_id(agent_client_protocol::schema::v1::MessageId::new(*id));
                }
                notify(connection, session, SessionUpdate::AgentMessageChunk(chunk));
            }
            Step::Think { text } => {
                notify(
                    connection,
                    session,
                    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(*text),
                    ))),
                );
            }
            Step::Call { id, title, kind } => {
                notify(
                    connection,
                    session,
                    SessionUpdate::ToolCall(
                        ToolCall::new(ToolCallId::new(*id), *title)
                            .kind(*kind)
                            .status(ToolCallStatus::InProgress),
                    ),
                );
            }
            Step::Update { id, status, text } => {
                let mut fields = ToolCallUpdateFields::default();
                fields.status = *status;
                if let Some(text) = text {
                    fields.content = Some(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(*text),
                    ))]);
                }
                notify(
                    connection,
                    session,
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(*id),
                        fields,
                    )),
                );
            }
            Step::Propose {
                id,
                path,
                old_text,
                new_text,
            } => {
                let mut diff = Diff::new(std::path::PathBuf::from(*path), *new_text);
                if let Some(old) = old_text {
                    diff = diff.old_text(old.to_string());
                }
                let mut fields = ToolCallUpdateFields::default();
                fields.content = Some(vec![ToolCallContent::Diff(diff)]);
                notify(
                    connection,
                    session,
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(*id),
                        fields,
                    )),
                );
            }
            Step::Ask { id, options } => {
                let options: Vec<PermissionOption> = options
                    .iter()
                    .map(|(name, kind)| {
                        PermissionOption::new(PermissionOptionId::new(*name), *name, *kind)
                    })
                    .collect();
                let request = RequestPermissionRequest::new(
                    session.clone(),
                    ToolCallUpdate::new(ToolCallId::new(*id), ToolCallUpdateFields::default()),
                    options,
                );
                let answered = connection.send_request(request).block_task().await;
                let chosen = match answered {
                    Ok(response) => match response.outcome {
                        agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(
                            s,
                        ) => Some(s.option_id.0.to_string()),
                        _ => None,
                    },
                    Err(_) => None,
                };
                transcript.lock().expect("transcript").answers.push(chosen);
            }
            Step::Plan { entries } => {
                notify(
                    connection,
                    session,
                    SessionUpdate::Plan(Plan::new(
                        entries
                            .iter()
                            .map(|(content, status)| {
                                PlanEntry::new(*content, PlanEntryPriority::Medium, status.clone())
                            })
                            .collect::<Vec<_>>(),
                    )),
                );
            }
            Step::Read { path } => {
                let result = connection
                    .send_request(ReadTextFileRequest::new(
                        session.clone(),
                        std::path::PathBuf::from(*path),
                    ))
                    .block_task()
                    .await
                    .map(|r| r.content)
                    .map_err(|e| e.to_string());
                transcript.lock().expect("transcript").reads.push(result);
            }
            Step::Write { path, content } => {
                let result = connection
                    .send_request(WriteTextFileRequest::new(
                        session.clone(),
                        std::path::PathBuf::from(*path),
                        content.to_string(),
                    ))
                    .block_task()
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                transcript.lock().expect("transcript").writes.push(result);
            }
        }
    }
}

fn notify(
    connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
    session: &SessionId,
    update: SessionUpdate,
) {
    let _ = connection.send_notification(SessionNotification::new(session.clone(), update));
}

/// Options a permission request usually carries, for the tests that do not care about the wording.
pub const ALLOW_OR_REJECT: &[(&str, PermissionOptionKind)] = &[
    ("allow", PermissionOptionKind::AllowOnce),
    ("reject", PermissionOptionKind::RejectOnce),
];
