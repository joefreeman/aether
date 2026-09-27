//! `activity/*` — the work in progress in a workspace: shells running a command, agents working
//! through a turn, and the git operations you started.
//!
//! One list, whatever the kind, because the questions asked of it are the same — *is anything
//! running, what, and stop it* — and the status bar's count, the picker and the cancel
//! each used to ask them of three separate states that could not agree on scope. The list is the
//! server's to derive: it is read off the shells, conversations and git operations themselves, so
//! it cannot disagree with them either.
//!
//! **Scoped to the workspace.** A client is told about the work of the workspace it has active —
//! the shells and conversations of that workspace, and the git operations in repos it can see —
//! and nothing else.
//!
//! Not in it: the periodic fetch (unannounced by design) and language-server progress (not
//! something you started, nor something you can stop; the LSP picker shows it).

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::ViewId;
use serde::{Deserialize, Serialize};

/// Which piece of work — by the thing it belongs to, since each kind has at most one in flight per
/// owner: a shell runs one command at a time, a conversation one turn, a repo one git operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityId {
    /// The command a shell view is running.
    Shell { view_id: ViewId },
    /// The turn an agent conversation is working through.
    Agent { view_id: ViewId },
    /// A git operation (fetch, push, pull, worktree add) in a repo, by its working directory.
    Git { repo_id: String },
}

impl ActivityId {
    /// The view the work belongs to — what going to it opens. `None` for a git operation, which
    /// belongs to a repo rather than to anything on screen.
    pub fn view_id(&self) -> Option<ViewId> {
        match self {
            ActivityId::Shell { view_id } | ActivityId::Agent { view_id } => Some(*view_id),
            ActivityId::Git { .. } => None,
        }
    }
}

/// One piece of work in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub id: ActivityId,
    /// What it belongs to, as the user knows it: `Shell 2`, `Agent 1`, or the repo's directory
    /// name.
    pub owner: String,
    /// What it is doing: the shell's command, the agent's current tool call (`thinking` when it
    /// has not named one), or the git operation (`Pushing`).
    pub label: String,
}

// ---- activity/changed (notification) -----------------------------------------------------------

/// The receiving client's workspace's work in progress, whole — sent whenever any of it starts,
/// finishes or relabels, and when the client's workspace changes. Whole rather than a delta: the
/// list is a handful of entries, and a replacement cannot drift.
pub struct ActivityChanged;
impl NotificationMethod for ActivityChanged {
    const NAME: &'static str = "activity/changed";
    type Params = ActivityChangedParams;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityChangedParams {
    #[serde(default)]
    pub items: Vec<Activity>,
}

// ---- activity/cancel ---------------------------------------------------------------------------

/// Stop one piece of work — `Ctrl-d` on its row in the activity picker. A shell's command is killed
/// with its whole process group, an agent's turn is cancelled, a git operation is killed.
pub struct ActivityCancel;
impl RpcMethod for ActivityCancel {
    const NAME: &'static str = "activity/cancel";
    type Params = ActivityCancelParams;
    type Result = ActivityCancelResult;
    // Stopping a run edits nothing a client holds: a transcript grows by the runner's own writes.
    const MUTATES_TEXT: bool = false;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityCancelParams {
    pub id: ActivityId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityCancelResult {
    /// False when it was no longer running — it finished between the keystroke and this arriving,
    /// which the client treats as success rather than an error. The convention `git/cancel` and
    /// `shell/cancel` share.
    pub cancelled: bool,
}
