//! What an agent view *is*, server-side: a conversation of blocks and the input under it.
//!
//! A conversation is a composed view whose elements are **one document per block** — a message, a
//! tool call, a diff — with the input bound last as an ordinary editable document. One document
//! per block rather than one transcript, because ACP addresses what it has already sent: a tool
//! call is updated by its id long after later blocks exist, and a conversation kept in one rope
//! would have to splice in the middle and re-key every block below. Per-block documents keep every
//! write an append, which is the only shape [`crate::state::Document::write_tail`] allows.
//!
//! This module holds the model and the boxes' names; [`connection`] talks to the agent and
//! [`crate::handlers::agent`] drives it. The split is the shell's: nothing here touches a rope, and
//! nothing here spawns a process.

pub mod config;
pub mod connection;
/// A test seam, not behind `cfg(test)`: the integration tests link this library rather than
/// compiling into it, so a gated module would be invisible to exactly the tests that need it.
pub mod dummy;

use aether_protocol::agent::{BlockId, PermissionOption, StopReason};
use aether_protocol::viewport::Element;
use aether_protocol::BufferId;
use std::path::PathBuf;

// `$HOME` shortened to `~` — the shell view's own, so two kinds of composed view cannot disagree
// about how a directory is written on a box.
use crate::shell::display_path;

pub use config::{AgentSpec, KNOWN_AGENTS};
pub use connection::{AgentCommand, AgentEvent, AgentHandle, ChunkKind};

/// One agent view's state.
///
/// Lives on the view's own virtual document (as `Generated::Agent`), because it *is* what the view
/// was generated from — the same place a patch's index and a shell's transcript live, for the same
/// reason: the content and the account of what the content means are built together and must not
/// drift apart. Note the view document itself holds **no text**; every block's text is in the
/// block's own document.
#[derive(Debug)]
pub struct Conversation {
    /// The blocks, oldest first. One element of the view each.
    pub blocks: Vec<Block>,
    /// The document holding the prompt being typed. Internal: never listed, never backed up,
    /// never session-recorded, never dirty. Dropped with the view.
    pub input: BufferId,
    /// Where the agent was started — the root of the project holding the buffer that was focused
    /// when the view opened, else the workspace's first root. Shown on the input's box.
    pub cwd: PathBuf,
    /// Which row of [`KNOWN_AGENTS`] is behind this conversation.
    pub agent: &'static AgentSpec,
    /// `Agent N` — what the picker row, the status bar and the outline call this conversation.
    pub title: String,
    /// The turn in flight, if one is.
    pub turn: Option<Turn>,
    /// The ACP session this conversation is, once the handshake has produced one. Recorded in the
    /// snapshot so a restart can ask the agent to load its own context back; `None` until
    /// [`AgentEvent::Ready`] arrives, and for a conversation restored from disk that has not been
    /// reconnected.
    pub session: Option<String>,
    /// How to talk to the agent, once there is one. Dropping the conversation drops this, which
    /// ends the connection task and kills the subprocess group — which is why closing a view needs
    /// no separate teardown call.
    ///
    /// `None` for a conversation **restored from disk**: opening one is reading a record, and an
    /// agent is a subprocess that costs real money to run, so it starts on the first prompt.
    pub handle: Option<AgentHandle>,
    /// A session id from a snapshot that we have asked, or are about to ask, the agent to load.
    /// Distinct from [`Self::session`], which is a session the agent has confirmed it has: a
    /// snapshot written from a session that failed to load would claim a context nobody holds.
    pub resuming: Option<String>,
    /// The generation and input revision the on-disk snapshot last captured — one stamp, since
    /// most of what changes about a conversation is in neither document's text.
    pub backed_up: Option<(u64, aether_protocol::Revision)>,
    /// Bumped by every change to this conversation's state — a block pushed, text appended, a
    /// status set. What a future backup flush would compare against; most of this state is not in
    /// any document's text.
    pub generation: u64,
    /// Source of block ids, unique within this conversation.
    next_block: BlockId,
}

/// A turn in flight: one `session/prompt` that has not come back yet.
#[derive(Debug, Clone)]
pub struct Turn {
    /// What the user sent, for the status bar and for a toast when it finishes out of sight.
    pub prompt: String,
    /// The title of the tool call the agent is currently working through, if any.
    pub activity: Option<String>,
}

/// One block of a conversation, and the document holding its text.
#[derive(Debug)]
pub struct Block {
    pub id: BlockId,
    /// What the agent calls this block, when it calls it anything. `None` is the anonymous case:
    /// an agent that streams content chunks without a `messageId` gets one block per run of
    /// same-kind chunks, which is the best identity available. See [`Conversation::chunk_target`].
    pub key: Option<BlockKey>,
    /// This block's own document. Read-only and internal, like the view's.
    pub buffer: BufferId,
    pub kind: BlockKind,
}

/// What the agent named a block. Only ever compared, never parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKey {
    /// A `messageId` from a content chunk.
    Message(String),
    /// A `toolCallId`.
    ToolCall(String),
    /// The plan. There is one, and a new plan replaces it.
    Plan,
    /// The n-th diff inside one tool call's content, so a re-sent content list updates the diffs
    /// it already produced rather than appending a second copy of each.
    Diff(String, usize),
}

/// What a block is, and the part of its state that lives in chrome rather than text.
#[derive(Debug, Clone)]
pub enum BlockKind {
    /// What the user sent. Written by us, not the agent — an agent echoing the prompt back as a
    /// `user_message_chunk` updates this block rather than adding a second one.
    UserMessage,
    /// Prose from the agent. Markdown, and highlighted as such.
    AgentMessage,
    /// The agent's reasoning, when it chooses to share it. Rendered like a message; a shell may
    /// choose to dim it.
    AgentThought,
    /// A tool the agent ran, or is running, or is asking permission to run.
    ToolCall(ToolCall),
    /// A file the agent proposes to change, as a patch over the existing patch machinery.
    Diff(Diff),
    /// The agent's plan for the turn.
    Plan,
}

impl BlockKind {
    /// Whether a content chunk of this kind may append to a block of that kind. Only the message
    /// kinds stream; a tool call's text arrives through its own content list.
    fn is_message(&self) -> bool {
        matches!(
            self,
            BlockKind::UserMessage | BlockKind::AgentMessage | BlockKind::AgentThought
        )
    }

    /// The word a block's box is named with when it has nothing better to say.
    pub fn noun(&self) -> &'static str {
        match self {
            BlockKind::UserMessage => "You",
            BlockKind::AgentMessage => "Agent",
            BlockKind::AgentThought => "Thinking",
            BlockKind::ToolCall(_) => "Tool",
            BlockKind::Diff(_) => "Diff",
            BlockKind::Plan => "Plan",
        }
    }
}

/// A tool call's state — all of it chrome, none of it text, which is why a status change costs a
/// layout rebuild and no document write at all.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_call_id: String,
    /// The agent's own wording for what it is doing.
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
    /// Where in the workspace this touches, if it said. The first is what `Enter` on the block
    /// opens.
    pub locations: Vec<Location>,
    /// Set while the agent is blocked on us. The turn cannot proceed until it is answered.
    pub permission: Option<PendingPermission>,
}

/// A permission request the agent is waiting on.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    /// Ours, not the protocol's: what [`AgentCommand::Respond`] names so the connection task can
    /// find the responder it parked.
    pub request: u64,
    /// The agent's options, in the order it offered them.
    pub options: Vec<PermissionOption>,
}

impl PendingPermission {
    /// The option `Space v a` answers with: the first that allows. `None` when the agent offered
    /// no allowing option, in which case there is nothing to accept.
    pub fn accept(&self) -> Option<&PermissionOption> {
        self.options.iter().find(|o| o.kind.allows())
    }

    /// The option `Space v d` answers with: the first that rejects.
    pub fn reject(&self) -> Option<&PermissionOption> {
        self.options.iter().find(|o| o.kind.rejects())
    }
}

/// A file and line a tool call touched.
#[derive(Debug, Clone)]
pub struct Location {
    pub path: PathBuf,
    /// 0-based, as everywhere in this editor. ACP sends 1-based; the conversion happens once, at
    /// the connection boundary.
    pub line: Option<u32>,
}

/// What kind of thing a tool call is, for the word its box wears.
///
/// A closed enum with an `Other` arm rather than the wire string, so a shell matches exhaustively
/// and a kind we have not seen still renders as something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

impl ToolKind {
    pub fn label(self) -> &'static str {
        match self {
            ToolKind::Read => "read",
            ToolKind::Edit => "edit",
            ToolKind::Delete => "delete",
            ToolKind::Move => "move",
            ToolKind::Search => "search",
            ToolKind::Execute => "run",
            ToolKind::Think => "think",
            ToolKind::Fetch => "fetch",
            ToolKind::SwitchMode => "mode",
            ToolKind::Other => "tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    /// The mark a state draws as: a glyph and the highlight role that colours it.
    ///
    /// **Both**, not colour alone. Two reasons, and the second is the binding one: colour-only
    /// status fails for a colour-blind reader, and the palette this may draw from is the patch
    /// roles — inventing new ones would add to the single part of the theme with no cross-shell
    /// parity test. Four roles for five states means pending and running would collide, so the
    /// shape has to carry the difference anyway.
    pub fn mark(self) -> (&'static str, &'static str) {
        use crate::patch::{ADDED, FILE, META, REMOVED};
        match self {
            ToolStatus::Pending => ("○", META),
            ToolStatus::InProgress => ("◐", FILE),
            ToolStatus::Completed => ("●", ADDED),
            ToolStatus::Failed => ("✕", REMOVED),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::InProgress => "running",
            ToolStatus::Completed => "done",
            ToolStatus::Failed => "failed",
        }
    }
}

/// A proposed change to one file, already rendered as a unified patch.
#[derive(Debug, Clone)]
pub struct Diff {
    pub path: PathBuf,
    /// True when the file did not exist before — `oldText: null` on the wire.
    pub is_new: bool,
}

impl Conversation {
    pub fn new(input: BufferId, cwd: PathBuf, agent: &'static AgentSpec, title: String) -> Self {
        Conversation {
            blocks: Vec::new(),
            input,
            cwd,
            agent,
            title,
            turn: None,
            session: None,
            handle: None,
            resuming: None,
            generation: 0,
            backed_up: None,
            next_block: 0,
        }
    }

    pub fn is_running(&self) -> bool {
        self.turn.is_some()
    }

    /// The block a keyed update belongs to, by key.
    pub fn find(&self, key: &BlockKey) -> Option<usize> {
        self.blocks.iter().position(|b| b.key.as_ref() == Some(key))
    }

    pub fn find_by_id(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == id)
    }

    /// The block an *anonymous* content chunk of `kind` appends to: the last block, if it is a
    /// message block of the same kind. Anything else — a tool call since, a different message
    /// kind, nothing at all — means this chunk opens a new block.
    ///
    /// This is the fallback for an agent that streams without a `messageId`, which the protocol
    /// permits (`ContentChunk::message_id` is optional) and which is the path most agents actually
    /// take. Without it every chunk would become a block of its own.
    pub fn chunk_target(&self, kind: &BlockKind) -> Option<usize> {
        let last = self.blocks.len().checked_sub(1)?;
        let block = &self.blocks[last];
        let same = std::mem::discriminant(&block.kind) == std::mem::discriminant(kind);
        (block.kind.is_message() && same).then_some(last)
    }

    /// Register a new block and hand back its index. The caller has already created `buffer`.
    pub fn push(&mut self, key: Option<BlockKey>, buffer: BufferId, kind: BlockKind) -> usize {
        let id = self.next_block;
        self.next_block += 1;
        self.generation += 1;
        self.blocks.push(Block {
            id,
            key,
            buffer,
            kind,
        });
        self.blocks.len() - 1
    }

    /// The tool call block holding a pending permission request, if one is outstanding. At most
    /// one can be: the agent is blocked on the answer, so it cannot ask a second question.
    pub fn pending_permission(&self) -> Option<(BlockId, &PendingPermission)> {
        self.blocks.iter().find_map(|b| match &b.kind {
            BlockKind::ToolCall(tc) => tc.permission.as_ref().map(|p| (b.id, p)),
            _ => None,
        })
    }

    /// Every document this conversation owns — the blocks' and the input's. What closing the view
    /// drops.
    pub fn documents(&self) -> Vec<BufferId> {
        let mut ids: Vec<BufferId> = self.blocks.iter().map(|b| b.buffer).collect();
        ids.push(self.input);
        ids
    }
}

// ---- chrome ------------------------------------------------------------------------------------

/// The name on a block's box, drawn on its top border: who is speaking, or what a tool is doing
/// and how it went.
///
/// Styled with a patch's own roles, deliberately: a conversation is another composed view, and
/// inventing a palette for it would add roles to the one part of the theme with no cross-shell
/// parity test.
pub fn block_title(block: &Block) -> Vec<Element> {
    use crate::patch::{ADDED, FILE, META, REMOVED};

    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut push = |s: &str, role: &'static str| {
        if s.is_empty() {
            return;
        }
        let start = text.len();
        text.push_str(s);
        highlights.push(highlight(start, text.len(), role));
    };

    match &block.kind {
        BlockKind::ToolCall(tc) => {
            // The state as a mark, then the agent's own words for what it is doing. The *kind*
            // ("run", "read") is not shown: the agent's title already says it — "Running cargo
            // test" — and printing the kind first made every box wear the same word, which read as
            // the name of the block rather than as a description of it. It stands in only when the
            // agent gave no title at all.
            let (glyph, role) = tc.status.mark();
            push(glyph, role);
            push(" ", META);
            let title = one_line(&tc.title);
            push(
                if title.is_empty() {
                    tc.kind.label()
                } else {
                    &title
                },
                FILE,
            );
            // Kept as words, unlike every other state: this one is an instruction to the reader
            // rather than a report, and it has to survive being skimmed.
            if tc.permission.is_some() {
                push("  ", META);
                push("needs permission", REMOVED);
            }
        }
        BlockKind::Diff(diff) => {
            push(&display_path(&diff.path), FILE);
            if diff.is_new {
                push("  ", META);
                push("new file", ADDED);
            }
        }
        kind => push(kind.noun(), META),
    }

    vec![Element::text(text, highlights)]
}

/// Whether a block is prose — the conversation itself, rendered bare, rather than a named thing
/// that happened. The two the user is actually reading and writing; everything else keeps its box.
///
/// A thought is deliberately **not** prose here: it is the agent's working rather than its answer,
/// and un-boxing it would leave no way to tell reasoning from reply.
pub fn is_prose(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::UserMessage | BlockKind::AgentMessage)
}

/// Whether a block **changed a file** — what `c` steps to, and nothing else.
///
/// A conversation is mostly things that happened, and only some of them are changes. A patch the
/// agent proposed is one. A tool that writes — an edit, a delete, a move — is one. Reading a file,
/// searching, running a command, fetching, thinking out loud: those happened, and `o` steps them,
/// but calling them changes is what made `c` and `o` the same key in a conversation.
///
/// Read off [`ToolKind`], which ACP already gives us, rather than off anything we infer: the agent
/// says what sort of tool it ran, and this is only a question about that.
pub fn changes_a_file(kind: &BlockKind) -> bool {
    match kind {
        BlockKind::Diff(_) => true,
        BlockKind::ToolCall(tc) => {
            matches!(tc.kind, ToolKind::Edit | ToolKind::Delete | ToolKind::Move)
        }
        BlockKind::UserMessage
        | BlockKind::AgentMessage
        | BlockKind::AgentThought
        | BlockKind::Plan => false,
    }
}

/// Whether a block is **blocked on the user** — a tool call the agent cannot proceed past until
/// it is allowed or declined.
///
/// Such a block refuses to fold. Folding is for the record of what happened, and this block is not
/// a record yet: it is a question, and its answer lives in the chrome inside the box. A folded one
/// would leave the turn stalled behind a rule with no way to see what it was asking. The view
/// rebuilds on every status change, so this flips back the moment the question is answered and the
/// block folds away on its own — no state to reconcile, and nothing that can strand a block open.
pub fn awaits_permission(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::ToolCall(tc) if tc.permission.is_some())
}

/// Whether a block is **prose on the wire** — sent as [`aether_protocol::ui::Element::Prose`], the
/// markdown parsed, so a heading is a heading and a fence is a panel rather than source shown back.
///
/// Only the agent's reply. What you typed stays as you typed it: re-rendering a prompt would mean
/// the line you wrote and the line shown back to you differed. That also keeps the two bare blocks
/// visibly distinct now that neither wears a box.
pub fn is_rendered(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::AgentMessage)
}

/// The label above a bare block: who is speaking, when that is not obvious.
///
/// Only the user's own turn wears one. The agent's reply is the thing you are reading, and a
/// heading over it would be the box we just removed in another shape — but a prompt with nothing
/// above it is indistinguishable from the reply that follows, which is the one thing this has to
/// prevent.
pub fn speaker_row(block: &Block) -> Vec<Element> {
    use crate::patch::META;

    if !matches!(block.kind, BlockKind::UserMessage) {
        return Vec::new();
    }
    let text = block.kind.noun().to_string();
    let highlights = vec![highlight(0, text.len(), META)];
    vec![Element::chrome(vec![Element::row(vec![Element::text(
        text, highlights,
    )])])]
}

/// The chrome inside a tool call's box: the permission question, when there is one. Everything
/// else a block has to say is in its title or its text.
///
/// The options are **buttons** — [`aether_protocol::ui::Element::Action`] — so `Tab` reaches them
/// and `Enter` answers, and a pointer can simply press one. They were a row of coloured words with
/// two keybindings (`Space v a`, `Space v d`) pointing at them from the client's keymap, which is
/// the arrangement the action vocabulary exists to end: what a view can do is the view's to say.
///
/// The wording stays the agent's own, as it always was. What the shell reads is
/// [`aether_protocol::ui::ViewAction::kind`] — accept or reject — so it can paint the two apart
/// without parsing labels, exactly as it did from
/// [`aether_protocol::agent::PermissionKind`] before.
pub fn permission_row(block: &Block) -> Vec<Element> {
    use aether_protocol::ui::ViewAction;

    let BlockKind::ToolCall(tc) = &block.kind else {
        return Vec::new();
    };
    let Some(pending) = &tc.permission else {
        return Vec::new();
    };

    let mut children: Vec<Element> = Vec::new();
    for (i, option) in pending.options.iter().enumerate() {
        if i > 0 {
            children.push(Element::Space { cols: 2 });
        }
        // An option that neither allows nor rejects is still an answer, and answering it ends the
        // turn's wait — so it goes out as a reject, which is what declining to allow means.
        children.push(Element::Action {
            action: ViewAction::Permission {
                allow: option.kind.allows(),
            },
            label: vec![Element::text(option.label.clone(), Vec::new())],
            enabled: true,
        });
    }
    vec![Element::chrome(vec![Element::row(children)])]
}

/// The agent's plan as the lines of its block: one entry per line, each marked with its state.
///
/// The mark is the **glyph only** here, with no colour: this is document text rather than chrome,
/// so it carries no highlight roles of its own. That is why the shape does the work — the same
/// reason [`ToolStatus::mark`] varies the glyph at all rather than leaning on colour.
pub fn plan_text(entries: &[(String, ToolStatus)]) -> String {
    let mut text = String::new();
    for (content, status) in entries {
        let (glyph, _) = status.mark();
        text.push_str(glyph);
        text.push(' ');
        text.push_str(content);
        text.push('\n');
    }
    text
}

/// The line written into a block when a turn ends in a way worth recording in the transcript
/// rather than only in a toast.
pub fn stop_notice(reason: &StopReason) -> Option<String> {
    reason
        .is_notable()
        .then(|| format!("[{}]\n", reason.label()))
}

/// How a **turn** is named where it is listed — the outline's rows, and the breadcrumb naming the
/// turn you are reading.
///
/// The first line of what you typed. Your own words, because that is the only name a turn has and
/// the only one that tells two of them apart; the word "You" over every row named nothing. A
/// prompt that ran on gets the `⏎` the commands in a shell's outline get, for the same reason: the
/// row says there was more without pretending to show it.
pub fn turn_label(text: &str) -> String {
    one_line(text.trim())
}

/// What a block is, in a word or two: its kind, and what it says if that is not enough on its own.
pub fn block_label(block: &Block) -> String {
    match &block.kind {
        BlockKind::ToolCall(tc) => format!("{} {}", tc.kind.label(), one_line(&tc.title)),
        BlockKind::Diff(diff) => format!("diff {}", display_path(&diff.path)),
        kind => kind.noun().to_string(),
    }
}

fn one_line(s: &str) -> String {
    match s.find('\n') {
        Some(i) => format!("{} ⏎", &s[..i]),
        None => s.to_string(),
    }
}

fn highlight(start: usize, end: usize, kind: &str) -> aether_protocol::viewport::Highlight {
    aether_protocol::viewport::Highlight {
        start: start as u32,
        end: end as u32,
        kind: kind.to_string(),
    }
}

// ---- persistence ---------------------------------------------------------------------------------

/// A conversation written down, for the backup file. See [`Conversation::snapshot`].
///
/// **The display, not the agent's memory.** Those are two different things and only one of them is
/// ours: this holds what was on screen — every block, including the tool calls, diffs and plan the
/// agent may not replay — while the agent's context lives with the agent and comes back, if at all,
/// through `session/load`. Keeping both is the point: you can read yesterday's conversation without
/// launching anything, and a resumed agent does not have to reconstruct what we already watched.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentSnapshot {
    pub version: u32,
    /// Which row of [`KNOWN_AGENTS`] this was, by id. An id this build no longer knows means the
    /// snapshot is readable but cannot be resumed.
    pub agent: String,
    pub cwd: PathBuf,
    /// The ACP session this conversation was, if it got as far as having one. What `session/load`
    /// would name to bring the agent's own context back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// What was typed but not sent.
    #[serde(default)]
    pub input: String,
    pub blocks: Vec<BlockSnapshot>,
}

/// One block written down: what it was, what its box said, and its text.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSnapshot {
    pub kind: BlockKindSnapshot,
    pub text: String,
}

/// A block's kind, flattened for the wire. Deliberately not the live [`BlockKind`]: that carries a
/// pending permission request, which is a thing only a running agent can have — a snapshot of one
/// would come back as a question nobody can answer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockKindSnapshot {
    UserMessage,
    AgentMessage,
    AgentThought,
    ToolCall {
        title: String,
        tool: ToolKind,
        status: ToolStatus,
    },
    Diff {
        path: PathBuf,
        is_new: bool,
    },
    Plan,
}

/// Most characters a snapshot keeps, oldest blocks dropped first. A conversation that ran all day
/// holds more than anyone scrolls back through after a restart.
pub const SNAPSHOT_BUDGET: usize = 2 * 1024 * 1024;

impl AgentSnapshot {
    /// Drop whole blocks from the front until the text fits. The newest block is never dropped: a
    /// conversation that came back empty would be worse than one that came back short.
    pub fn trimmed(mut self, budget: usize) -> Self {
        let mut total: usize = self.blocks.iter().map(|b| b.text.len()).sum();
        while total > budget && self.blocks.len() > 1 {
            let dropped = self.blocks.remove(0);
            total -= dropped.text.len();
        }
        self
    }
}

impl Conversation {
    /// Write this conversation down. `text_of` reads a block's document — the texts live in the
    /// blocks' own buffers, which this module cannot reach.
    pub fn snapshot(&self, input: &str, text_of: impl Fn(BufferId) -> String) -> AgentSnapshot {
        AgentSnapshot {
            version: 1,
            agent: self.agent.id.to_string(),
            cwd: self.cwd.clone(),
            session: self.session.clone(),
            input: input.to_string(),
            blocks: self
                .blocks
                .iter()
                .map(|b| BlockSnapshot {
                    kind: match &b.kind {
                        BlockKind::UserMessage => BlockKindSnapshot::UserMessage,
                        BlockKind::AgentMessage => BlockKindSnapshot::AgentMessage,
                        BlockKind::AgentThought => BlockKindSnapshot::AgentThought,
                        BlockKind::ToolCall(tc) => BlockKindSnapshot::ToolCall {
                            title: tc.title.clone(),
                            tool: tc.kind,
                            status: tc.status,
                        },
                        BlockKind::Diff(d) => BlockKindSnapshot::Diff {
                            path: d.path.clone(),
                            is_new: d.is_new,
                        },
                        BlockKind::Plan => BlockKindSnapshot::Plan,
                    },
                    text: text_of(b.buffer),
                })
                .collect(),
        }
    }
}

impl BlockKindSnapshot {
    /// The live kind this was. A restored tool call has no pending permission by construction —
    /// see [`BlockKindSnapshot`].
    pub fn restore(&self) -> BlockKind {
        match self {
            BlockKindSnapshot::UserMessage => BlockKind::UserMessage,
            BlockKindSnapshot::AgentMessage => BlockKind::AgentMessage,
            BlockKindSnapshot::AgentThought => BlockKind::AgentThought,
            BlockKindSnapshot::ToolCall {
                title,
                tool,
                status,
            } => BlockKind::ToolCall(ToolCall {
                tool_call_id: String::new(),
                title: title.clone(),
                kind: *tool,
                status: *status,
                locations: Vec::new(),
                permission: None,
            }),
            BlockKindSnapshot::Diff { path, is_new } => BlockKind::Diff(Diff {
                path: path.clone(),
                is_new: *is_new,
            }),
            BlockKindSnapshot::Plan => BlockKind::Plan,
        }
    }

    /// Which language the restored block's document parses as, matching what it had when live.
    pub fn language(&self) -> Option<&'static str> {
        match self {
            BlockKindSnapshot::UserMessage
            | BlockKindSnapshot::AgentMessage
            | BlockKindSnapshot::AgentThought
            | BlockKindSnapshot::Plan => Some("markdown"),
            BlockKindSnapshot::Diff { .. } => Some("diff"),
            BlockKindSnapshot::ToolCall { .. } => None,
        }
    }
}
