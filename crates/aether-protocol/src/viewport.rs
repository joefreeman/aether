//! Viewport messages — §7 of the protocol doc.

use crate::envelope::{NotificationMethod, RpcMethod};
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VisualRow {
    pub continuation_indent: u32,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Serialize, Deserialize)]
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
}
