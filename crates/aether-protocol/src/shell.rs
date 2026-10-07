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

// ---- shell/start -------------------------------------------------------------------------------

/// Start a shell.
///
/// **Creates**, unless [`ShellStartParams::reuse`] names a shell by what it shows. "Start" rather
/// than "open" because opening is `view/open`'s: showing a view that exists, which is how the
/// shells picker gets you back to one you already have. The old "focused idle shell, else the MRU idle one, else a new one" heuristic
/// went with the picker — it existed because there was no way to *list* the shells, and it made the
/// same key mean two different things depending on state the user could not see. `reuse` is not
/// that: it matches a directory and a last command, which the shells picker's rows display.
pub struct ShellStart;
impl RpcMethod for ShellStart {
    const NAME: &'static str = "shell/start";
    type Params = ShellStartParams;
    type Result = ShellStartResult;
}

/// All optional: `{}` is the new-shell key's empty shell where a new one starts. The fields are what
/// makes a **task** a shortcut for starting a shell rather than a mechanism of its own — the tasks
/// picker opens one in the task's directory with its command typed, and runs it unless asked not
/// to, in the shell that ran it last if there is one (`reuse`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellStartParams {
    /// Absolute directory to start in, in place of the one a new shell would pick. Must exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Text to put in the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Run the input at once, exactly as `Enter` in it would ([`ShellRun`]). Ignored without
    /// `input`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run: bool,
    /// Rather than minting a shell, use one of this workspace's that is **in `cwd` and whose last
    /// command was `input`** — live or restored, the first in the shells picker's order. Nothing
    /// links a shell to a task: the match is read off what the shell shows, so running anything
    /// else in it lets it go, and `input` typed by hand counts the same. On a match, `run` runs the
    /// line without touching what is typed in that shell's input, and without `run` the line
    /// replaces it. A match that is still running is switched to and nothing more
    /// ([`NotRun::Busy`]). Ignored without both `cwd` and `input`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reuse: bool,
}

/// Why an asked-for run did not start. The shell is open either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotRun {
    /// The line did not parse, or named a command that is not there.
    Refused { message: String },
    /// The shell asked for is running something already — its message names what, and the key
    /// that stops it.
    Busy { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellStartResult {
    /// The opened view, in the same shape `git/show` returns — so the client's adopt path is
    /// identical and nothing about a shell needs its own opening ceremony.
    pub opened: crate::view::ViewOpenResult,
    /// Which element of the view is the input, so the start can land the cursor there without
    /// re-deriving it from the window it has not received yet. The same number the window's
    /// [`crate::ui::ElementRole::Input`] marks.
    pub input: FieldId,
    /// Why an asked-for run did not start. The shell is open regardless — for a refusal in a new
    /// shell, with the text still in its input and the word at fault selected, exactly as a refused
    /// `Enter` leaves it: failing the whole start would throw away the one place the line can be
    /// fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_run: Option<NotRun>,
}

// ---- shell/run ---------------------------------------------------------------------------------

/// Run what is typed in the shell's input — `Enter` in the input element.
///
/// **No `command` parameter.** The text is the input document's, which the server already holds;
/// sending it back would make the client the authority on content it does not own. The server
/// trims it, refuses an empty or whitespace-only input, clears the input through the ordinary
/// edit path, appends a run and starts it.
///
/// One run at a time per shell: a line submitted while one is going — or while others are already
/// waiting — is **queued** behind them, a box of its own saying so, and starts when everything
/// ahead of it has finished, however that went. A queued line is only parsed at `Enter`; whether
/// its command and files are there is asked when it starts, since what runs ahead of it may be
/// what makes them. A directory change is the exception: it is applied at once, queue or not.
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

/// What the line became. `run` is the run it started or queued, or `None` for a line that changed
/// the shell's state without running anything — a directory change, which moves the directory in
/// the input's title and leaves no box behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}

// ---- shell/cancel ------------------------------------------------------------------------------

/// Stop the shell's running command — reached by `Ctrl-d` on its row in the activity picker,
/// through [`crate::activity::ActivityCancel`], and by the run's own cancel button
/// ([`crate::ui::ViewAction::Cancel`]). Kills the whole process group, so a `cargo build` goes
/// with the `sh` that started it. What is queued behind it starts next.
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

// ---- shell/delete ------------------------------------------------------------------------------

/// Delete a shell — `Ctrl-d` on its row in the shells picker. Stops its process, discards the
/// snapshot it would come back from, and removes the row; live or dormant alike.
///
/// The shell counterpart of [`crate::agent::AgentDelete`], for the same reason: closing a shell's
/// view **keeps** it — the row goes dormant, and opening it again reads the transcript back —
/// while this destroys it. The shapes are a close's because the landing is. Errors for a view that
/// is not a shell.
pub struct ShellDelete;
impl RpcMethod for ShellDelete {
    const NAME: &'static str = "shell/delete";
    type Params = crate::view::ViewCloseParams;
    type Result = crate::view::ViewCloseResult;
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
    /// The shell's name (`Shell 2`), for a client that is not showing it to say which shell it
    /// means — two can be running the same command in different directories.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
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
    /// Killed by a signal nobody here asked for — or caught going by a restart, which is the
    /// same thing to the command.
    Killed,
    /// Stopped because someone asked: `shell/cancel`, the run's own stop button, the view
    /// closing. Apart from `Killed` because a stop you asked for is not a failure.
    Cancelled,
    /// Output hit the per-run cap and the group was killed. Distinct from `Killed` because the
    /// transcript is *incomplete*, which is a thing the reader has to be told.
    Truncated,
    /// A queued line the shell would not accept once its turn came — a command or file that was
    /// still not there. Nothing ran; the run's output says why.
    Refused,
}

impl RunStatus {
    /// One-word label for a run's header — what a shell paints after the command.
    pub fn label(self) -> String {
        match self {
            RunStatus::Running => "running".into(),
            RunStatus::Exited { code: 0 } => "ok".into(),
            RunStatus::Exited { code } => format!("exit {code}"),
            RunStatus::Killed => "killed".into(),
            RunStatus::Cancelled => "cancelled".into(),
            RunStatus::Truncated => "truncated".into(),
            RunStatus::Refused => "not accepted".into(),
        }
    }
}
