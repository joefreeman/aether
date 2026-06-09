//! Viewport messages — §7 of the protocol doc.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::git::{GitBufferStatus, GitChangeCounts};
use crate::lsp::{DiagnosticCounts, LspServerStatus};
use crate::search::SearchMatchRange;
use crate::{BufferId, Revision, ViewportId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WrapMode {
    Soft,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScrollPosition {
    pub logical_line: u32,
    pub sub_row: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogicalLineRender {
    pub logical_line: u32,
    pub visual_rows: Vec<VisualRow>,
    /// Per-line byte ranges where the current server-side search query matches. Empty when no
    /// search is active on this buffer for this client. Multi-line matches contribute one entry
    /// to each line they touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_matches: Vec<SearchMatchRange>,
    /// Virtual (non-buffer) rows rendered *above* this logical line, only populated when the
    /// viewport has the inline diff view enabled. Currently these are the baseline lines a hunk
    /// removed or replaced, shown as phantom "deleted" rows that have no cursor position. The
    /// client renders them before the line's `visual_rows` and counts them as occupied screen
    /// rows, but never lets the cursor land on them. Deletions at end-of-buffer anchor to the
    /// trailing empty line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtual_rows_above: Vec<VirtualRow>,
    /// Per-line Git change marker, computed whenever the buffer's hunks are known (independent of
    /// the diff-view toggle) so the client can always draw a gutter change-bar. `Added` /
    /// `Modified` are the new-side lines of those hunks; `Deleted` marks the line a pure deletion
    /// sits above. `None` for unchanged lines. The client also reuses `Added`/`Modified` to tint
    /// the line background while the diff view is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_marker: Option<DiffMarker>,
    /// Language-server diagnostics intersecting this logical line, as byte ranges within the line
    /// (already converted from the server's LSP position encoding). A diagnostic spanning multiple
    /// lines contributes one entry — carrying the full message — to each line it touches, so the
    /// client can underline the span and show the message wherever the cursor lands. Empty when no
    /// diagnostics apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<DiagnosticSpan>,
}

/// One diagnostic's footprint on a single logical line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSpan {
    /// Byte offset within the logical line where the underline starts.
    pub start: u32,
    /// Byte offset within the logical line where the underline ends (exclusive). For a zero-width
    /// diagnostic (`start == end`) the client underlines one cell so it's visible.
    pub end: u32,
    pub severity: DiagnosticSeverity,
    /// The full diagnostic message (repeated on each line the diagnostic covers).
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMarker {
    Added,
    Modified,
    /// Lines were removed immediately above this one (a pure deletion). The line itself is
    /// unchanged — only the gutter flags it; it carries no background tint.
    Deleted,
}

/// A rendered row that doesn't correspond to any buffer line — see
/// [`LogicalLineRender::virtual_rows_above`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualRow {
    pub text: String,
    pub kind: VirtualRowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualRowKind {
    /// A baseline line removed or replaced in the working buffer (inline diff).
    Deleted,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VisualRow {
    /// Byte offset within the *logical line* where this row's text starts. For the first row
    /// of a logical line this is always 0; for continuation rows it's the byte right after the
    /// preceding row's break point. Used by the client to map a cursor's logical column to the
    /// visual row + column it should render on.
    pub byte_offset: u32,
    pub continuation_indent: u32,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Highlight {
    /// Byte offset within the containing `Segment::text`.
    pub start: u32,
    pub end: u32,
    /// Tree-sitter highlight name (e.g. `"keyword"`, `"string"`, `"comment"`).
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Window {
    pub first_logical_line: u32,
    pub last_logical_line_exclusive: u32,
    /// Total number of logical lines in the buffer. Lets the client clamp scroll targets
    /// without round-tripping.
    pub line_count: u32,
    /// Highest legal value for `ScrollPosition.logical_line`: the buffer's last visual row sits
    /// at the bottom of the viewport. Server-computed because under soft wrap each line can
    /// occupy multiple visual rows.
    pub max_scroll_logical_line: u32,
    /// Total visual rows in the whole buffer for this viewport's wrap+cols (real wrapped rows plus
    /// any diff phantom rows). Lets a client size a native scroll container to the full document
    /// (`total_visual_rows × line_height`). Equals `line_count` under `WrapMode::None` with no diff.
    pub total_visual_rows: u32,
    /// Visual-row index at which `first_logical_line` begins (cumulative rows of all lines above
    /// it). Lets a client absolutely-position this window inside the full-height scroller.
    pub first_visual_row: u32,
    /// Display width (in cols) of the buffer's widest line, for sizing a native horizontal scroll
    /// container under `WrapMode::None`. `0` under soft wrap (content always fits `cols`).
    pub max_line_width: u32,
    /// Buffer-wide Git change summary for the status bar (added/modified/deleted line counts vs
    /// HEAD). Omitted from the wire when the buffer is clean / untracked / outside a repo.
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub git_changes: GitChangeCounts,
    /// Buffer-level Git status (branch + staged/unstaged counts) for the status bar. `None` outside
    /// a repo. Rides the window so it updates live on edits, the same way `git_changes` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<GitBufferStatus>,
    pub lines: Vec<LogicalLineRender>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogicalLineRange {
    pub start_logical_line: u32,
    pub end_logical_line_exclusive: u32,
}

// ---- viewport/subscribe -------------------------------------------------------------------------

pub struct ViewportSubscribe;
impl RpcMethod for ViewportSubscribe {
    const NAME: &'static str = "viewport/subscribe";
    type Params = ViewportSubscribeParams;
    type Result = ViewportSubscribeResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSubscribeParams {
    pub buffer_id: BufferId,
    pub cols: u32,
    pub rows: u32,
    pub overscan_rows: u32,
    pub scroll: ScrollPosition,
    pub wrap: WrapMode,
    /// Cols the client reserves at the start of each *continuation* row for a wrap indicator
    /// glyph (e.g. "↪ "). The server subtracts this from the available width on continuation
    /// rows so the visible text + marker fit within `cols`. 0 disables.
    pub continuation_marker_width: u32,
    /// On-screen width of a tab character, in cols. The server uses this for soft-wrap math,
    /// visual-line motions, and centring so its idea of where bytes land matches what the
    /// client actually renders. Most clients will pass 4 or 8; 0 collapses tabs to zero width
    /// (don't do this unless you also strip tabs client-side).
    pub tab_width: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSubscribeResult {
    pub viewport_id: ViewportId,
    pub window: Window,
    /// Buffer-level status, snapshotted at subscribe time. Subscribing is the act of *showing* a
    /// buffer, so it's where a client seeds the buffer-wide state it can't derive from the window:
    /// external-change flags, diagnostic counts, and language-server health. Carried in the
    /// response (not a follow-up notification) so it arrives atomically with the window, with no
    /// ordering race against the editor switch. Live updates then flow through `buffer/state`,
    /// `lsp/diagnostics_changed`, and `lsp/status_changed`.
    #[serde(default)]
    pub buffer_status: BufferStatusSnapshot,
}

/// The buffer-level state a client needs to start showing a buffer, beyond the rendered window —
/// see [`ViewportSubscribeResult::buffer_status`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BufferStatusSnapshot {
    /// File changed on disk while the buffer was dirty (the watcher couldn't silently reload).
    #[serde(default)]
    pub externally_modified: bool,
    /// File was removed on disk.
    #[serde(default)]
    pub externally_deleted: bool,
    /// Per-severity diagnostic counts for the status bar. Empty when none / no language server.
    #[serde(default, skip_serializing_if = "DiagnosticCounts::is_empty")]
    pub diagnostics: DiagnosticCounts,
    /// Health of the language server backing this buffer, if one is attached. `None` for an
    /// unbacked buffer (no server configured / no workspace root / not yet started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_status: Option<LspServerStatus>,
}

// ---- viewport/resize ----------------------------------------------------------------------------

pub struct ViewportResize;
impl RpcMethod for ViewportResize {
    const NAME: &'static str = "viewport/resize";
    type Params = ViewportResizeParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportResizeParams {
    pub viewport_id: ViewportId,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportWindowResult {
    pub window: Window,
}

// ---- viewport/scroll ----------------------------------------------------------------------------

pub struct ViewportScroll;
impl RpcMethod for ViewportScroll {
    const NAME: &'static str = "viewport/scroll";
    type Params = ViewportScrollParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportScrollParams {
    pub viewport_id: ViewportId,
    pub scroll: ScrollPosition,
}

// ---- viewport/scroll_to_row ---------------------------------------------------------------------

/// Scroll so the given absolute visual row is at the top of the viewport. Visual-row-addressed
/// (rather than logical-line) so a client doing native pixel scrolling can map `scrollTop /
/// line_height` straight to a request — the server resolves the visual row to the logical line it
/// falls in and returns the window (with `first_visual_row` for absolute positioning).
pub struct ViewportScrollToRow;
impl RpcMethod for ViewportScrollToRow {
    const NAME: &'static str = "viewport/scroll_to_row";
    type Params = ViewportScrollToRowParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportScrollToRowParams {
    pub viewport_id: ViewportId,
    pub top_visual_row: u32,
}

// ---- viewport/set_wrap --------------------------------------------------------------------------

pub struct ViewportSetWrap;
impl RpcMethod for ViewportSetWrap {
    const NAME: &'static str = "viewport/set_wrap";
    type Params = ViewportSetWrapParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSetWrapParams {
    pub viewport_id: ViewportId,
    pub wrap: WrapMode,
}

// ---- viewport/unsubscribe -----------------------------------------------------------------------

pub struct ViewportUnsubscribe;
impl RpcMethod for ViewportUnsubscribe {
    const NAME: &'static str = "viewport/unsubscribe";
    type Params = ViewportUnsubscribeParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportUnsubscribeParams {
    pub viewport_id: ViewportId,
}

// ---- viewport/lines_changed (notification) ------------------------------------------------------

pub struct ViewportLinesChanged;
impl NotificationMethod for ViewportLinesChanged {
    const NAME: &'static str = "viewport/lines_changed";
    type Params = ViewportLinesChangedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportLinesChangedParams {
    pub viewport_id: ViewportId,
    pub revision: Revision,
    pub range: LogicalLineRange,
    pub replacement_lines: Vec<LogicalLineRender>,
    /// Total line count after the edit. Lets the client keep its `line_count` cache fresh as
    /// edits add/remove lines, so scroll clamping stays accurate.
    pub line_count: u32,
    /// Recomputed maximum legal `scroll_logical_line` after the edit.
    pub max_scroll_logical_line: u32,
    /// Recomputed total visual rows after the edit — lets a native-scrolling client resize its
    /// scroll container (the wrapped height can change when an edit lengthens/shortens a line).
    pub total_visual_rows: u32,
    /// Visual-row index of the changed range's first line, so the client can reposition the window.
    pub first_visual_row: u32,
    /// Recomputed widest-line width (cols) after the edit, for native horizontal scroll sizing.
    pub max_line_width: u32,
    /// Recomputed buffer-wide Git change summary for the status bar. Omitted when the buffer is
    /// clean / untracked / outside a repo.
    #[serde(default, skip_serializing_if = "GitChangeCounts::is_empty")]
    pub git_changes: GitChangeCounts,
    /// Recomputed buffer-level Git status (branch + staged/unstaged counts). `None` outside a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<GitBufferStatus>,
}
