//! Cursor & selection messages — §9 of the protocol doc.
//!
//! `Motion` is shared with `input/delete` (§8.2).

use crate::envelope::RpcMethod;
use crate::{BufferId, LogicalPosition, ViewportId};
use serde::{Deserialize, Serialize};

// ---- Motion vocabulary --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerticalDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WordBoundary {
    #[serde(rename = "word")]
    Word,
    #[serde(rename = "WORD")]
    BigWord,
    #[serde(rename = "subword")]
    Subword,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Motion {
    Char {
        direction: Direction,
        count: u32,
    },
    Word {
        direction: Direction,
        count: u32,
        boundary: WordBoundary,
    },
    /// Word *end* — moves to the last char of the word (vim's `e`).
    WordEnd {
        direction: Direction,
        count: u32,
        boundary: WordBoundary,
    },
    LogicalLine {
        direction: Direction,
        count: u32,
        preserve_col: bool,
    },
    LineStart,
    LineEnd,
    LineFirstNonblank,
    /// Vim's `+`/`-` — move `count` logical lines forward/backward and land on the first
    /// non-blank char of the target line. Unlike `LogicalLine`, the destination column always
    /// comes from the target line's indentation, never the current column.
    LogicalLineFirstNonblank {
        direction: Direction,
        count: u32,
    },
    BufferStart,
    BufferEnd,
    Goto {
        position: LogicalPosition,
    },
    VisualLine {
        viewport_id: ViewportId,
        direction: VerticalDirection,
        count: u32,
    },
    VisualLineStart {
        viewport_id: ViewportId,
    },
    VisualLineEnd {
        viewport_id: ViewportId,
    },
    /// `f`/`t`/`F`/`T` — move to the `count`-th occurrence of `ch` in the given direction,
    /// scanning across line boundaries. When `till` is `true` the cursor stops one char *before*
    /// the match (for forward) or one *after* (for backward) — the Helix `t`/`T` semantics.
    FindChar {
        ch: char,
        direction: Direction,
        count: u32,
        till: bool,
    },
    /// Jump to the bracket that matches the one at the cursor (or, if the cursor isn't on a
    /// bracket, the bracket that encloses the cursor's position). With `extend_selection`,
    /// produces a selection from the cursor's original position to the matching bracket — the
    /// natural "select around brackets" gesture (Vim's `v%`).
    ///
    /// `inner: true` shifts the target one char *inside* the bracket (so the brackets
    /// themselves are excluded). The handler also toggles direction when the cursor already
    /// sits at one inner side, so `MatchBracket { inner: true }` followed by extend lands on
    /// the opposite inner side — the "select inside brackets" gesture.
    MatchBracket {
        inner: bool,
    },
    /// Step to the next symbol after the cursor in the buffer's LSP document-symbol outline — the
    /// same flat list (document order) the `Space o` picker shows, landing on the symbol's name.
    /// A plain linear walk: nesting doesn't gate it (it freely crosses in and out of containers),
    /// resolved by `cursor::resolve_navigation_motion`. LSP-only — a no-op when the outline hasn't
    /// loaded or the buffer has no language server. `count` walks that many symbols in one go,
    /// stopping at the last one when the outline runs out before the count is met.
    NextNavigationUnit {
        count: u32,
    },
    /// Mirror of [`NextNavigationUnit`] — step `count` symbols back before the cursor.
    PrevNavigationUnit {
        count: u32,
    },
    /// Jump to the last char of the smallest navigation unit containing the cursor. Paired
    /// with shift-extend on the TUI, this is "select to end of current function / element /
    /// rule set". No-op when the cursor isn't inside any navigation unit (e.g. on a blank
    /// line between top-level items).
    EndOfNavigationUnit,
    /// Mirror of [`EndOfNavigationUnit`] — jump to the first char of the enclosing unit.
    StartOfNavigationUnit,
    /// Collapse to an edge of the current selection — the Insert-entry motions (`I`/`A`
    /// family). Unlike the other motions this reads the whole selection (anchor and
    /// cursor), which is exactly why it lives server-side: the client would otherwise
    /// compute selection bounds the server already owns (docs/protocol-composites.md, F).
    SelectionEdge {
        edge: SelectionEdge,
    },
    // Tree-sitter motions are added when phase 2 lands.
}

/// Where [`Motion::SelectionEdge`] lands, relative to the selection's inclusive
/// `[start, end]` char range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionEdge {
    /// The selection's first char.
    Start,
    /// One char *past* the selection's last char (multi-byte and end-of-line handled by
    /// the server's char arithmetic) — the append position.
    AfterEnd,
    /// The first non-blank column of the selection's first line.
    FirstLineNonblank,
    /// One past the last char of the selection's last line — the end-of-line append
    /// position (`col` = the line's byte length excluding the newline).
    LastLineEnd,
}

// ---- cursor/move --------------------------------------------------------------------------------

pub struct CursorMove;
impl RpcMethod for CursorMove {
    const NAME: &'static str = "cursor/move";
    type Params = CursorMoveParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorMoveParams {
    pub buffer_id: BufferId,
    pub motion: Motion,
    pub extend_selection: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    pub position: LogicalPosition,
    /// The other end of the cursor's selection. The selection is always non-empty: it's the
    /// inclusive range `[min(anchor, position), max(anchor, position)]`. When `anchor ==
    /// position` the selection is a single character (the "point" cursor visualised as the
    /// block). In Insert mode the invariant `anchor == position` is maintained — operations
    /// that take a motion (Backspace, Delete) bypass the selection.
    pub anchor: LogicalPosition,
    /// Bracket pair `(open, close)` related to the cursor — set when the cursor sits on or
    /// inside a bracket-bounded construct, `None` otherwise. Server-populated on every
    /// response that returns `CursorState`; never stored in `state.cursors`. Drives the
    /// client's match-bracket highlight overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_bracket: Option<(LogicalPosition, LogicalPosition)>,
    /// `Some` when the cursor is currently sitting on a grep hit from this client's cached
    /// grep results. Carries the 1-based index of the hit (across the whole workspace, not
    /// just the current file) and the total hit count, so the status bar can render `(C/D)`
    /// alongside the in-buffer search counter. `None` when the cursor isn't on any hit or
    /// there are no cached grep results. Derived per-response like `match_bracket`; never
    /// stored in `state.cursors`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep_position: Option<GrepPosition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepPosition {
    /// 1-based index of the hit the cursor is currently on, within the full ordered list of
    /// cached grep hits.
    pub current: u32,
    /// Total number of cached grep hits across the workspace.
    pub total: u32,
}

impl CursorState {
    /// True when the selection covers exactly one char (anchor and position coincide).
    pub fn is_point(&self) -> bool {
        self.anchor == self.position
    }
}

// ---- cursor/set ---------------------------------------------------------------------------------

pub struct CursorSet;
impl RpcMethod for CursorSet {
    const NAME: &'static str = "cursor/set";
    type Params = CursorSetParams;
    type Result = CursorState;
}

/// Snapping unit for `cursor/set` — how far the server expands the given endpoints outward.
/// Drives mouse multi-click selection: single click → `Char`, double → `Word`, triple → `Line`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    /// Use the endpoints exactly as given (clamped to the buffer).
    #[default]
    Char,
    /// Expand each endpoint to the boundary of the same-category char run it sits in (word
    /// chars, symbols, or intra-line whitespace; a newline is its own unit).
    Word,
    /// Expand to the whole-line normal form: `anchor.col == 0`, `cursor.col == line_end`.
    Line,
}

impl Granularity {
    pub fn is_char(&self) -> bool {
        *self == Granularity::Char
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSetParams {
    pub buffer_id: BufferId,
    pub position: LogicalPosition,
    /// Other end of the selection. Pass `anchor == position` to collapse to a point.
    pub anchor: LogicalPosition,
    /// Snap both endpoints outward to this unit. The selection direction (which end the cursor
    /// occupies) is preserved.
    #[serde(default, skip_serializing_if = "Granularity::is_char")]
    pub granularity: Granularity,
}

// ---- cursor/select_line -------------------------------------------------------------------------

pub struct CursorSelectLine;
impl RpcMethod for CursorSelectLine {
    const NAME: &'static str = "cursor/select_line";
    type Params = CursorSelectLineParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSelectLineParams {
    pub buffer_id: BufferId,
    pub direction: Direction,
    pub extend: bool,
    /// Select this many lines (`0` = `1`) — the repeat loop lives server-side.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
}

// ---- cursor/select_word -------------------------------------------------------------------------

/// `w` / `Alt-w` — select a word. The first press grabs the word under the cursor (anchor to its
/// start, cursor to its end); a repeat press advances to the next word. With `extend` the advance
/// keeps the existing anchor, growing the selection by a word instead of replacing it. Single-char
/// words are stepped over rather than dwelt on (a point cursor on a one-char word is
/// indistinguishable from that word already being selected, so the gesture keeps moving forward).
/// Returns the new cursor state. See `resolve_select_word` server-side for the exact rule.
pub struct CursorSelectWord;
impl RpcMethod for CursorSelectWord {
    const NAME: &'static str = "cursor/select_word";
    type Params = CursorSelectWordParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSelectWordParams {
    pub buffer_id: BufferId,
    pub boundary: WordBoundary,
    pub extend: bool,
    /// Repeat the select this many times (`0` = `1`) — the repeat loop lives server-side, so
    /// `3w` selects the third word (or, with `extend`, grows the selection by three words).
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
}

// ---- cursor/undo and cursor/redo ----------------------------------------------------------------
//
// Per-client motion history: rewinds only this client's cursor/selection changes, capped at the
// last buffer mutation. Independent of `input/undo` (which rewinds buffer state).

pub struct CursorUndo;
impl RpcMethod for CursorUndo {
    const NAME: &'static str = "cursor/undo";
    type Params = CursorUndoParams;
    type Result = CursorUndoResult;
}

pub struct CursorRedo;
impl RpcMethod for CursorRedo {
    const NAME: &'static str = "cursor/redo";
    type Params = CursorUndoParams;
    type Result = CursorUndoResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorUndoParams {
    pub buffer_id: BufferId,
    /// Step the motion history this many times (`0` = `1`), stopping early once it's
    /// exhausted — the repeat loop lives server-side.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CursorUndoResult {
    pub applied: bool,
    pub cursor: CursorState,
}

// ---- cursor/tree_select -------------------------------------------------------------------------
//
// Tree-sitter–driven selection resizing (Helix `Alt-o`-style). `Expand` grows the selection to the
// smallest enclosing syntax node strictly larger than the current selection; `Contract` reverses
// one step. The server maintains a per-(client, buffer) history so a chain of expands can be undone
// by an equal number of contracts. Any other cursor RPC (or buffer mutation) clears the history.

pub struct CursorTreeSelect;
impl RpcMethod for CursorTreeSelect {
    const NAME: &'static str = "cursor/tree_select";
    type Params = CursorTreeSelectParams;
    type Result = CursorState;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeSelectDirection {
    /// Grow the selection to the enclosing syntax node.
    Expand,
    /// Shrink one step back along the expand history.
    Contract,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorTreeSelectParams {
    pub buffer_id: BufferId,
    pub direction: TreeSelectDirection,
    /// Repeat the resize this many times (`0` = `1`), stopping early once the cursor stops
    /// changing — the repeat loop lives server-side.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
}

// ---- cursor/swap_anchor -------------------------------------------------------------------------

pub struct CursorSwapAnchor;
impl RpcMethod for CursorSwapAnchor {
    const NAME: &'static str = "cursor/swap_anchor";
    type Params = CursorSwapAnchorParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSwapAnchorParams {
    pub buffer_id: BufferId,
}

// ---- cursor/select_all --------------------------------------------------------------------------

/// Select the whole buffer: anchor at the start `(0, 0)`, cursor at the end of the last line —
/// the whole-line, forward-direction normal form. Returns the new cursor state.
pub struct CursorSelectAll;
impl RpcMethod for CursorSelectAll {
    const NAME: &'static str = "cursor/select_all";
    type Params = CursorSelectAllParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSelectAllParams {
    pub buffer_id: BufferId,
}
