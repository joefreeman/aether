//! Buffer lifecycle messages — §6 of the protocol doc.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::ScrollPosition;
use crate::{BufferId, LogicalPosition, Revision};
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
    /// Place the cursor here after opening, overriding any persisted `CursorState` for this
    /// `(client, buffer)`. Coordinates follow the same conventions as the rest of the protocol
    /// (0-based line, 0-based byte col); out-of-range values are clamped (line to the last line,
    /// col to the line's end). Used by the grep picker to open + jump in one round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_to: Option<LogicalPosition>,
    /// Transient-buffer intent. `Some(true)`: if this open *creates* the buffer, mark it
    /// transient — the server closes it automatically once no viewport shows it anymore, unless
    /// it's been promoted first (an existing buffer is never demoted). `Some(false)`: promote the
    /// buffer to permanent. `None` (the default): leave the flag as it is. Buffers are also
    /// promoted by their first edit, a save, or a user-initiated reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
    /// Record the jump origin (the buffer the client is leaving) onto this client's nav
    /// history before switching — `nav/record` folded into the open, so result-style
    /// navigation (picker selections, goto-definition, fresh scratch) is one round-trip
    /// (docs/protocol-composites.md, A). Ignored if the buffer doesn't exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_nav_from: Option<BufferId>,
    /// Prime the opened buffer's search with this query — a `search/set` anchored at the
    /// post-open cursor (the `jump_to` hit), so the first match at-or-after lands
    /// *selected* and `n`/`Alt-n` step on from it. The result's `cursor` reflects the
    /// selection. Fire-and-forget semantics: pattern errors are dropped. Empty = no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prime_search: Option<String>,
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
    /// Small per-project display number for a scratch buffer (`(scratch N)`); `None` for
    /// file-backed buffers. The client renders the buffer label from this rather than `buffer_id`,
    /// so the numbers stay small and reset as scratches close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_number: Option<u32>,
    /// Server-side cursor state for this `(client, buffer)`. `CursorState::default()` for a buffer
    /// the client hasn't touched yet; the prior position for a buffer the client is reopening.
    #[serde(default)]
    pub cursor: CursorState,
    /// Last scroll position recorded for this `(client, buffer)` on a prior viewport subscription
    /// for this buffer. `None` when the client has never had a viewport on the buffer — the client
    /// should default to `{logical_line: 0, sub_row: 0.0}`. Lets reopen restore the prior view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollPosition>,
    /// The language server backing this buffer, when one is configured for its language and a
    /// workspace root was found. `None` otherwise. Lets the client show *this buffer's* server
    /// health (servers are keyed by `(language, workspace_root)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_server: Option<crate::lsp::LspServerRef>,
    /// True while the buffer is transient (auto-closes once hidden — see
    /// [`BufferOpenParams::transient`]). Promotion mid-session is pushed via `buffer/state`.
    #[serde(default)]
    pub transient: bool,
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
    type Result = BufferCloseResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCloseParams {
    pub buffer_id: BufferId,
    /// Also open the next buffer (the MRU successor, or a fresh scratch when none remain)
    /// and return it in `opened` — the close-then-attach client chain folded into one
    /// round-trip (docs/protocol-composites.md, B).
    #[serde(default)]
    pub open_next: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCloseResult {
    /// The next-most-recently-used buffer in this client's MRU after the close. `None` when
    /// no buffers remain — the client should open a fresh scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_buffer_id: Option<BufferId>,
    /// With `open_next`: the buffer the client should now show, fully opened (the MRU
    /// successor or a fresh scratch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<BufferOpenResult>,
}

// ---- buffer/closed (notification) ---------------------------------------------------------------

/// Pushed to a client when a buffer it currently has open is closed by *another* client (a plain
/// `buffer/close`, or a path/project deletion that tore the buffer down). The receiving client
/// switches to `next_buffer_id` (its MRU top after the close), or opens a fresh scratch when
/// `None` — the same convention as [`BufferCloseResult`]. Only sent to clients that had a viewport
/// on the buffer; the client that initiated the close learns the outcome from its RPC result
/// instead.
pub struct BufferClosed;
impl NotificationMethod for BufferClosed {
    const NAME: &'static str = "buffer/closed";
    type Params = BufferClosedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferClosedParams {
    /// The buffer that was closed out from under this client.
    pub buffer_id: BufferId,
    /// The buffer the client should switch to, or `None` to open a fresh scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_buffer_id: Option<BufferId>,
}

// ---- buffer/reload ------------------------------------------------------------------------------

pub struct BufferReload;
impl RpcMethod for BufferReload {
    const NAME: &'static str = "buffer/reload";
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
    /// True while the buffer is transient (auto-closes once hidden). Flips to false when the
    /// buffer is promoted — by its first edit, a save, a user-initiated reload, or an explicit
    /// `buffer/open { transient: false }`.
    #[serde(default)]
    pub transient: bool,
}
