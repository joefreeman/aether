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

/// Per-class Git change counts: how many buffer lines fall into each change class for one diff.
/// `added` / `modified` count the new-side lines of Added / Modified hunks; `deleted` counts the
/// lines removed by pure deletions. A clean file (or one with no repo / untracked) reports all
/// zeros. Used as the staged/unstaged halves of [`GitBufferStatus`] (the status bar).
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

/// Buffer-level Git status for the status bar: the branch, and the change counts split into staged
/// (HEAD → index) and unstaged (index → working buffer). `Some` for any file inside a repo (the
/// counts are zero for a clean / untracked file); `None` outside a repo. The counts here match
/// `git diff --cached` (staged) and `git diff` (unstaged), so the status bar agrees with the
/// terminal. Distinct from the per-line gutter markers, which the viewport window carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBufferStatus {
    /// Branch name, or a short commit hash when HEAD is detached. `None` only when it can't be
    /// resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Staged changes: HEAD → index (`git diff --cached`).
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub staged: GitChangeCounts,
    /// Unstaged changes: index → working buffer (`git diff`).
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub unstaged: GitChangeCounts,
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

// ---- git/apply_hunk -----------------------------------------------------------------------------

/// Toggle the staged state of — or revert — the change under the cursor or the selected lines.
/// Cursor-relative like the input commands: the server resolves the client's cursor/selection,
/// so no positions ride the wire. A bare cursor (anchor == position) addresses the whole hunk it
/// sits on (a pure deletion belongs to the line its phantom rows render above, or the last line
/// at end-of-buffer); a wider selection is snapped to whole lines and taken at line granularity.
///
/// Toggle writes the repository index and requires a non-dirty buffer (the index must not hold
/// content that exists nowhere on disk — the client tells the user to save first). Revert is an
/// ordinary buffer edit through the undo stack and works on a dirty buffer. The result's
/// [`ApplyHunkStatus`] reports which direction a toggle resolved to.
pub struct GitApplyHunk;
impl RpcMethod for GitApplyHunk {
    const NAME: &'static str = "git/apply_hunk";
    type Params = GitApplyHunkParams;
    type Result = GitApplyHunkResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitApplyHunkParams {
    pub buffer_id: BufferId,
    pub action: HunkAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkAction {
    /// Flip the addressed change's staged state, unstaged-first (mirroring `Revert`'s layering):
    /// anything unstaged in the region is staged (index ← buffer, `git add -p`-style); when the
    /// region holds nothing unstaged, its staged change is pulled back out (index ← HEAD). The
    /// region's stage is visible in the combined view's colours, so the direction is readable
    /// before pressing — and reported back via [`ApplyHunkStatus`] after.
    Toggle,
    /// Restore baseline content in the buffer for the addressed change (undoable edit). Peels the
    /// top layer of the H→I→B change stack: an unstaged change reverts to the index's content;
    /// a staged-only region (buffer == index ≠ HEAD) reverts to HEAD's — pressing again on a
    /// re-modified region therefore peels unstaged first, then staged. View-independent, like
    /// `Toggle`.
    Revert,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GitApplyHunkResult {
    /// Cursor after the action — unchanged for a toggle, clamped into the edited text for
    /// revert. Always echoed so the client can adopt it unconditionally (mirrors `lsp/format`).
    pub cursor: CursorState,
    pub status: ApplyHunkStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyHunkStatus {
    /// A toggle staged the region's unstaged change(s).
    Staged,
    /// A toggle pulled the region's staged change(s) back out of the index.
    Unstaged,
    /// A revert restored baseline content in the buffer.
    Reverted,
    /// No matching change under the cursor / in the selection, in either direction.
    NoChange,
    /// Toggle refused because the buffer has unsaved edits — save first.
    DirtyBuffer,
    /// The buffer isn't in a Git repository (or the index write failed).
    Unavailable,
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
