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
    /// Abbreviated (7-char) commit hash. Empty when `is_uncommitted`.
    pub commit: String,
    pub author: String,
    /// Author time as Unix seconds. `0` when `is_uncommitted`.
    pub timestamp: i64,
    /// First line of the commit message. Empty when `is_uncommitted`.
    pub summary: String,
    /// The line is a local, not-yet-committed edit (or a brand-new working-tree line).
    pub is_uncommitted: bool,
}
