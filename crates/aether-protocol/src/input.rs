//! Input commands — §8 of the protocol doc. Plus undo/redo from §10.
//!
//! All input commands are cursor-relative; none carry positions on the wire. If a selection
//! exists, the command's implicit range is that selection.

use crate::cursor::{CursorState, VerticalDirection};
use crate::envelope::RpcMethod;
use crate::{BufferId, Revision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct EditResult {
    pub revision: Revision,
    /// Cursor position immediately after the edit. Saves the client a round-trip to learn where
    /// the cursor landed.
    pub cursor: CursorState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferOnlyParams {
    pub buffer_id: BufferId,
}

// ---- input/text ---------------------------------------------------------------------------------

pub struct InputText;
impl RpcMethod for InputText {
    const NAME: &'static str = "input/text";
    type Params = InputTextParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputTextParams {
    pub buffer_id: BufferId,
    pub text: String,
    /// If true, the post-edit cursor selects the just-inserted text (used by the paste path).
    /// Default false: cursor lands just past the inserted text with no anchor.
    #[serde(default)]
    pub select_pasted: bool,
}

// ---- input/delete -------------------------------------------------------------------------------

/// Delete the current inclusive selection. For a point cursor (`anchor == position`) this is
/// the 1-char range under the block cursor. Used by Normal-mode `Ctrl-d` / `Delete` /
/// `Ctrl-c`, and by Insert-mode `Delete` (forward) — the point at the cursor IS the char to
/// delete.
pub struct InputDelete;
impl RpcMethod for InputDelete {
    const NAME: &'static str = "input/delete";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

// ---- input/backspace ----------------------------------------------------------------------------

/// Delete the char immediately before the cursor's position and leave the cursor at that
/// position. Used by Insert-mode `Backspace` — there's no meaningful selection in Insert mode,
/// and "delete the previous char" is its own gesture, distinct from "delete the selection".
pub struct InputBackspace;
impl RpcMethod for InputBackspace {
    const NAME: &'static str = "input/backspace";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

// ---- line operations (Insert-mode Ctrl-d / Ctrl-c / Ctrl-r) -------------------------------------

/// Delete the cursor's line entirely — both content and trailing newline. The buffer shrinks
/// by one line; the cursor lands at col 0 of what's now at the line's position (the next line
/// promoted up, or the previous line if we deleted the last line). Insert-mode `Ctrl-d`.
pub struct InputDeleteLine;
impl RpcMethod for InputDeleteLine {
    const NAME: &'static str = "input/delete_line";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

/// Blank the cursor's line — delete its content but keep the line and its newline. Cursor
/// lands at col 0 of the now-empty line. Insert-mode `Ctrl-c` ("change line").
pub struct InputChangeLine;
impl RpcMethod for InputChangeLine {
    const NAME: &'static str = "input/change_line";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

/// Replace the cursor's line (content + newline) with `text`. The clipboard payload usually
/// ends in `\n`; if it doesn't, the replacement is "the line's text becomes `text`, and the
/// newline boundary moves to wherever `text` ends." Insert-mode `Ctrl-r`.
pub struct InputReplaceLine;
impl RpcMethod for InputReplaceLine {
    const NAME: &'static str = "input/replace_line";
    type Params = InputReplaceLineParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputReplaceLineParams {
    pub buffer_id: BufferId,
    pub text: String,
}

// ---- input/indent, input/dedent -----------------------------------------------------------------

pub struct InputIndent;
impl RpcMethod for InputIndent {
    const NAME: &'static str = "input/indent";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

pub struct InputDedent;
impl RpcMethod for InputDedent {
    const NAME: &'static str = "input/dedent";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

// ---- input/newline_and_indent -------------------------------------------------------------------

pub struct InputNewlineAndIndent;
impl RpcMethod for InputNewlineAndIndent {
    const NAME: &'static str = "input/newline_and_indent";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

// ---- input/toggle_comment -----------------------------------------------------------------------

/// Toggle line-comment status on the cursor's line, or — when there's a selection — on every
/// line the selection touches. The server uses the buffer language's `line_comment` prefix
/// (`"//"`, `"#"`, `"%"`, etc.). Languages without a single-line comment form (markdown, html,
/// css, json) make this a no-op.
pub struct InputToggleComment;
impl RpcMethod for InputToggleComment {
    const NAME: &'static str = "input/toggle_comment";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

// ---- input/move_lines ---------------------------------------------------------------------------

/// Move the cursor's line (or, if a selection is active, all lines covered by it) up or down by
/// one, swapping with the adjacent line. The cursor moves with the lines. No-op at the buffer
/// edge.
pub struct InputMoveLines;
impl RpcMethod for InputMoveLines {
    const NAME: &'static str = "input/move_lines";
    type Params = InputMoveLinesParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputMoveLinesParams {
    pub buffer_id: BufferId,
    pub direction: VerticalDirection,
}

// ---- input/surround, input/unsurround -----------------------------------------------------------

/// What a surround/unsurround operates on. Normal mode targets the selection; Insert mode — which
/// has no selection to speak of — targets the cursor's whole line, mirroring the line-scoped
/// `input/delete_line` / `input/change_line` family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurroundTarget {
    /// Wrap/strip the current selection.
    #[default]
    Selection,
    /// Wrap/strip the cursor line's content (excluding the trailing newline).
    Line,
}

/// Wrap the surround target with a delimiter pair (`Ctrl-s <delim>`). `delimiter` is the key typed
/// after `Ctrl-s` — either member of a bracket pair (`(`/`)`, `{`/`}`, `[`/`]`, `<`/`>`), a vim-style
/// alias (`b`/`B`/`r`/`a`), or a symmetric quote (`"`, `'`, `` ` ``). The server resolves it to an
/// open/close pair and replaces the target with `open + <target text> + close` in a single edit. For
/// a selection target the post-edit cursor re-selects just the wrapped text; for a line target it
/// collapses to a point past the close. An unrecognized delimiter is a no-op.
pub struct InputSurround;
impl RpcMethod for InputSurround {
    const NAME: &'static str = "input/surround";
    type Params = InputSurroundParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputSurroundParams {
    pub buffer_id: BufferId,
    pub delimiter: char,
    #[serde(default)]
    pub target: SurroundTarget,
}

/// Strip the delimiter pair immediately hugging the surround target (`Ctrl-Alt-s`) — the inverse of
/// `input/surround`. For a selection target the server checks the single char just outside each end
/// of the selection; for a line target it checks the line content's first and last chars. If they
/// form a known pair it removes both — a selection target leaves the now-inner text selected so
/// repeated presses peel nested layers. If they aren't a pair, it's a no-op.
pub struct InputUnsurround;
impl RpcMethod for InputUnsurround {
    const NAME: &'static str = "input/unsurround";
    type Params = InputUnsurroundParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputUnsurroundParams {
    pub buffer_id: BufferId,
    #[serde(default)]
    pub target: SurroundTarget,
}

// ---- input/undo, input/redo ---------------------------------------------------------------------

pub struct InputUndo;
impl RpcMethod for InputUndo {
    const NAME: &'static str = "input/undo";
    type Params = BufferOnlyParams;
    type Result = UndoResult;
}

pub struct InputRedo;
impl RpcMethod for InputRedo {
    const NAME: &'static str = "input/redo";
    type Params = BufferOnlyParams;
    type Result = UndoResult;
}

// ---- input/join_lines ---------------------------------------------------------------------------

/// Join the current line with the next: drop the line's trailing whitespace + the newline + the
/// next line's leading whitespace, replace with a single space. If a selection spans multiple
/// lines, join all of them.
pub struct InputJoinLines;
impl RpcMethod for InputJoinLines {
    const NAME: &'static str = "input/join_lines";
    type Params = BufferOnlyParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UndoResult {
    pub revision: Revision,
    pub applied: bool,
    /// Cursor position for the requesting client after the operation. When `applied` is `false`
    /// (stack empty), the cursor is unchanged but echoed back for consistency.
    pub cursor: CursorState,
}
