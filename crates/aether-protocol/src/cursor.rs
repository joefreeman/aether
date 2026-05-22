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
    MatchBracket,
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
    pub anchor: Option<LogicalPosition>,
    /// Bracket pair `(open, close)` related to the cursor — set when the cursor sits on or
    /// inside a bracket-bounded construct, `None` otherwise. Server-populated on every
    /// response that returns `CursorState`; never stored in `state.cursors`. Drives the
    /// client's match-bracket highlight overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_bracket: Option<(LogicalPosition, LogicalPosition)>,
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
    pub anchor: Option<LogicalPosition>,
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
    pub anchor: Option<LogicalPosition>,
}
