//! `agent/*` — a conversation with an ACP coding agent, rendered as a view.
//!
//! The agent is a subprocess speaking the [Agent Client Protocol][acp] over its stdio; the server
//! is its client. **That wire does not appear here.** This module is Aether's own protocol —
//! what a shell needs in order to present a conversation and drive it — and it stays as small as
//! the shell view's: the text of a prompt is already server-side (it is the input document), the
//! conversation's content rides the existing `view/lines_changed`, and everything a shell paints
//! comes off the window it already receives.
//!
//! The scope names what it is, not who drives it, like `git/*`, `lsp/*` and `shell/*`.
//!
//! [acp]: https://agentclientprotocol.com/

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::ui::FieldId;
use crate::ViewId;
use serde::{Deserialize, Serialize};

/// One block within one conversation, unique for the life of the view — a message, a tool call, a
/// diff or the plan. Not a global id: a block is only ever named alongside the view it belongs to.
pub type BlockId = u64;

// ---- agent/open --------------------------------------------------------------------------------

/// Mint an agent conversation — `Space Alt-a`.
///
/// **Always creates**, exactly as [`crate::shell::ShellOpen`] does and for the same reason: the
/// agents picker (`Space a`) is how you return to a conversation you already have, so the open key
/// has one meaning. The "focused idle conversation, else the MRU idle one" heuristic and the
/// `from_view` field it was decided from are both gone.
pub struct AgentOpen;
impl RpcMethod for AgentOpen {
    const NAME: &'static str = "agent/open";
    type Params = AgentOpenParams;
    type Result = AgentOpenResult;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOpenParams {
    /// Which agent to launch, by the id of a row in the server's table. `None` takes the first
    /// that resolves on `PATH` — which is the whole of the choice for a machine with one agent
    /// installed, and the reason this is not a required parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOpenResult {
    /// The opened view, in the same shape `git/show` and `shell/open` return — so the client's
    /// adopt path is identical and nothing about a conversation needs its own opening ceremony.
    pub opened: crate::view::ViewOpenResult,
    /// Which element of the view is the input, so the open can land the cursor there without
    /// re-deriving it from a window it has not received yet. The same number the window's
    /// [`crate::ui::ElementRole::Input`] marks.
    pub input: FieldId,
}

// ---- agent/prompt ------------------------------------------------------------------------------

/// Send what is typed in the conversation's input — `Enter` in the input element.
///
/// **No `prompt` parameter.** The text is the input document's, which the server already holds;
/// sending it back would make the client the authority on content it does not own. The server
/// trims it, refuses an empty or whitespace-only input, clears the input through the ordinary edit
/// path, appends a user-message block and sends `session/prompt`.
///
/// One turn at a time per conversation: prompting while one is running fails with
/// [`crate::error::ErrorCode::AGENT_BUSY`], and the typed text is deliberately left alone so
/// typing ahead costs nothing.
pub struct AgentPrompt;
impl RpcMethod for AgentPrompt {
    const NAME: &'static str = "agent/prompt";
    type Params = AgentPromptParams;
    type Result = AgentPromptResult;
    // The buffer this *names* is the conversation's read-only transcript; the document it edits is
    // the input, which is an ordinary one. Declaring it a mutation would have the client decline
    // every `Enter` locally — see `RpcMethod::MUTATES_TEXT`.
    const MUTATES_TEXT: bool = false;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPromptParams {
    pub view_id: ViewId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPromptResult {
    /// False when the input held nothing worth sending. Not an error: the user pressed `Enter` on
    /// an empty line, which is a no-op rather than a mistake.
    pub sent: bool,
}

// ---- agent/cancel ------------------------------------------------------------------------------

/// Stop the conversation's running turn — reached by `Space v c` through [`crate::view::ViewInterrupt`]. The agent is asked to stop; unfinished tool
/// calls are marked cancelled and any pending permission request is answered `cancelled`, which is
/// what the protocol requires of a client that cancels.
pub struct AgentCancel;
impl RpcMethod for AgentCancel {
    const NAME: &'static str = "agent/cancel";
    type Params = AgentCancelParams;
    type Result = AgentCancelResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCancelParams {
    pub view_id: ViewId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCancelResult {
    /// False when nothing was running — the turn ended between the keystroke and this arriving,
    /// which the client treats as success rather than an error. Same convention as `git/cancel`
    /// and `shell/cancel`.
    pub cancelled: bool,
}

// ---- agent/respond -----------------------------------------------------------------------------

/// Answer a tool call's pending permission request — `Space v a` / `Space v d`, or `Enter` on the
/// block in Normal mode.
///
/// The options are the agent's, not ours: it supplies their ids and labels, and this returns one
/// of them. That is why the answer is an opaque `option` string rather than an enum, and why this
/// could not have been folded into the client's `Prompt::Confirm`, whose `ConfirmKind` is closed
/// precisely so a shell owns the wording of every question it asks.
pub struct AgentRespond;
impl RpcMethod for AgentRespond {
    const NAME: &'static str = "agent/respond";
    type Params = AgentRespondParams;
    type Result = AgentRespondResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRespondParams {
    pub view_id: ViewId,
    pub answer: Answer,
    /// Which block's request to answer. `None` means the one the conversation is blocked on, which
    /// is how a keystroke reaches it: at most one request can be outstanding, because the agent is
    /// waiting on it, so naming one is only needed by a client that offers several at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockId>,
}

/// How the user answered.
///
/// `Allow` and `Decline` are what a keystroke sends: the agent supplies the options and their
/// kinds, and the server picks the first of the right kind — so the client never parses the
/// agent's wording, and a key can never mean the opposite of what it says. `Option` is for a
/// client that offers the list itself and hands back an id from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Allow,
    Decline,
    Cancel,
    Option { id: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRespondResult {
    /// False when the request was already answered — by a cancel, or by another client racing
    /// this one. Not an error, for the same reason `cancelled: false` is not.
    pub answered: bool,
}

// ---- agent/turn_changed (notification) ---------------------------------------------------------

/// Pushed when a turn starts and when it ends. Mirrors `shell/run_changed`.
///
/// The conversation's content does **not** ride here: it is buffer text of the block documents,
/// and travels as `view/lines_changed` like any other content.
pub struct AgentTurnChanged;
impl NotificationMethod for AgentTurnChanged {
    const NAME: &'static str = "agent/turn_changed";
    type Params = AgentTurnChangedParams;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnChangedParams {
    pub view_id: ViewId,
    /// The turn that is now running, or the one that just ended. `None` means the conversation is
    /// idle again and nothing is left to say about how it got there — the blocks say it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnState>,
}

/// What a turn is: whether it is going, and how it ended if it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnState {
    pub running: bool,
    /// The title of the tool call the agent is working through, when it is working through one.
    /// The status bar shows it; the block's chrome shows it too, so this is only for a client
    /// looking somewhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// How the turn finished. `None` while it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// Why a turn ended.
///
/// A closed enum, tagged: every shell matches it exhaustively, and a reason added later cannot
/// render as a blank. `Other` carries the wire string for a reason this build does not know —
/// which is not the same as a missing arm, and is the honest way to survive a protocol that is
/// still gaining vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    /// The agent finished its turn of its own accord.
    EndTurn,
    /// It ran out of model context.
    MaxTokens,
    /// It made as many model requests as it is allowed in one turn.
    MaxTurnRequests,
    /// It declined to continue.
    Refusal,
    /// We asked it to stop.
    Cancelled,
    /// The connection failed, or the agent died. Not a protocol stop reason: ours, for the case
    /// where there is no answer at all.
    Failed { message: String },
    /// A stop reason this build does not know.
    Other { reason: String },
}

impl StopReason {
    /// One short phrase for a toast or a block's chrome.
    pub fn label(&self) -> String {
        match self {
            StopReason::EndTurn => "done".into(),
            StopReason::MaxTokens => "out of context".into(),
            StopReason::MaxTurnRequests => "turn limit reached".into(),
            StopReason::Refusal => "declined".into(),
            StopReason::Cancelled => "cancelled".into(),
            StopReason::Failed { message } => format!("failed: {message}"),
            StopReason::Other { reason } => reason.clone(),
        }
    }

    /// Whether this is worth interrupting the user for. A turn that simply ended is not.
    pub fn is_notable(&self) -> bool {
        !matches!(self, StopReason::EndTurn)
    }
}

// ---- shared block vocabulary -------------------------------------------------------------------

/// One option on a permission request, as the agent supplied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    /// Opaque to us; handed straight back in [`AgentRespondParams::option`].
    pub id: String,
    /// The agent's own wording. A shell paints this; it never invents its own.
    pub label: String,
    /// Whether this option allows or rejects, so a shell can style the two differently and bind
    /// `Space v a` / `Space v d` to the right ones without parsing labels.
    pub kind: PermissionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// A kind this build does not know — shown, but not bound to an accept or reject key.
    Other,
}

impl PermissionKind {
    pub fn allows(self) -> bool {
        matches!(
            self,
            PermissionKind::AllowOnce | PermissionKind::AllowAlways
        )
    }

    pub fn rejects(self) -> bool {
        matches!(
            self,
            PermissionKind::RejectOnce | PermissionKind::RejectAlways
        )
    }
}
