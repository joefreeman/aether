//! Buffer lifecycle messages — §6 of the protocol doc.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::ScrollPosition;
use crate::{BufferId, Revision};
use serde::{Deserialize, Serialize};

// ---- buffer/open --------------------------------------------------------------------------------

pub struct BufferOpen;
impl RpcMethod for BufferOpen {
    const NAME: &'static str = "buffer/open";
    type Params = BufferOpenParams;
    type Result = BufferOpenResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BufferOpenParams {
    /// Attach to an already-open buffer by id. When set, `path_index` / `relative_path` /
    /// `create_if_missing` are ignored — the server returns the existing buffer's state. Errors
    /// if the id isn't a live buffer. Used by the buffer picker to switch to a scratch buffer
    /// (which has no path to feed into the path-keyed open flow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    pub language: Option<String>,
    /// When `true` and the target file doesn't exist on disk, the server creates an empty
    /// buffer with the path set but no file on disk yet — the file gets created on the next
    /// `buffer/save`. When `false` (the default) the server errors if the file is missing.
    #[serde(default)]
    pub create_if_missing: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferOpenResult {
    pub buffer_id: BufferId,
    pub language: Option<String>,
    pub line_count: u32,
    pub byte_count: u64,
    pub revision: Revision,
    /// The revision at which this buffer was last persisted to disk (or `0` for a fresh scratch
    /// buffer). The client derives `dirty` as `revision != saved_revision`.
    pub saved_revision: Revision,
    /// Canonical absolute path of the file on disk, when the buffer is backed by one. `None` for
    /// scratch buffers. Lets the client (e.g. file-browser navigation) work in absolute paths.
    pub path: Option<String>,
    /// Server-side cursor state for this `(client, buffer)`. `CursorState::default()` for a buffer
    /// the client hasn't touched yet; the prior position for a buffer the client is reopening.
    #[serde(default)]
    pub cursor: CursorState,
    /// Last scroll position recorded for this `(client, buffer)` on a prior viewport subscription
    /// for this buffer. `None` when the client has never had a viewport on the buffer — the client
    /// should default to `{logical_line: 0, sub_row: 0.0}`. Lets reopen restore the prior view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollPosition>,
}

// ---- buffer/save --------------------------------------------------------------------------------

pub struct BufferSave;
impl RpcMethod for BufferSave {
    const NAME: &'static str = "buffer/save";
    type Params = BufferSaveParams;
    type Result = BufferSaveResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferSaveParams {
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// When the resolved target points at an on-disk file that isn't this buffer's current
    /// path, the server rejects with `WOULD_OVERWRITE` unless this is `true`. The client uses
    /// it as a two-step "ask, then confirm" handshake: first attempt with `false`, and on the
    /// specific error retry with `true` after the user confirms.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferSaveResult {
    pub saved_at_unix_ms: u64,
    pub revision: Revision,
}

// ---- buffer/close -------------------------------------------------------------------------------

pub struct BufferClose;
impl RpcMethod for BufferClose {
    const NAME: &'static str = "buffer/close";
    type Params = BufferCloseParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCloseParams {
    pub buffer_id: BufferId,
}

// ---- buffer/copy & buffer/cut -------------------------------------------------------------------

pub struct BufferCopy;
impl RpcMethod for BufferCopy {
    const NAME: &'static str = "buffer/copy";
    type Params = BufferCopyParams;
    type Result = BufferCopyResult;
}

pub struct BufferCut;
impl RpcMethod for BufferCut {
    const NAME: &'static str = "buffer/cut";
    type Params = BufferCopyParams;
    type Result = BufferCutResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCopyParams {
    pub buffer_id: BufferId,
    pub scope: CopyScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyScope {
    /// The current selection (always ≥1 char in normal mode: an explicit selection if anchor is
    /// set, the implicit 1-char range at the cursor otherwise).
    Selection,
    /// The cursor's current logical line, including its trailing newline.
    Line,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCopyResult {
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCutResult {
    pub text: String,
    pub revision: Revision,
    pub cursor: CursorState,
}

// ---- buffer/state (notification) ----------------------------------------------------------------

pub struct BufferState;
impl NotificationMethod for BufferState {
    const NAME: &'static str = "buffer/state";
    type Params = BufferStateParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferStateParams {
    pub buffer_id: BufferId,
    /// Revision at the most recent successful save. The client derives `dirty` as `revision !=
    /// saved_revision`, so this notification only needs to fire when the saved point changes
    /// (i.e. on save / load), not on every mutation.
    pub saved_revision: Revision,
    pub saved_at_unix_ms: Option<u64>,
}
