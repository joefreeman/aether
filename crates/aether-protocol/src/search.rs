//! Server-stateful search. Replaces the old stateless `buffer/search` RPC. The server owns the
//! per-`(client, buffer)` query + match list; the client just sees a summary and lets the server
//! drive navigation. Visible match highlights ride along with viewport line renders.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

// ---- search/set ---------------------------------------------------------------------------------

/// Set (or replace) the active search query for the given buffer. An empty `query` is equivalent
/// to `search/clear`. The server runs the search, stores the match list, and pushes refreshed
/// highlights to every viewport subscribed to this buffer. If `anchor` is provided, the server
/// also moves the cursor onto the first match at-or-after that position (wrapping if needed) —
/// used during incremental search so the cursor anchors to where `/` was pressed.
pub struct SearchSet;
impl RpcMethod for SearchSet {
    const NAME: &'static str = "search/set";
    type Params = SearchSetParams;
    type Result = SearchSetResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSetParams {
    pub buffer_id: BufferId,
    pub query: String,
    pub anchor: Option<LogicalPosition>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSetResult {
    pub cursor: CursorState,
    pub summary: SearchSummary,
}

// ---- search/clear -------------------------------------------------------------------------------

pub struct SearchClear;
impl RpcMethod for SearchClear {
    const NAME: &'static str = "search/clear";
    type Params = SearchClearParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchClearParams {
    pub buffer_id: BufferId,
}

// ---- search/next & search/prev ------------------------------------------------------------------

/// Move the cursor to the next match after its current position (wraps to the first match at the
/// buffer end). No-op if there's no active search or no matches. When `extend` is set the anchor
/// stays put and only the cursor head moves to the match, growing the selection.
pub struct SearchNext;
impl RpcMethod for SearchNext {
    const NAME: &'static str = "search/next";
    type Params = SearchNavParams;
    type Result = SearchNavResult;
}

pub struct SearchPrev;
impl RpcMethod for SearchPrev {
    const NAME: &'static str = "search/prev";
    type Params = SearchNavParams;
    type Result = SearchNavResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchNavParams {
    pub buffer_id: BufferId,
    /// Keep the current anchor and move only the cursor head onto the match (`Shift-n` /
    /// `Shift-Alt-n`), so the selection grows from the anchor to the match. When false the
    /// navigation re-selects just the match (anchor at its start, head at its end).
    pub extend: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchNavResult {
    pub cursor: CursorState,
    pub summary: SearchSummary,
}

// ---- summary + notification ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSummary {
    pub buffer_id: BufferId,
    /// Total matches found (or `MAX_MATCHES` when `truncated` is true).
    pub total: u32,
    /// True when the server hit its match cap and the actual match count exceeds `total`.
    pub truncated: bool,
    /// 1-based index of the match the cursor's selection currently sits on (i.e. the selection's
    /// start equals a match's start). `0` means the cursor isn't on a match.
    pub current_index: u32,
}

/// Pushed when the server-side search state changes in a way the client should know about:
/// matches were recomputed (after a buffer edit), or the cursor crossed a match boundary so the
/// 1-based current index moved. Highlights themselves come via viewport line renders.
pub struct SearchStateChanged;
impl NotificationMethod for SearchStateChanged {
    const NAME: &'static str = "search/state_changed";
    type Params = SearchSummary;
}

// ---- per-line match range (embedded in LogicalLineRender) ---------------------------------------

/// Byte range within a logical line covered by a search match. Multi-line matches show up as one
/// entry per line they touch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SearchMatchRange {
    pub start: u32,
    pub end: u32,
}
