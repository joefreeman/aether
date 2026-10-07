//! Server-stateful search over a **view**. The server owns the per-`(client, view)` query, match
//! list and current match; the client sees a summary and lets the server drive navigation. Match
//! highlights ride along with the window renders.
//!
//! A view is searched end to end — every element's text, the removed lines its inline diff draws,
//! and the chrome its builder marks as content (a shell run's command) — and `n` walks the matches
//! in view order, crossing elements. Every match has somewhere it is *painted* and somewhere the
//! cursor can *sit* next to it; for buffer text the two are the same, and the selection is the
//! match exactly as it always was.

use crate::cursor::{CursorState, Direction};
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::picker::MatchOptions;
use crate::ui::FieldId;
use crate::viewport::ViewportFocusElementResult;
use crate::{LogicalPosition, ViewId};
use serde::{Deserialize, Serialize};

// ---- search/set ---------------------------------------------------------------------------------

/// Set (or replace) the active search query for the given view. An empty `query` is equivalent to
/// `search/clear`. The server runs the search, stores the match list, and pushes refreshed
/// highlights to the client's viewport on the view. If `anchor` is provided, the server also makes
/// the first match at-or-after that position current (wrapping if needed) and seats the cursor by
/// it — used during incremental search so the search anchors to where `/` was pressed.
pub struct SearchSet;
impl RpcMethod for SearchSet {
    const NAME: &'static str = "search/set";
    type Params = SearchSetParams;
    type Result = SearchSetResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSetParams {
    pub view_id: ViewId,
    pub query: String,
    pub anchor: Option<SearchAnchor>,
    /// When `true` and `anchor` is set, grow the selection from `anchor` *through* the matched term
    /// (anchor stays at `anchor`, head lands on the match's last char) instead of re-selecting just
    /// the match — this is the `?` "select to match" entry. Only text matches qualify, since a
    /// selection ends on text, and only in the anchor's own element, since a selection lives in one
    /// buffer: when the next one is in another element the cursor stays where it was. When the
    /// match is only found by wrapping past the end, the selection resets to just the match,
    /// mirroring how `search/step` handles a wrap.
    #[serde(default)]
    pub extend: bool,
    /// Derive the query from the current selection instead of `query` (which is ignored): the
    /// server takes the selection's text, regex-escapes it, and searches for it literally — `Alt-/`
    /// in one round-trip. The result's `query` echoes what was searched; `None` there means the
    /// selection was empty and nothing was set.
    #[serde(default)]
    pub from_selection: bool,
    /// How the pattern matches: case mode, whole-word, and regex-vs-literal. Defaults (regex,
    /// smartcase) reproduce the long-standing buffer-search behavior, so an absent field is a
    /// no-op. Toggled in the search prompt (`Alt-c` / `Alt-w` / `Alt-e`) and carried over from a
    /// grep result that primed the search.
    #[serde(default, skip_serializing_if = "MatchOptions::is_default")]
    pub options: MatchOptions,
}

/// Where an incremental search started: a position in one of the view's elements.
///
/// The element is named because the position is a line of *its* buffer, and the search may have
/// moved focus to another element since — every keystroke after the first re-anchors here, wherever
/// the previous one landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchAnchor {
    pub element: FieldId,
    pub position: LogicalPosition,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchSetResult {
    pub cursor: CursorState,
    pub summary: SearchSummary,
    /// With `from_selection`: the effective (regex-escaped) query that was set, or `None`
    /// when the selection was empty (no search was set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Set when seating the cursor moved focus to another element — the same rebind a line motion
    /// walking out of its element answers with ([`crate::cursor::CursorMoveResult::crossed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossed: Option<ViewportFocusElementResult>,
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
    pub view_id: ViewId,
}

// ---- search/next & search/prev ------------------------------------------------------------------

/// Step the current match `count` matches in `direction` (`Forward` = next, `Backward` = prev),
/// wrapping at the view's ends, and seat the cursor by it. No-op if there's no active search or no
/// matches. When `extend` is set the anchor stays put and only the cursor head moves to the match,
/// growing the selection — over text matches only, since a selection ends on text, and never into
/// another element, since it lives in one buffer: a step whose next match is elsewhere does
/// nothing.
pub struct SearchStep;
impl RpcMethod for SearchStep {
    const NAME: &'static str = "search/step";
    type Params = SearchStepParams;
    type Result = SearchNavResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchStepParams {
    pub view_id: ViewId,
    /// `Forward` steps to the next match (`n`), `Backward` to the previous (`N`). Defaults to
    /// `Forward`, the common case, and is then omitted on the wire.
    #[serde(default, skip_serializing_if = "crate::is_forward")]
    pub direction: Direction,
    /// Keep the current anchor and move only the cursor head onto the match (`Shift-n` /
    /// `Shift-Alt-n`), so the selection grows from the anchor to the match. When false the
    /// navigation re-selects just the match (anchor at its start, head at its end).
    pub extend: bool,
    /// Step this many matches (`3n`). `0` is treated as `1`. Default `1`.
    #[serde(default = "default_nav_count", skip_serializing_if = "is_one")]
    pub count: u32,
    /// Set this query first (`search/set` with no anchor), then step — the history-revive chain
    /// (`n` after the search was dropped) folded into one round-trip. When the revived query has no
    /// matches, the step is skipped and the zero-total summary comes back as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_query: Option<String>,
    /// Match options for the `set_query` revive (ignored without it) — the options the revived
    /// history entry recorded, so it matches the way it did before it was dropped (a regex revived
    /// as a literal would quietly find nothing). Defaults (literal, smartcase) when absent.
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

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchNavResult {
    pub cursor: CursorState,
    pub summary: SearchSummary,
    /// Set when the step moved focus to another element. See [`SearchSetResult::crossed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossed: Option<ViewportFocusElementResult>,
}

// ---- summary + notification ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchSummary {
    pub view_id: ViewId,
    /// Matches the view is showing (or `MAX_MATCHES` when `truncated` is true) — what `n` walks.
    pub total: u32,
    /// True when the server hit its match cap and the actual match count exceeds `total`.
    pub truncated: bool,
    /// 1-based index of the current match, in the order `total` counts. `0` when there is none.
    ///
    /// Set by a step or an incremental keystroke, and otherwise the match the cursor head sits
    /// inside — so it stays live across `?` / `Shift-n` selections spanning several matches, and
    /// drops when the cursor moves off a match it was never in (a removed line above it).
    pub current_index: u32,
    /// Matches inside elements folded shut in this viewport. Counted apart from `total` because
    /// nothing of them is on screen and `n` does not visit them; their count rides the folded
    /// element's title.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub folded: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Pushed when a search's summary changes without anything re-rendering: the cursor crossing a
/// match boundary, or an edit outside the slice a viewport has loaded (which pushes
/// [`crate::buffer::BufferChanged`] rather than a window). Every other change to a search — a
/// recompute after an edit in view, a view rebuilt under it — arrives on the window that re-render
/// carries ([`crate::viewport::Window::search`]).
pub struct SearchStateChanged;
impl NotificationMethod for SearchStateChanged {
    const NAME: &'static str = "search/state_changed";
    type Params = SearchSummary;
}

// ---- painted match range -------------------------------------------------------------------------

/// Byte range covered by a search match within one painted run of text — a logical line, a removed
/// line of the inline diff, or a piece of chrome. Multi-line matches show up as one entry per line
/// they touch, each carrying the same `index`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct SearchMatchRange {
    pub start: u32,
    pub end: u32,
    /// The match's 1-based position in the order [`SearchSummary::total`] counts. A shell paints the
    /// range whose index is the summary's `current_index` as the current match, so stepping costs no
    /// re-render. `0` for ranges that are not a search's (symbol highlights), which are never
    /// current.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub index: u32,
}
