//! Input commands — §8 of the protocol doc. Plus undo/redo from §10.
//!
//! All input commands are cursor-relative; none carry positions on the wire. If a selection
//! exists, the command's implicit range is that selection.

use crate::cursor::{CursorState, Motion};
use crate::envelope::RpcMethod;
use crate::{BufferId, Revision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct EditResult {
    pub revision: Revision,
    /// Cursor position immediately after the edit. Saves the client a round-trip to learn where
    /// the cursor landed.
    pub cursor: CursorState,
    /// Whether the buffer is dirty after this edit. Reflects revision-vs-saved-revision; undo
    /// back to a saved state can clear this.
    pub dirty: bool,
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
}

// ---- input/delete -------------------------------------------------------------------------------

pub struct InputDelete;
impl RpcMethod for InputDelete {
    const NAME: &'static str = "input/delete";
    type Params = InputDeleteParams;
    type Result = EditResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InputDeleteParams {
    pub buffer_id: BufferId,
    pub motion: Motion,
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
    pub dirty: bool,
}
