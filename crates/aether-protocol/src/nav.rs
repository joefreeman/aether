//! Navigation history (the jump list) — browser-style back/forward across files.
//!
//! Semantics deliberately mirror browser history: a qualifying jump records a back-entry and
//! truncates the forward stack; there is no interior reordering or dedup (only the client's own
//! "this jump didn't move me" check gates recording). This keeps the terminal client and the web
//! client — which rides the *native* browser history — behaving identically.
//!
//! - TUI: drives the server-side list via [`NavRecord`] (before each qualifying navigation) and
//!   [`NavBack`] / [`NavForward`] (the `Alt-Left` / `Alt-Right` keys).
//! - Web: uses native browser history + `popstate`; it only needs [`NavGoto`] to restore a stored
//!   entry (open the buffer, reopening a closed file by path, and restore the full
//!   cursor/selection) without polluting the per-buffer motion-undo (`z`) history.

use crate::buffer::BufferOpenResult;
use crate::cursor::CursorState;
use crate::envelope::RpcMethod;
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// `nav/record` — snapshot the client's current location onto the back stack and clear the forward
/// stack. Called *before* a qualifying (cross-file-capable) navigation. The client owns the
/// "didn't move" decision — it knows both origin and destination — so the server records
/// unconditionally except for collapsing an exact duplicate of the current back-stack top.
pub struct NavRecord;
impl RpcMethod for NavRecord {
    const NAME: &'static str = "nav/record";
    type Params = NavRecordParams;
    type Result = NavRecordResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavRecordParams {
    /// The client's current buffer (where the jump originates). The server reads the cursor for
    /// `(client, buffer)` and maps the path itself. Passed explicitly rather than inferred from a
    /// viewport, since a client may hold several viewports over its lifetime.
    pub buffer_id: BufferId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavRecordResult {
    /// Whether an entry was pushed (false when there was no current location, or it duplicated
    /// the back-stack top).
    pub recorded: bool,
}

/// `nav/back` — step one entry back through the jump list and navigate there.
pub struct NavBack;
impl RpcMethod for NavBack {
    const NAME: &'static str = "nav/back";
    type Params = NavStepParams;
    type Result = NavStepResult;
}

/// `nav/forward` — step one entry forward through the jump list and navigate there.
pub struct NavForward;
impl RpcMethod for NavForward {
    const NAME: &'static str = "nav/forward";
    type Params = NavStepParams;
    type Result = NavStepResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavStepParams {
    /// The client's current buffer, pushed onto the opposite stack as we step (see
    /// [`NavRecordParams::buffer_id`] for why it's explicit).
    pub buffer_id: BufferId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavStepResult {
    /// The buffer to switch to, with its cursor/selection already restored, or `None` when the
    /// end of the stack is reached (nothing to do).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BufferOpenResult>,
}

/// `nav/goto` — open a stored entry (reopening a closed file by `path_index`/`relative_path` when
/// its `buffer_id` is gone) and restore the full cursor/selection *without* recording a motion in
/// the per-buffer `z` history. Used by the web client on `popstate`; the back/forward stacks live
/// in the browser there, so this performs no stack bookkeeping.
pub struct NavGoto;
impl RpcMethod for NavGoto {
    const NAME: &'static str = "nav/goto";
    type Params = NavGotoParams;
    type Result = NavStepResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NavGotoParams {
    /// Preferred reference when the buffer is still open (covers scratch buffers, which have no
    /// path). Falls back to the path fields when it's gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    /// The cursor/selection to restore (anchor + position). Clamped to the buffer's current
    /// bounds server-side. `match_bracket`/`grep_position` are recomputed and may be omitted.
    pub cursor: CursorState,
}
