//! `agent/*` — opening an agent view, prompting it, stopping it, and answering it.
//!
//! The prompt text never travels: it is the input document's, which the server already holds, so
//! `agent/prompt` names only the view. What comes back travels as ordinary buffer content — every
//! block is a document and its text is `view/lines_changed` like any other change — and the only
//! thing this scope pushes of its own is *whether a turn is running*, which is what the status bar
//! shows and what `agent/cancel` acts on.
//!
//! The agent talks back through [`crate::agent::connection`]; [`pump`] is where its events become
//! blocks. Two of those events are requests from the agent for editor state, and answering them
//! from the live buffer rather than the disk is the reason this view is on protocol v1 at all.

use super::*;
use aether_protocol::agent::{
    AgentCancelParams, AgentCancelResult, AgentOpenParams, AgentOpenResult, AgentPromptParams,
    AgentPromptResult, AgentRespondParams, AgentRespondResult, AgentTurnChanged,
    AgentTurnChangedParams, Answer, StopReason, TurnState,
};
use aether_protocol::ViewId;
use std::path::Path;

use crate::agent::{
    AgentCommand, AgentEvent, BlockKey, BlockKind, ChunkKind, Conversation, Diff,
    PendingPermission, ToolCall, ToolKind, ToolStatus,
};

/// Most characters one conversation may hold across all its blocks before further output is
/// dropped. An agent that has decided to print a binary file should not be able to exhaust memory,
/// and nobody reads past this.
const MAX_CONVERSATION_CHARS: usize = 8 * 1024 * 1024;

// ---- agent/open --------------------------------------------------------------------------------

pub async fn agent_open(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: AgentOpenParams,
) -> Result<AgentOpenResult, RpcError> {
    let client_id = ctx.client_id;

    // Pressing the key *inside* an agent view means "another one"; anywhere else it means "the one
    // I can type at". Decided here because only the server knows what `from_view` is.
    let from_agent_view = {
        let s = state.lock().await;
        params.from_view.is_some_and(|v| {
            s.try_presenting_buffer(v)
                .and_then(|b| s.try_doc_of(b))
                .and_then(|d| d.conversation())
                .is_some()
        })
    };
    if !from_agent_view {
        let existing = {
            let s = state.lock().await;
            idle_conversation(&s, client_id)
        };
        if let Some(view_buffer) = existing {
            return present(state, ctx, view_buffer).await;
        }
    }

    let spec = resolve_agent(params.agent.as_deref())?;
    let view_buffer = mint_conversation(state, client_id, spec, None).await?;
    // A fresh conversation connects at once: you opened it to talk to something.
    ensure_agent(state, view_buffer).await?;
    present(state, ctx, view_buffer).await
}

/// The row of the agent table this open names, or the first one on `PATH`.
fn resolve_agent(id: Option<&str>) -> Result<&'static crate::agent::AgentSpec, RpcError> {
    match id {
        Some(id) => crate::agent::config::by_id(id)
            .ok_or_else(|| RpcError::internal(format!("no agent called {id}"))),
        None => crate::agent::config::default_agent().ok_or_else(|| {
            RpcError::new(ErrorCode::AGENT_UNAVAILABLE, "no ACP agent found on PATH")
        }),
    }
}

/// Build a conversation's documents and view, optionally from a snapshot, and open it.
///
/// Shared by a fresh open and a restore, so the two cannot drift in how a conversation is put
/// together — the shell's `mint_shell` for the same reason. Connects to nothing: see
/// [`ensure_agent`].
async fn mint_conversation(
    state: &SharedState,
    client_id: ClientId,
    spec: &'static crate::agent::AgentSpec,
    seed: Option<(u32, crate::agent::AgentSnapshot)>,
) -> Result<BufferId, RpcError> {
    // Where the agent runs: the project holding the focused file, else the workspace's first root
    // — the shell view's rule, and for the same reason. A restored conversation keeps the
    // directory it had, so its record still refers to where the work happened.
    let cwd = match &seed {
        Some((_, snap)) => snap.cwd.clone(),
        None => {
            let s = state.lock().await;
            crate::handlers::shell::shell_cwd(&s, client_id)
        }
    };

    let view_buffer = {
        let mut s = state.lock().await;
        let workspace = s.active_workspace_or_err(client_id)?.id.clone();
        let number = match &seed {
            Some((number, _)) => *number,
            None => next_agent_number(&s, &workspace),
        };
        let title = format!("Agent {number}");

        // The input first: the conversation names it, so it has to exist to be named. No language:
        // what is typed here is prose for an agent, not anyone's script.
        let input = s.allocate_buffer_id();
        let typed = seed.as_ref().map(|(_, snap)| snap.input.clone());
        s.insert_buffer_with_document(input, None, false, |id| {
            let mut doc = Document::field(id, None);
            if let Some(typed) = typed.filter(|t| !t.is_empty()) {
                doc.restore_unsaved(&typed);
            }
            doc
        });
        s.buffer_workspaces.insert(input, workspace.clone());

        let view_buffer = s.allocate_buffer_id();
        let doc_id = s.allocate_document_id();
        let mut conversation = Conversation::new(input, cwd, spec, title.clone());
        if let Some((_, snap)) = &seed {
            conversation.session = snap.session.clone();
        }
        let doc = Document::virtual_content(
            doc_id,
            crate::state::VirtualSource {
                target: crate::state::VirtualTarget::agent(&workspace, number),
                title,
            },
            // The view's own document holds no text: every block has one. It exists to be the
            // thing the view presents and to carry the conversation.
            String::new(),
            None,
            Some(Generated::Agent(conversation)),
            false,
        );
        s.documents.insert(doc_id, doc);
        s.buffers.insert(
            view_buffer,
            Buffer {
                id: view_buffer,
                document: doc_id,
                scratch_number: None,
            },
        );
        s.buffer_workspaces.insert(view_buffer, workspace.clone());
        // Not transient: a conversation is somewhere you are working, and a transient one would
        // close itself the moment you looked at a file, taking a running turn with it.
        s.open_view(view_buffer);
        s.touch_mru(view_buffer);
        view_buffer
    };

    // The blocks, each in a document of its own, exactly as a live one builds them.
    if let Some((_, snap)) = seed {
        for block in snap.blocks {
            let kind = block.kind.restore();
            let language = block.kind.language();
            if let Some(buffer) = new_block(state, view_buffer, None, kind, language).await {
                let mut s = state.lock().await;
                s.set_block_text(view_buffer, buffer, &block.text);
            }
        }
    }

    let pushes = {
        let mut s = state.lock().await;
        refresh_view_pickers(&mut s)
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    // Recorded in the session at once, as every other open is — and as a shell is: the snapshot is
    // what brings the conversation back, and the session entry is what says there is one to bring.
    let workspace = {
        let s = state.lock().await;
        s.buffer_workspaces.get(&view_buffer).cloned()
    };
    if let Some(workspace) = workspace {
        persist_workspace_session(state, &workspace, false).await;
    }
    Ok(view_buffer)
}

/// Connect this conversation to its agent, if it has none.
///
/// A conversation restored from disk has no agent behind it: opening one is reading a record, and
/// an agent is a subprocess that costs real money to run. This is where one starts — on the first
/// prompt — and where a session the agent still remembers is asked for back.
async fn ensure_agent(state: &SharedState, view_buffer: BufferId) -> Result<(), RpcError> {
    let (needed, spec, cwd, session) = {
        let s = state.lock().await;
        let Some(c) = s.try_doc_of(view_buffer).and_then(|d| d.conversation()) else {
            return Err(RpcError::internal("not an agent view"));
        };
        (
            c.handle.is_none(),
            c.agent,
            c.cwd.clone(),
            c.session.clone(),
        )
    };
    if !needed {
        return Ok(());
    }

    // Resolved outside the lock: it may run the user's login shell.
    let env = crate::handlers::shell::shell_environment(&cwd).await;

    // Where the agent comes from — a real subprocess, an in-process dummy, or nothing at all on a
    // test server that installed no dummy. The refusal is the whole point of the enum: see
    // [`crate::state::AgentLauncher`].
    enum Launch {
        Subprocess,
        Dummy(agent_client_protocol::Channel),
        Refuse,
    }
    let launch = {
        let s = state.lock().await;
        match &s.agent_launcher {
            crate::state::AgentLauncher::Subprocess => Launch::Subprocess,
            crate::state::AgentLauncher::Dummy(make) => Launch::Dummy(make()),
            crate::state::AgentLauncher::Refuse => Launch::Refuse,
        }
    };
    let (handle, events) = match launch {
        Launch::Subprocess => {
            crate::agent::connection::spawn(spec, cwd.clone(), env, session.clone())
        }
        Launch::Dummy(channel) => crate::agent::connection::connect(channel, cwd, session.clone()),
        Launch::Refuse => {
            return Err(RpcError::new(
                ErrorCode::AGENT_UNAVAILABLE,
                "this server does not launch agents (test server with no dummy installed)",
            ))
        }
    };

    let view_id = {
        let mut s = state.lock().await;
        let view_id = s
            .view_presenting(view_buffer)
            .ok_or_else(|| RpcError::internal("the agent view was not opened"))?;
        if let Some(c) = conversation_mut(&mut s, view_buffer) {
            c.handle = Some(handle);
            // The session we are asking to come back to, if there was one. Cleared until the
            // agent confirms: a session id we could not load is worse than none, because a
            // snapshot written from it would claim a context the agent does not have.
            c.resuming = session;
        }
        view_id
    };

    let pump_state = state.clone();
    tokio::spawn(async move {
        pump(pump_state, view_buffer, view_id, events).await;
    });
    Ok(())
}

/// A dormant conversation, opened:/// A dormant conversation, opened: read its snapshot back and rebuild what was on screen.
///
/// **Launches nothing.** An agent is a subprocess that costs real money to run, so opening five
/// restored conversations at startup must not start five agents — the conversation comes back as a
/// record, and the agent starts when you prompt it ([`agent_prompt`]). That is the one place this
/// deliberately differs from a shell, whose restore is inert either way.
pub async fn open_restored_agent(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    number: u32,
) -> Result<ViewOpenResult, RpcError> {
    let client_id = ctx.client_id;
    let snapshot = {
        let s = state.lock().await;
        let workspace = s.active_workspace_or_err(client_id)?.id.clone();
        let path = s
            .backups_path
            .as_deref()
            .map(|root| crate::backup::agent_backup_path(root, &workspace, number));
        path.and_then(|p| crate::backup::read(&p))
            .and_then(|(json, _)| serde_json::from_str::<crate::agent::AgentSnapshot>(&json).ok())
            .ok_or_else(|| RpcError::internal("that conversation's snapshot has gone"))?
    };

    // The agent the conversation was had with. An id this build no longer knows leaves the record
    // readable and falls back to whichever agent is available for a follow-up.
    let spec = crate::agent::config::by_id(&snapshot.agent)
        .or_else(crate::agent::config::default_agent)
        .ok_or_else(|| RpcError::new(ErrorCode::AGENT_UNAVAILABLE, "no ACP agent found on PATH"))?;
    let view_buffer = mint_conversation(state, client_id, spec, Some((number, snapshot))).await?;
    let (opened, _input) = {
        let input_buffer = {
            let s = state.lock().await;
            s.try_doc_of(view_buffer)
                .and_then(|d| d.conversation())
                .map(|c| c.input)
                .ok_or_else(|| RpcError::internal("a restored conversation has lost its input"))?
        };
        crate::handlers::shell::land_in_field(state, ctx, view_buffer, input_buffer).await?
    };
    Ok(opened)
}

/// Adopt a conversation's view for this client and land the cursor in its input — the same
/// landing a shell gets, through the same function.
async fn present(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    view_buffer: BufferId,
) -> Result<AgentOpenResult, RpcError> {
    let input_buffer = {
        let s = state.lock().await;
        s.try_doc_of(view_buffer)
            .and_then(|d| d.conversation())
            .map(|c| c.input)
            .ok_or_else(|| RpcError::internal("an agent view has lost its input"))?
    };
    let (opened, input) =
        crate::handlers::shell::land_in_field(state, ctx, view_buffer, input_buffer).await?;
    Ok(AgentOpenResult { opened, input })
}

/// The conversation `Space n n` lands in when it is not making a new one: the focused one if it is
/// idle, else the workspace's most recently used idle one.
fn idle_conversation(s: &ServerState, client_id: ClientId) -> Option<BufferId> {
    let workspace = s.active_workspace(client_id).map(|w| w.id.clone())?;
    let mut candidates: Vec<(u64, BufferId)> = s
        .buffer_workspaces
        .iter()
        .filter(|(_, w)| **w == workspace)
        .filter_map(|(id, _)| {
            let c = s.try_doc_of(*id)?.conversation()?;
            (!c.is_running()).then(|| {
                (
                    s.view_presenting(*id)
                        .and_then(|v| s.try_view(v))
                        .map_or(0, |v| v.last_used),
                    *id,
                )
            })
        })
        .collect();
    candidates.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    candidates.first().map(|(_, id)| *id)
}

fn next_agent_number(s: &ServerState, workspace: &str) -> u32 {
    let used: std::collections::HashSet<u32> = s
        .buffer_workspaces
        .iter()
        .filter(|(_, w)| w.as_str() == workspace)
        .filter_map(|(id, _)| s.try_doc_of(*id))
        .filter_map(|d| match d.virtual_source.as_ref().map(|v| &v.target) {
            Some(crate::state::VirtualTarget::Agent { number, .. }) => Some(*number),
            _ => None,
        })
        .collect();
    (1..).find(|n| !used.contains(n)).unwrap_or(1)
}

// ---- agent/prompt ------------------------------------------------------------------------------

pub async fn agent_prompt(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: AgentPromptParams,
) -> Result<AgentPromptResult, RpcError> {
    let client_id = ctx.client_id;
    // A conversation restored from disk has no agent until now: opening one was reading a record.
    // Started here, before the input is touched, so a failure to launch leaves what you typed
    // exactly where it was.
    let view_buffer_for_agent = {
        let s = state.lock().await;
        s.try_presenting_buffer(params.view_id)
    };
    if let Some(view_buffer) = view_buffer_for_agent {
        ensure_agent(state, view_buffer).await?;
    }
    let (text, view_buffer) = {
        let mut s = state.lock().await;
        let view_buffer = s
            .try_presenting_buffer(params.view_id)
            .ok_or_else(|| RpcError::internal("no such view"))?;
        let c = s
            .try_doc_of(view_buffer)
            .and_then(|d| d.conversation())
            .ok_or_else(|| RpcError::internal("not an agent view"))?;

        // Refused *before* the input is touched: typing ahead of a running turn is a reasonable
        // thing to do, so the text stays exactly as it was.
        if c.is_running() {
            return Err(RpcError::new(
                ErrorCode::AGENT_BUSY,
                format!("{} is working — Space n c stops it", c.title),
            ));
        }

        let input = c.input;
        let text = s.doc_of(input).text.to_string();
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(AgentPromptResult { sent: false });
        }

        // Cleared through the ordinary edit path, not by swapping the rope: revision-guarded
        // pushes and every client's cursor stay coherent that way.
        let cursors = document_cursor_snapshot(&s, input);
        let end = s.doc_of(input).text.len_chars();
        s.editable_doc(input)?
            .apply_edit(0, end, "", EditKindTag::Delete, cursors);
        (text, view_buffer)
    };

    // The prompt becomes a block of its own before the agent says anything, so the conversation
    // reads as a conversation rather than as a stream of answers.
    push_block(
        state,
        view_buffer,
        None,
        BlockKind::UserMessage,
        Some("markdown"),
        &format!("{text}\n"),
    )
    .await;

    let sent = {
        let mut s = state.lock().await;
        let Some(c) = s
            .try_doc_of_mut(view_buffer)
            .and_then(|d| d.generated.as_mut())
            .and_then(crate::state::Generated::conversation_mut)
        else {
            return Err(RpcError::internal("not an agent view"));
        };
        c.turn = Some(crate::agent::Turn {
            prompt: text.clone(),
            activity: None,
        });
        c.generation += 1;
        c.handle
            .as_ref()
            .is_some_and(|h| h.send(AgentCommand::Prompt(text.clone())))
    };
    if !sent {
        clear_turn(
            state,
            view_buffer,
            params.view_id,
            StopReason::Failed {
                message: "the agent is not running".into(),
            },
        )
        .await;
        return Err(RpcError::new(
            ErrorCode::AGENT_UNAVAILABLE,
            "the agent is not running",
        ));
    }

    record_history(state, client_id, &text).await;
    refresh(state, view_buffer).await;
    push_turn_changed(
        state,
        params.view_id,
        Some(TurnState {
            running: true,
            activity: None,
            stop_reason: None,
        }),
    )
    .await;
    Ok(AgentPromptResult { sent: true })
}

async fn record_history(state: &SharedState, client_id: ClientId, text: &str) {
    let mut s = state.lock().await;
    if let Some(workspace) = s.active_workspace(client_id).map(|w| w.id.clone()) {
        if s.history.record(
            &workspace,
            aether_protocol::history::HistoryKind::Agent,
            aether_protocol::history::HistoryEntry::bare(text.to_string()),
        ) {
            s.history_dirty = true;
        }
    }
}

// ---- agent/cancel ------------------------------------------------------------------------------

pub async fn agent_cancel(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: AgentCancelParams,
) -> Result<AgentCancelResult, RpcError> {
    let cancelled = {
        let s = state.lock().await;
        let Some(view_buffer) = s.try_presenting_buffer(params.view_id) else {
            return Ok(AgentCancelResult { cancelled: false });
        };
        match s.try_doc_of(view_buffer).and_then(|d| d.conversation()) {
            // The turn ended between the keystroke and this arriving: success, not an error.
            Some(c) if !c.is_running() => false,
            Some(c) => c
                .handle
                .as_ref()
                .is_some_and(|h| h.send(AgentCommand::Cancel)),
            None => false,
        }
    };
    Ok(AgentCancelResult { cancelled })
}

// ---- agent/respond -----------------------------------------------------------------------------

pub async fn agent_respond(
    state: &SharedState,
    _ctx: &mut ConnectionCtx,
    params: AgentRespondParams,
) -> Result<AgentRespondResult, RpcError> {
    let view_buffer = {
        let s = state.lock().await;
        s.try_presenting_buffer(params.view_id)
            .ok_or_else(|| RpcError::internal("no such view"))?
    };

    let answered = {
        let mut s = state.lock().await;
        let Some(c) = s
            .try_doc_of_mut(view_buffer)
            .and_then(|d| d.generated.as_mut())
            .and_then(crate::state::Generated::conversation_mut)
        else {
            return Err(RpcError::internal("not an agent view"));
        };
        // The block named, or the one the conversation is blocked on — at most one can be, since
        // the agent is waiting for the answer.
        let index = match params.block {
            Some(id) => c.find_by_id(id),
            None => c.blocks.iter().position(
                |b| matches!(&b.kind, BlockKind::ToolCall(tc) if tc.permission.is_some()),
            ),
        };
        let Some(index) = index else {
            return Ok(AgentRespondResult { answered: false });
        };
        let BlockKind::ToolCall(tc) = &mut c.blocks[index].kind else {
            return Ok(AgentRespondResult { answered: false });
        };
        // Taken, not read: the request is answered exactly once, so a second press on a block
        // whose question has already gone finds nothing rather than answering it twice.
        let Some(pending) = tc.permission.take() else {
            return Ok(AgentRespondResult { answered: false });
        };
        // The agent's own options decide what allowing and declining *are*; we only pick which
        // kind. An agent that offered no option of the asked-for kind gets a cancel, which is the
        // honest answer to "allow" when nothing on offer allows anything.
        let option = match &params.answer {
            Answer::Allow => pending.accept().map(|o| o.id.clone()),
            Answer::Decline => pending.reject().map(|o| o.id.clone()),
            Answer::Cancel => None,
            Answer::Option { id } => Some(id.clone()),
        };
        c.generation += 1;
        c.handle.as_ref().is_some_and(|h| {
            h.send(AgentCommand::Respond {
                request: pending.request,
                option,
            })
        })
    };

    refresh(state, view_buffer).await;
    Ok(AgentRespondResult { answered })
}

// ---- the pump ----------------------------------------------------------------------------------

/// Turn one conversation's stream of agent events into blocks, for as long as the agent lives.
///
/// Ends when the connection closes or the view goes — the conversation owns the handle, so
/// dropping the view drops the sender, which ends the connection task, which ends this.
async fn pump(
    state: SharedState,
    view_buffer: BufferId,
    view_id: ViewId,
    mut events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
) {
    while let Some(event) = events.recv().await {
        // The view closed under us: nothing left to write into.
        {
            let s = state.lock().await;
            if s.try_doc_of(view_buffer)
                .and_then(|d| d.conversation())
                .is_none()
            {
                return;
            }
        }
        apply(&state, view_buffer, view_id, event).await;
    }
}

async fn apply(state: &SharedState, view_buffer: BufferId, view_id: ViewId, event: AgentEvent) {
    match event {
        AgentEvent::Ready { session, resumed } => {
            // A conversation we restored but could not resume is showing a record this agent has
            // never seen. Saying so is the whole point: without it you would ask a follow-up that
            // assumes context nobody holds.
            let asked = {
                let mut s = state.lock().await;
                let Some(c) = conversation_mut(&mut s, view_buffer) else {
                    return;
                };
                c.session = Some(session);
                let asked = c.resuming.take();
                c.generation += 1;
                asked
            };
            if asked.is_some() && !resumed {
                push_block(
                    state,
                    view_buffer,
                    None,
                    BlockKind::AgentThought,
                    Some("markdown"),
                    "*A new session: the turns above are not in this agent's context.*\n",
                )
                .await;
            }
            refresh(state, view_buffer).await;
        }

        AgentEvent::Chunk {
            kind,
            message_id,
            text,
        } => {
            let block_kind = match kind {
                ChunkKind::User => BlockKind::UserMessage,
                ChunkKind::Agent => BlockKind::AgentMessage,
                ChunkKind::Thought => BlockKind::AgentThought,
            };
            let key = message_id.map(BlockKey::Message);
            append_to_block(state, view_buffer, key, block_kind, Some("markdown"), &text).await;
        }

        AgentEvent::ToolCall {
            tool_call_id,
            title,
            kind,
            status,
            locations,
            text,
        } => {
            let key = BlockKey::ToolCall(tool_call_id.clone());
            let created = {
                let mut s = state.lock().await;
                let Some(c) = conversation_mut(&mut s, view_buffer) else {
                    return;
                };
                match c.find(&key) {
                    // **Merge**, never replace: an update carries only the fields that changed, so
                    // assigning the whole struct would blank a title the agent set earlier.
                    Some(index) => {
                        if let BlockKind::ToolCall(tc) = &mut c.blocks[index].kind {
                            if let Some(title) = title.clone() {
                                tc.title = title;
                            }
                            if let Some(kind) = kind {
                                tc.kind = kind;
                            }
                            if let Some(status) = status {
                                tc.status = status;
                            }
                            if let Some(locations) = locations.clone() {
                                tc.locations = locations;
                            }
                            // What the status bar says while this is the live call.
                            let activity = tc.title.clone();
                            let running = matches!(tc.status, ToolStatus::InProgress);
                            if let Some(turn) = &mut c.turn {
                                if running {
                                    turn.activity = Some(activity);
                                }
                            }
                        }
                        c.generation += 1;
                        false
                    }
                    None => true,
                }
            };
            if created {
                let call = ToolCall {
                    tool_call_id,
                    title: title.unwrap_or_else(|| "working".into()),
                    kind: kind.unwrap_or(ToolKind::Other),
                    status: status.unwrap_or(ToolStatus::Pending),
                    locations: locations.unwrap_or_default(),
                    permission: None,
                };
                new_block(
                    state,
                    view_buffer,
                    Some(key.clone()),
                    BlockKind::ToolCall(call),
                    None,
                )
                .await;
            }
            // A tool call's own output appends to its own block, which is what makes an update
            // to a call that is no longer the last one an append rather than a splice.
            if let Some(text) = text {
                append_to_key(state, view_buffer, &key, &text).await;
                push_lines_changed(state, view_buffer).await;
            }
            refresh(state, view_buffer).await;
            push_turn_state(state, view_buffer, view_id).await;
        }

        AgentEvent::Diff {
            tool_call_id,
            index,
            path,
            old_text,
            new_text,
        } => {
            let key = BlockKey::Diff(tool_call_id, index);
            // v1 sends the two texts; the patch machinery wants a patch. `git2` will diff two
            // buffers for us, so this costs no new dependency and the result classifies itself at
            // generation exactly as `git/show`'s does.
            let patch = render_patch(&path, old_text.as_deref(), &new_text);
            let existing = {
                let s = state.lock().await;
                s.try_doc_of(view_buffer)
                    .and_then(|d| d.conversation())
                    .and_then(|c| c.find(&key))
                    .map(|i| {
                        s.try_doc_of(view_buffer)
                            .and_then(|d| d.conversation())
                            .map(|c| c.blocks[i].buffer)
                    })
            };
            match existing.flatten() {
                // A re-sent content list updates the diff it already produced rather than
                // appending a second copy of it.
                Some(buffer) => {
                    let mut s = state.lock().await;
                    s.set_block_text(view_buffer, buffer, &patch);
                }
                None => {
                    let diff = Diff {
                        path: path.clone(),
                        is_new: old_text.is_none(),
                    };
                    let block = new_block(
                        state,
                        view_buffer,
                        Some(key),
                        BlockKind::Diff(diff),
                        Some("diff"),
                    )
                    .await;
                    if let Some(buffer) = block {
                        let mut s = state.lock().await;
                        s.set_block_text(view_buffer, buffer, &patch);
                    }
                }
            }
            refresh(state, view_buffer).await;
            push_lines_changed(state, view_buffer).await;
        }

        AgentEvent::Plan { entries } => {
            let text = crate::agent::plan_text(&entries);
            let existing = {
                let s = state.lock().await;
                s.try_doc_of(view_buffer)
                    .and_then(|d| d.conversation())
                    .and_then(|c| c.find(&BlockKey::Plan).map(|i| c.blocks[i].buffer))
            };
            match existing {
                // A new plan replaces the old one; the agent sends it whole.
                Some(buffer) => {
                    let mut s = state.lock().await;
                    s.set_block_text(view_buffer, buffer, &text);
                }
                None => {
                    if let Some(buffer) = new_block(
                        state,
                        view_buffer,
                        Some(BlockKey::Plan),
                        BlockKind::Plan,
                        Some("markdown"),
                    )
                    .await
                    {
                        let mut s = state.lock().await;
                        s.set_block_text(view_buffer, buffer, &text);
                    }
                }
            }
            refresh(state, view_buffer).await;
            push_lines_changed(state, view_buffer).await;
        }

        AgentEvent::Permission {
            request,
            tool_call_id,
            options,
        } => {
            // The same upsert rule the rest of the stream follows: a request naming a tool call we
            // were never told about **creates** the block rather than being dropped. Dropping it
            // hangs the turn with no question on screen and no way to answer — the agent is
            // blocked on us, so there would be nothing to do but close the view.
            let key = tool_call_id.map(BlockKey::ToolCall);
            let known = {
                let s = state.lock().await;
                let Some(c) = s.try_doc_of(view_buffer).and_then(|d| d.conversation()) else {
                    return;
                };
                key.as_ref().and_then(|k| c.find(k)).is_some()
            };
            if !known {
                let call = ToolCall {
                    tool_call_id: match &key {
                        Some(BlockKey::ToolCall(id)) => id.clone(),
                        _ => String::new(),
                    },
                    title: "wants permission".into(),
                    kind: ToolKind::Other,
                    status: ToolStatus::Pending,
                    locations: Vec::new(),
                    permission: None,
                };
                new_block(
                    state,
                    view_buffer,
                    key.clone(),
                    BlockKind::ToolCall(call),
                    None,
                )
                .await;
            }
            {
                let mut s = state.lock().await;
                let Some(c) = conversation_mut(&mut s, view_buffer) else {
                    return;
                };
                // The named block, or the last tool call if the request named none at all.
                let index = match &key {
                    Some(k) => c.find(k),
                    None => c
                        .blocks
                        .iter()
                        .rposition(|b| matches!(b.kind, BlockKind::ToolCall(_))),
                };
                if let Some(index) = index {
                    if let BlockKind::ToolCall(tc) = &mut c.blocks[index].kind {
                        tc.permission = Some(PendingPermission { request, options });
                    }
                }
                c.generation += 1;
            }
            refresh(state, view_buffer).await;
            push_turn_state(state, view_buffer, view_id).await;
        }

        // The two requests that make this an editor's agent rather than a chat pane.
        AgentEvent::ReadFile {
            path,
            line,
            limit,
            reply,
        } => {
            let answer = read_text_file(state, &path, line, limit).await;
            let _ = reply.send(answer);
        }
        AgentEvent::WriteFile {
            path,
            content,
            reply,
        } => {
            let answer = write_text_file(state, &path, &content).await;
            let _ = reply.send(answer);
            refresh(state, view_buffer).await;
        }

        AgentEvent::TurnEnded(reason) => {
            clear_turn(state, view_buffer, view_id, reason).await;
        }

        AgentEvent::Failed(message) => {
            clear_turn(state, view_buffer, view_id, StopReason::Failed { message }).await;
        }
    }
}

// ---- block plumbing ----------------------------------------------------------------------------

fn conversation_mut(s: &mut ServerState, view_buffer: BufferId) -> Option<&mut Conversation> {
    s.try_doc_of_mut(view_buffer)?
        .generated
        .as_mut()?
        .conversation_mut()
}

/// Create a block, its document, and its place in the view. Returns the block's buffer.
async fn new_block(
    state: &SharedState,
    view_buffer: BufferId,
    key: Option<BlockKey>,
    kind: BlockKind,
    language: Option<&str>,
) -> Option<BufferId> {
    let mut s = state.lock().await;
    let workspace = s.buffer_workspaces.get(&view_buffer).cloned();
    let title = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .map(|c| c.title.clone())?;
    let target = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.virtual_source.as_ref())
        .map(|v| v.target.clone())?;

    let buffer = s.allocate_buffer_id();
    let doc_id = s.allocate_document_id();
    let doc = Document::block(
        doc_id,
        crate::state::VirtualSource { target, title },
        language.map(str::to_string),
    );
    s.documents.insert(doc_id, doc);
    s.buffers.insert(
        buffer,
        Buffer {
            id: buffer,
            document: doc_id,
            scratch_number: None,
        },
    );
    if let Some(workspace) = workspace {
        s.buffer_workspaces.insert(buffer, workspace);
    }
    let c = conversation_mut(&mut s, view_buffer)?;
    c.push(key, buffer, kind);
    s.rebuild_view_layout(view_buffer);
    s.rebind_viewports_of(view_buffer);
    Some(buffer)
}

/// Append `text` to the block `key` names, creating it if the agent has not used this key before.
/// That "create on unknown key" rule *is* upsert, and dropping the update instead would lose the
/// first chunk of every message.
async fn append_to_block(
    state: &SharedState,
    view_buffer: BufferId,
    key: Option<BlockKey>,
    kind: BlockKind,
    language: Option<&str>,
    text: &str,
) {
    let target = {
        let s = state.lock().await;
        let Some(c) = s.try_doc_of(view_buffer).and_then(|d| d.conversation()) else {
            return;
        };
        match &key {
            Some(key) => c.find(key).map(|i| c.blocks[i].buffer),
            // No `messageId`: append to whichever block of this kind is open, which is the best
            // identity an agent that streams without ids gives us.
            None => c.chunk_target(&kind).map(|i| c.blocks[i].buffer),
        }
    };
    let buffer = match target {
        Some(buffer) => Some(buffer),
        None => new_block(state, view_buffer, key, kind, language).await,
    };
    if let Some(buffer) = buffer {
        let mut s = state.lock().await;
        if !over_budget(&s, view_buffer) {
            s.extend_block(view_buffer, buffer, text);
        }
    }
    push_lines_changed(state, view_buffer).await;
}

/// Append to a block that is known to exist, by key.
async fn append_to_key(state: &SharedState, view_buffer: BufferId, key: &BlockKey, text: &str) {
    let mut s = state.lock().await;
    let Some(buffer) = s
        .try_doc_of(view_buffer)
        .and_then(|d| d.conversation())
        .and_then(|c| c.find(key).map(|i| c.blocks[i].buffer))
    else {
        return;
    };
    if !over_budget(&s, view_buffer) {
        s.extend_block(view_buffer, buffer, text);
    }
}

/// A block written by us rather than by the agent — the prompt.
async fn push_block(
    state: &SharedState,
    view_buffer: BufferId,
    key: Option<BlockKey>,
    kind: BlockKind,
    language: Option<&str>,
    text: &str,
) {
    if let Some(buffer) = new_block(state, view_buffer, key, kind, language).await {
        let mut s = state.lock().await;
        s.extend_block(view_buffer, buffer, text);
    }
    push_lines_changed(state, view_buffer).await;
}

/// Whether this conversation has taken as much text as it is allowed to hold.
fn over_budget(s: &ServerState, view_buffer: BufferId) -> bool {
    let Some(c) = s.try_doc_of(view_buffer).and_then(|d| d.conversation()) else {
        return true;
    };
    let total: usize = c
        .blocks
        .iter()
        .filter_map(|b| s.try_doc_of(b.buffer))
        .map(|d| d.text.len_chars())
        .sum();
    total >= MAX_CONVERSATION_CHARS
}

/// v1's flat `oldText`/`newText` as a unified patch, so an agent's proposed change renders through
/// the machinery `git/show` already uses.
///
/// `git2::Patch::from_buffers` is libgit2's own two-buffer diff — no new dependency, and the same
/// generator the rest of this editor's patches come from.
fn render_patch(path: &Path, old_text: Option<&str>, new_text: &str) -> String {
    let old = old_text.unwrap_or("");
    let name = path.file_name().map(std::path::Path::new).unwrap_or(path);
    match git2::Patch::from_buffers(
        old.as_bytes(),
        Some(name),
        new_text.as_bytes(),
        Some(name),
        None,
    )
    .and_then(|mut p| p.to_buf())
    {
        Ok(buf) => buf.as_str().unwrap_or_default().to_string(),
        // A diff we cannot render is still worth showing as the new text: losing the agent's
        // proposal entirely because libgit2 declined is the worse failure.
        Err(_) => new_text.to_string(),
    }
}

// ---- the filesystem bridge ---------------------------------------------------------------------

/// `fs/read_text_file` — served from the **live document** when one is open, so the agent reads
/// what the user can see rather than what was last written to disk.
///
/// That is not our embellishment: the protocol specifies this method as returning contents
/// "including unsaved changes in the editor", and it is the single clearest reason this view
/// speaks protocol v1. A path with no document open falls through to disk.
async fn read_text_file(
    state: &SharedState,
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, String> {
    let canonical = crate::handlers::buffer::canonicalize_partial(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    let live = {
        let s = state.lock().await;
        s.document_for_path(&canonical)
            .and_then(|id| s.documents.get(&id))
            .map(|d| d.text.to_string())
    };
    let text = match live {
        Some(text) => text,
        None => std::fs::read_to_string(&canonical)
            .map_err(|e| format!("{}: {e}", canonical.display()))?,
    };

    // `line` is 1-based on the wire and `limit` is a count of lines; both are optional and either
    // may appear without the other.
    if line.is_none() && limit.is_none() {
        return Ok(text);
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let mut out = String::new();
    for l in text
        .lines()
        .skip(start)
        .take(limit.unwrap_or(u32::MAX) as usize)
    {
        out.push_str(l);
        out.push('\n');
    }
    Ok(out)
}

/// `fs/write_text_file` — applied through the **ordinary edit path** when the file is open, so the
/// agent's change is undoable, marks the document dirty, and is pushed to every viewport showing
/// it. A file nobody has open is written to disk and picked up by the watcher, and one that does
/// not exist is created, which the protocol requires.
async fn write_text_file(state: &SharedState, path: &Path, content: &str) -> Result<(), String> {
    let canonical = crate::handlers::buffer::canonicalize_partial(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    let open = {
        let s = state.lock().await;
        s.document_for_path(&canonical)
            .and_then(|id| s.buffers.values().find(|b| b.document == id).map(|b| b.id))
    };

    if let Some(buffer) = open {
        let mut s = state.lock().await;
        let cursors = document_cursor_snapshot(&s, buffer);
        let end = s.doc_of(buffer).text.len_chars();
        // Through `editable_doc`, so a read-only document refuses this by construction rather
        // than by a check this function has to remember.
        s.editable_doc(buffer)
            .map_err(|e| e.message.clone())?
            .apply_edit(0, end, content, EditKindTag::Agent, cursors);
        drop(s);
        let pushes = {
            let s = state.lock().await;
            collect_doc_lines_changed_pushes(&s, buffer)
        };
        for (sender, notif) in pushes {
            let _ = sender.send(notif).await;
        }
        return Ok(());
    }

    if let Some(parent) = canonical.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&canonical, content).map_err(|e| format!("{}: {e}", canonical.display()))
}

// ---- pushes ------------------------------------------------------------------------------------

/// Rebuild the view's layout and re-bind its viewports — what every change to a block's chrome
/// needs, and what a text append already does for itself.
async fn refresh(state: &SharedState, view_buffer: BufferId) {
    let mut s = state.lock().await;
    s.rebuild_view_layout(view_buffer);
    s.rebind_viewports_of(view_buffer);
}

/// The content push for a block that just grew — the same one an edit sends, because to everything
/// downstream this *is* an edit to a document somebody is viewing.
async fn push_lines_changed(state: &SharedState, view_buffer: BufferId) {
    let pushes = {
        let s = state.lock().await;
        let Some(c) = s.try_doc_of(view_buffer).and_then(|d| d.conversation()) else {
            return;
        };
        let mut pushes = PendingPushes::new();
        for block in &c.blocks {
            pushes.extend(collect_doc_lines_changed_pushes(&s, block.buffer));
        }
        pushes
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

/// End the turn, and say how it went.
async fn clear_turn(
    state: &SharedState,
    view_buffer: BufferId,
    view_id: ViewId,
    reason: StopReason,
) {
    {
        let mut s = state.lock().await;
        let Some(c) = conversation_mut(&mut s, view_buffer) else {
            return;
        };
        c.turn = None;
        // A turn that ended cannot still be waiting on a question: the agent has stopped asking.
        for block in &mut c.blocks {
            if let BlockKind::ToolCall(tc) = &mut block.kind {
                tc.permission = None;
                // A call still marked in-progress when the turn ended never finished.
                if matches!(tc.status, ToolStatus::Pending | ToolStatus::InProgress) {
                    tc.status = ToolStatus::Failed;
                }
            }
        }
        c.generation += 1;
    }
    refresh(state, view_buffer).await;
    push_turn_changed(
        state,
        view_id,
        Some(TurnState {
            running: false,
            activity: None,
            stop_reason: Some(reason),
        }),
    )
    .await;
}

/// Re-send the current turn state — what a tool call starting or a permission arriving changes.
async fn push_turn_state(state: &SharedState, view_buffer: BufferId, view_id: ViewId) {
    let turn = {
        let s = state.lock().await;
        s.try_doc_of(view_buffer)
            .and_then(|d| d.conversation())
            .and_then(|c| {
                c.turn.as_ref().map(|t| TurnState {
                    running: true,
                    activity: t.activity.clone(),
                    stop_reason: None,
                })
            })
    };
    if let Some(turn) = turn {
        push_turn_changed(state, view_id, Some(turn)).await;
    }
}

/// Push `agent/turn_changed` to every connected client.
///
/// Every client, like `git/operation_changed` and `shell/run_changed`: a conversation belongs to
/// the workspace rather than to whoever pressed `Enter`, and a client with the view open wants the
/// indicator whether or not it started the turn.
async fn push_turn_changed(state: &SharedState, view_id: ViewId, turn: Option<TurnState>) {
    let params = AgentTurnChangedParams { view_id, turn };
    let value = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
    let pushes: PendingPushes = {
        let s = state.lock().await;
        s.clients
            .values()
            .map(|sess| {
                (
                    sess.outbound.clone(),
                    Notification {
                        jsonrpc: JsonRpc,
                        method: AgentTurnChanged::NAME.into(),
                        params: value.clone(),
                    },
                )
            })
            .collect()
    };
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}
