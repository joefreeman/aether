//! Buffer messages: saving, reloading and reading the text a view shows.
//!
//! What a client *presents* is a view, and those messages live in [`crate::view`]. These address
//! the document underneath, which several views can share.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, Revision};
use serde::{Deserialize, Serialize};

// ---- buffer/save --------------------------------------------------------------------------------

pub struct BufferSave;
impl RpcMethod for BufferSave {
    const NAME: &'static str = "buffer/save";
    const MUTATES_TEXT: bool = true;
    type Params = BufferSaveParams;
    type Result = BufferSaveResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferSaveParams {
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// Confirms the user has acknowledged a divergence between buffer state and disk. The
    /// server rejects in three cases unless this is `true`:
    /// - `WOULD_OVERWRITE`: the resolved target points at an on-disk file that isn't this
    ///   buffer's current path.
    /// - `EXTERNALLY_MODIFIED`: the buffer's own file changed on disk since it was last loaded
    ///   or saved.
    /// - `EXTERNALLY_DELETED`: the buffer's own file was removed on disk.
    ///
    /// In each case, the client uses a two-step "ask, then confirm" handshake: attempt with
    /// `false`, present the appropriate prompt for the specific error code, retry with `true`.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BufferSaveResult {
    pub saved_at_unix_ms: u64,
    pub revision: Revision,
}

/// A file inside the workspace, as a root index plus the path relative to that root — the shape
/// `view/open` takes, so a client can pass it straight back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferLocation {
    pub path_index: u32,
    pub relative_path: String,
}

// ---- buffer/reload ------------------------------------------------------------------------------

pub struct BufferReload;
impl RpcMethod for BufferReload {
    const NAME: &'static str = "buffer/reload";
    const MUTATES_TEXT: bool = true;
    type Params = BufferReloadParams;
    type Result = BufferReloadResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferReloadParams {
    pub buffer_id: BufferId,
    /// Confirms the user is willing to discard pending edits. The server rejects with
    /// `WOULD_DISCARD_CHANGES` when the buffer is dirty unless this is `true`. Clean buffers
    /// reload regardless. Two-step handshake mirrors the save-conflict pattern.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferReloadResult {
    /// The revision after reload — always strictly greater than the prior revision.
    pub revision: Revision,
    /// Mtime of the file the reload read from, in unix milliseconds.
    pub saved_at_unix_ms: Option<u64>,
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
    const MUTATES_TEXT: bool = true;
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

// ---- buffer/content -----------------------------------------------------------------------------

pub struct BufferContent;
impl RpcMethod for BufferContent {
    const NAME: &'static str = "buffer/content";
    type Params = BufferContentParams;
    type Result = BufferContentResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferContentParams {
    pub buffer_id: BufferId,
}

/// The buffer's full text at `revision`.
///
/// It exists because the reading view used to render from the whole document — fences, tables and
/// link reference definitions span arbitrarily, so a windowed view of the source cannot drive a
/// parse — and re-fetched on every change. The server parses now and the reader is sent
/// [`Element::Prose`](crate::ui::Element), so no shell calls this any more; what still asks is
/// anything that wants the text *as the buffer holds it*, independent of how a view presents it.
#[derive(Debug, Serialize, Deserialize)]
pub struct BufferContentResult {
    pub revision: Revision,
    pub text: String,
}

// ---- buffer/changed (notification) --------------------------------------------------------------

pub struct BufferChanged;
impl NotificationMethod for BufferChanged {
    const NAME: &'static str = "buffer/changed";
    type Params = BufferChangedParams;
}

/// Revision-only change signal for viewports the edit-push range gate skips. Typed edits push
/// `viewport/lines_changed` only to viewports whose pushed range intersects the edited lines;
/// a client rendering the whole document (the markdown reading view) still needs to hear about
/// every mutation, so the skip branch sends this instead. No window render rides it — clients
/// that draw the pushed window can ignore it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BufferChangedParams {
    pub buffer_id: BufferId,
    pub revision: Revision,
}

// ---- buffer/state (notification) ----------------------------------------------------------------

/// Pushed to every client with a viewport on a buffer when the *document*'s state changes: the
/// saved revision, the external-change flags, or the path a save-as moved it to. Transience is
/// not among them — it belongs to the view and rides [`crate::view::ViewState`].
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
    /// (i.e. on save / load / external reload), not on every mutation.
    pub saved_revision: Revision,
    pub saved_at_unix_ms: Option<u64>,
    /// True when the on-disk file changed externally and the buffer is dirty (so the server
    /// couldn't silently reload). Cleared by a successful save or a `buffer/reload`.
    #[serde(default)]
    pub externally_modified: bool,
    /// True when the on-disk file was removed externally. Cleared by a successful save (which
    /// recreates the file) or by the file being recreated externally.
    #[serde(default)]
    pub externally_deleted: bool,
    /// The buffer's current canonical path on disk (`None` for an unsaved scratch). Carried so a
    /// save-as — which renames the *shared* buffer — relabels every other client viewing it: they
    /// adopt the new path and re-derive their workspace-relative label. Unchanged on in-place
    /// save/reload (the client only adopts a differing path).
    #[serde(default)]
    pub path: Option<String>,
}
