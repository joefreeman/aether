//! `shell/*` — a transcript of non-interactive commands and their output, with an input under it.
//!
//! **Not a terminal.** There is no pty, no interactive program, no colour: a command runs to
//! completion under the user's shell, its output is appended to a read-only transcript, and the
//! next command is typed into an ordinary editable document bound as the view's last element.
//! That is what lets the whole thing be a *view* built out of the pieces a composed view already
//! has — elements over buffers, chrome above each — rather than a second rendering path.
//!
//! The scope names what it is, not who drives it, like `git/*` and `lsp/*`. Three methods and one
//! push is the whole surface: the command text is already server-side (it is the input document),
//! so `shell/run` carries no `command`, and the output itself rides the existing
//! `viewport/lines_changed` rather than inventing a content push of its own.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::ui::FieldId;
use crate::ViewId;
use serde::{Deserialize, Serialize};

/// One run within one shell, unique for the life of the view. Not a global id: a run is only ever
/// named alongside the view it belongs to.
pub type RunId = u64;

// ---- shell/open --------------------------------------------------------------------------------

/// Mint a shell — `Space Alt-t`.
///
/// **Always creates.** "New" is the explicit half of the pair: `Space t` opens the shells picker,
/// which is how you get back to one you already have. The old "focused idle shell, else the MRU
/// idle one, else a new one" heuristic went with the picker — it existed because there was no way
/// to *list* the shells, and it made the same key mean two different things depending on state the
/// user could not see.
pub struct ShellOpen;
impl RpcMethod for ShellOpen {
    const NAME: &'static str = "shell/open";
    type Params = ShellOpenParams;
    type Result = ShellOpenResult;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellOpenParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOpenResult {
    /// The opened view, in the same shape `git/show` returns — so the client's adopt path is
    /// identical and nothing about a shell needs its own opening ceremony.
    pub opened: crate::view::ViewOpenResult,
    /// Which element of the view is the input, so the open can land the cursor there without
    /// re-deriving it from the window it has not received yet. The same number the window's
    /// [`crate::ui::ElementRole::Input`] marks.
    pub input: FieldId,
}

// ---- shell/run ---------------------------------------------------------------------------------

/// Run what is typed in the shell's input — `Enter` in the input element.
///
/// **No `command` parameter.** The text is the input document's, which the server already holds;
/// sending it back would make the client the authority on content it does not own. The server
/// trims it, refuses an empty or whitespace-only input, clears the input through the ordinary
/// edit path, appends a run and starts it.
///
/// One run at a time per shell: submitting while one is active fails with
/// [`crate::error::ErrorCode::SHELL_BUSY`], and the typed text is deliberately left alone so
/// typing ahead costs nothing.
pub struct ShellRun;
impl RpcMethod for ShellRun {
    const NAME: &'static str = "shell/run";
    type Params = ShellRunParams;
    type Result = ShellRunResult;
    // The buffer this *names* is the shell view's read-only transcript; the document it edits is
    // the input, which is an ordinary one. Declaring it a mutation would have the client decline
    // every `Enter` locally — see `RpcMethod::MUTATES_TEXT`.
    const MUTATES_TEXT: bool = false;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunParams {
    pub view_id: ViewId,
}

/// What the line became. `run` is the run it started, or `None` for a line that changed the
/// shell's state without running anything — a directory change, which moves the directory in the
/// input's title and leaves no box behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}

// ---- shell/cancel ------------------------------------------------------------------------------

/// Stop the shell's running command — reached by `Space v c` through [`crate::view::ViewInterrupt`]. Kills the whole process group, so a
/// `cargo build` goes with the `sh` that started it.
pub struct ShellCancel;
impl RpcMethod for ShellCancel {
    const NAME: &'static str = "shell/cancel";
    type Params = ShellCancelParams;
    type Result = ShellCancelResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCancelParams {
    pub view_id: ViewId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCancelResult {
    /// False when nothing was running — the command finished between the keystroke and this
    /// arriving, which the client treats as success rather than an error. Same convention as
    /// `git/cancel`.
    pub cancelled: bool,
}

// ---- shell/run_changed (notification) ----------------------------------------------------------

/// Pushed when a shell's run starts and when it finishes. `run: None` means the shell is idle
/// again — the client clears its indicator without needing to know how it ended.
///
/// Mirrors `git/operation_changed`. The output itself does **not** ride here: it is buffer text
/// of the transcript document, and travels as `viewport/lines_changed` like any other content.
pub struct ShellRunChanged;
impl NotificationMethod for ShellRunChanged {
    const NAME: &'static str = "shell/run_changed";
    type Params = ShellRunChangedParams;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunChangedParams {
    pub view_id: ViewId,
    /// The run that just started, or the finished state of the one that just ended. `None` is not
    /// sent — a finish carries its outcome so a client that is looking elsewhere can say how it
    /// went — except where the shell is closing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunState>,
}

/// What one run is: the command as submitted, and where it has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub run: RunId,
    /// The command as typed, trimmed. Shown in the run's header and in the status indicator.
    pub command: String,
    pub status: RunStatus,
}

impl RunState {
    pub fn is_running(&self) -> bool {
        matches!(self.status, RunStatus::Running)
    }
}

/// How a run ended, or that it hasn't.
///
/// A closed enum, tagged: every shell matches it exhaustively, and a status added later cannot
/// render as a blank in the header of one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    /// The child exited on its own. `code` is the process exit status.
    Exited {
        code: i32,
    },
    /// Killed — by `shell/cancel`, by the view closing, or by a signal.
    Killed,
    /// Output hit the per-run cap and the group was killed. Distinct from `Killed` because the
    /// transcript is *incomplete*, which is a thing the reader has to be told.
    Truncated,
}

impl RunStatus {
    /// One-word label for a run's header — what a shell paints after the command.
    pub fn label(self) -> String {
        match self {
            RunStatus::Running => "running".into(),
            RunStatus::Exited { code: 0 } => "ok".into(),
            RunStatus::Exited { code } => format!("exit {code}"),
            RunStatus::Killed => "killed".into(),
            RunStatus::Truncated => "truncated".into(),
        }
    }
}
