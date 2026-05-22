//! Viewport messages — §7 of the protocol doc.

use crate::envelope::{NotificationMethod, RpcMethod};
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
}
