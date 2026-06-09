//! Git messages.
//!
//! Blame is request/response and cursor-driven: the client asks for the blame of the line its
//! cursor sits on (whenever that line changes) and renders the answer as end-of-line virtual
//! text. The server computes blame against the live buffer (folding in unsaved edits), so a line
//! the user just typed reports as uncommitted rather than misattributing to the previous author.

use crate::cursor::CursorState;
use crate::envelope::RpcMethod;
use crate::viewport::ViewportWindowResult;
use crate::{BufferId, ViewportId};
use serde::{Deserialize, Serialize};

// ---- git/navigate_hunk --------------------------------------------------------------------------

pub struct GitNavigateHunk;
impl RpcMethod for GitNavigateHunk {
    const NAME: &'static str = "git/navigate_hunk";
    type Params = GitNavigateHunkParams;
    type Result = GitNavigateHunkResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitNavigateHunkParams {
    pub buffer_id: BufferId,
    /// The cursor's current 0-based line; the search for the next/previous changed region starts
    /// from here.
    pub from_line: u32,
    pub direction: HunkDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkDirection {
    Next,
    Prev,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitNavigateHunkResult {
    /// Cursor after the jump. Equal to the incoming cursor when `moved` is false.
    pub cursor: CursorState,
    /// False when there's no hunk in the requested direction (cursor unchanged).
    pub moved: bool,
}

// ---- change counts (status-bar summary) ---------------------------------------------------------

/// Buffer-wide Git change summary for the status bar: how many buffer lines fall into each change
/// class, measured against the HEAD baseline (Phase 1 — staged and unstaged combined, matching
/// `git diff HEAD`). Mirrors the gutter change-bars: `added` / `modified` count the new-side lines
/// of Added / Modified hunks; `deleted` counts the lines removed by pure deletions. A clean file
/// (or one with no repo / untracked) reports all zeros. Rides the per-viewport `Window` and
/// `viewport/lines_changed` rather than a dedicated notification, since the counts only change when
/// a window is (re)rendered — on open, edit, or external HEAD change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChangeCounts {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

impl GitChangeCounts {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.modified == 0 && self.deleted == 0
    }
}

// ---- git/set_diff_view --------------------------------------------------------------------------

pub struct GitSetDiffView;
impl RpcMethod for GitSetDiffView {
    const NAME: &'static str = "git/set_diff_view";
    type Params = GitSetDiffViewParams;
    /// The freshly re-rendered window: toggling the diff view changes which virtual rows exist and
    /// therefore the visual-row layout and `max_scroll`, so the whole window is resent (like
    /// `viewport/set_wrap`).
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitSetDiffViewParams {
    pub viewport_id: ViewportId,
    pub enabled: bool,
}

// ---- git/blame_line -----------------------------------------------------------------------------

pub struct GitBlameLine;
impl RpcMethod for GitBlameLine {
    const NAME: &'static str = "git/blame_line";
    type Params = GitBlameLineParams;
    type Result = GitBlameLineResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitBlameLineParams {
    pub buffer_id: BufferId,
    /// 0-based buffer line whose blame is wanted.
    pub line: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitBlameLineResult {
    /// `None` when there's no blame for the line: no repo, untracked file, or a line past the
    /// end of the file. An uncommitted line is `Some` with `is_uncommitted = true`.
    pub blame: Option<BlameInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameInfo {
    /// Abbreviated (7-char) commit hash. Empty when `is_uncommitted`. The full message and metadata
    /// are fetched on demand via `git/commit_info` (the blame popover), keyed by this hash.
    pub commit: String,
    pub author: String,
    /// Author time as Unix seconds. `0` when `is_uncommitted`.
    pub timestamp: i64,
    /// The line is a local, not-yet-committed edit (or a brand-new working-tree line).
    pub is_uncommitted: bool,
}

// ---- git/commit_info ----------------------------------------------------------------------------

/// Full details for a single commit, resolved on demand from a hash the client already has (e.g.
/// the abbreviated hash in a line's [`BlameInfo`]). Drives the blame "commit details" popover and
/// is deliberately generic — not blame-specific — so a future log/show view can reuse it.
pub struct GitCommitInfo;
impl RpcMethod for GitCommitInfo {
    const NAME: &'static str = "git/commit_info";
    type Params = GitCommitInfoParams;
    type Result = GitCommitInfoResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitCommitInfoParams {
    /// Used to locate the repo (the commit is resolved in that buffer's repository).
    pub buffer_id: BufferId,
    /// Any revision the repo can parse — typically the abbreviated hash from [`BlameInfo::commit`].
    pub commit: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitCommitInfoResult {
    /// `None` when there's no repo for the buffer or the revision doesn't resolve to a commit.
    pub info: Option<CommitInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full (40-char) commit hash.
    pub commit: String,
    pub author: String,
    pub email: String,
    /// Author date, pre-formatted by the server in the commit's own timezone
    /// (`YYYY-MM-DD HH:MM:SS ±HHMM`), so both clients render it identically without a date library.
    pub date: String,
    /// The complete commit message (subject + body), trailing whitespace trimmed.
    pub message: String,
}

// ---- file status (explorer colouring) -----------------------------------------------------------

/// The Git status of a single file-explorer entry, used to colour it. Folded from libgit2's
/// per-path status flags into one value per entry (the working-tree + index state vs HEAD, matching
/// the gutter's "vs HEAD" model — staged and unstaged are not distinguished here).
///
/// For a **directory** entry this is the highest-priority status among its descendants, so a folder
/// inherits the colour of whatever changed inside it. The priority order is the declaration order
/// below (`Conflicted` highest, `Ignored` lowest): a real change always wins over an ignored
/// sibling, so a tracked folder holding a build artifact still reads as changed rather than gray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitStatus {
    /// A merge conflict (both sides modified the path).
    Conflicted,
    /// Removed from the working tree and/or staged for deletion.
    Deleted,
    /// Tracked and changed (working-tree and/or staged modification, or a rename).
    Modified,
    /// Newly staged (in the index, not in HEAD).
    Added,
    /// Present in the working tree but not tracked.
    Untracked,
    /// Excluded by a `.gitignore` rule (e.g. `target/`, `node_modules/`).
    Ignored,
}
