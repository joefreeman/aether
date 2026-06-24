//! Server-stateful search. Replaces the old stateless `buffer/search` RPC. The server owns the
//! per-`(client, buffer)` query + match list; the client just sees a summary and lets the server
//! drive navigation. Visible match highlights ride along with viewport line renders.

use crate::cursor::{CursorState, Direction};
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::picker::MatchOptions;
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
    /// When `true` and `anchor` is set, grow the selection from `anchor` *through* the matched term
    /// (anchor stays at `anchor`, head lands on the match's last char) instead of re-selecting just
    /// the match — this is the `?` "select to match" entry. Ignored when the match is only found by
    /// wrapping past the buffer end (the selection then resets to just the match, mirroring how
    /// `search/next` handles a wrap) or when `anchor` is `None`.
    #[serde(default)]
    pub extend: bool,
    /// Derive the query from the current selection instead of `query` (which is ignored):
    /// the server takes the selection's text, regex-escapes it, and searches for it
    /// literally — `Alt-/` in one round-trip (docs/protocol-composites.md, H). The result's
    /// `query` echoes what was searched; `None` there means the selection was empty and
    /// nothing was set.
    #[serde(default)]
    pub from_selection: bool,
    /// How the pattern matches: case mode, whole-word, and regex-vs-literal. Defaults (regex,
    /// smartcase) reproduce the long-standing buffer-search behavior, so an absent field is a
    /// no-op. Toggled in the search prompt (`Alt-c` / `Alt-w` / `Alt-e`) and carried over from a
    /// grep result that primed the search.
    #[serde(default, skip_serializing_if = "MatchOptions::is_default")]
    pub options: MatchOptions,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSetResult {
    pub cursor: CursorState,
    pub summary: SearchSummary,
    /// With `from_selection`: the effective (regex-escaped) query that was set, or `None`
    /// when the selection was empty (no search was set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
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

/// Step the cursor `count` matches in `direction` (`Forward` = next, `Backward` = prev), wrapping
/// at the buffer ends. No-op if there's no active search or no matches. When `extend` is set the
/// anchor stays put and only the cursor head moves to the match, growing the selection.
pub struct SearchStep;
impl RpcMethod for SearchStep {
    const NAME: &'static str = "search/step";
    type Params = SearchStepParams;
    type Result = SearchNavResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchStepParams {
    pub buffer_id: BufferId,
    /// `Forward` steps to the next match (`n`), `Backward` to the previous (`N`). Defaults to
    /// `Forward`, the common case, and is then omitted on the wire.
    #[serde(default, skip_serializing_if = "is_forward")]
    pub direction: Direction,
    /// Keep the current anchor and move only the cursor head onto the match (`Shift-n` /
    /// `Shift-Alt-n`), so the selection grows from the anchor to the match. When false the
    /// navigation re-selects just the match (anchor at its start, head at its end).
    pub extend: bool,
    /// Step this many matches (`3n`). `0` is treated as `1`. Default `1`.
    #[serde(default = "default_nav_count", skip_serializing_if = "is_one")]
    pub count: u32,
    /// Set this query first (`search/set` with no anchor), then step — the history-revive
    /// chain (`n` after the search was dropped) folded into one round-trip
    /// (docs/protocol-composites.md, I). When the revived query has no matches, the step is
    /// skipped and the zero-total summary comes back as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_query: Option<String>,
    /// Match options for the `set_query` revive (ignored without it) — the client's sticky search
    /// options, so a revived search matches the way it did before it was dropped. Defaults
    /// (regex, smartcase) when absent.
    #[serde(default, skip_serializing_if = "MatchOptions::is_default")]
    pub options: MatchOptions,
}

fn default_nav_count() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_one(n: &u32) -> bool {
    *n == 1
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_forward(d: &Direction) -> bool {
    matches!(d, Direction::Forward)
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
    /// 1-based index of the match the cursor head currently sits on (the head falls within the
    /// match's bounds). `0` means the head isn't on any match. Stays live even when the selection
    /// spans several matches (`?` / `Shift-n`), since only the head position matters.
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
