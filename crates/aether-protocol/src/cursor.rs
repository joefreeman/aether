//! Cursor & selection messages — §9 of the protocol doc.
//!
//! `Motion` is shared with `input/delete` (§8.2).

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, ClientId, LogicalPosition, Revision, ViewportId};
use serde::{Deserialize, Serialize};

// ---- Motion vocabulary --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
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
        /// When `true`, a `Forward` motion stops one character before the start of the next word
        /// (so a selection built from this motion doesn't include the next word's first char).
        /// Ignored for `Backward` — the analogous "stop just past the previous word" position is
        /// already what `WordEnd { Backward }` returns.
        exclusive: bool,
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
    /// Jump to the next per-language "navigation unit" past the cursor (functions, structs,
    /// HTML elements, CSS rule sets, etc. — see `LanguageConfig::navigation_kinds` on the
    /// server). The cursor's position implicitly determines the level: inside a method, `]`
    /// skips to the next method in the same class; on a class header, `]` skips to the next
    /// top-level item; at the last unit in a container, `]` is a no-op rather than
    /// crossing the scope boundary. Depth is preserved across presses.
    NextNavigationUnit,
    /// Mirror of [`NextNavigationUnit`].
    PrevNavigationUnit,
    /// Jump to the last char of the smallest navigation unit containing the cursor. Paired
    /// with shift-extend on the TUI, this is "select to end of current function / element /
    /// rule set". No-op when the cursor isn't inside any navigation unit (e.g. on a blank
    /// line between top-level items).
    EndOfNavigationUnit,
    /// Mirror of [`EndOfNavigationUnit`] — jump to the first char of the enclosing unit.
    StartOfNavigationUnit,
    // Tree-sitter motions are added when phase 2 lands.
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

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorSetParams {
    pub buffer_id: BufferId,
    pub position: LogicalPosition,
    /// Other end of the selection. Pass `anchor == position` to collapse to a point.
    pub anchor: LogicalPosition,
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CursorUndoResult {
    pub applied: bool,
    pub cursor: CursorState,
}

// ---- cursor/expand and cursor/contract ---------------------------------------------------------
//
// Tree-sitter–driven selection expansion (Helix `Alt-o`-style). `expand` grows the selection to
// the smallest enclosing syntax node strictly larger than the current selection. `contract`
// reverses one step. The server maintains a per-(client, buffer) history so a chain of expands
// can be undone by an equal number of contracts. Any other cursor RPC (or buffer mutation)
// clears the history.

pub struct CursorExpand;
impl RpcMethod for CursorExpand {
    const NAME: &'static str = "cursor/expand";
    type Params = CursorBufferOnlyParams;
    type Result = CursorState;
}

pub struct CursorContract;
impl RpcMethod for CursorContract {
    const NAME: &'static str = "cursor/contract";
    type Params = CursorBufferOnlyParams;
    type Result = CursorState;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorBufferOnlyParams {
    pub buffer_id: BufferId,
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

// ---- cursor/update (notification) ---------------------------------------------------------------

pub struct CursorUpdate;
impl NotificationMethod for CursorUpdate {
    const NAME: &'static str = "cursor/update";
    type Params = CursorUpdateParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CursorUpdateParams {
    pub buffer_id: BufferId,
    pub client_id: ClientId,
    pub revision: Revision,
    pub position: LogicalPosition,
    pub anchor: LogicalPosition,
}
