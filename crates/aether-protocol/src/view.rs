//! View lifecycle messages: presenting a view, closing one, and the state that belongs to the view
//! rather than to the text it shows.
//!
//! A view is what a client is *looking at*; a buffer is the text it edits. The two coincide for an
//! ordinary editor and part company for a composed view (a review windowing one file per hunk), so
//! the scopes stay apart: `view/*` presents, closes and describes what a viewport shows, while
//! `buffer/*` saves, reloads and reads the text underneath.

use crate::buffer::BufferLocation;
use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::ScrollPosition;
use crate::{BufferId, LogicalPosition, Revision};
use serde::{Deserialize, Serialize};

// ---- view/open --------------------------------------------------------------------------------

pub struct ViewOpen;
impl RpcMethod for ViewOpen {
    const NAME: &'static str = "view/open";
    type Params = ViewOpenParams;
    type Result = ViewOpenResult;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewOpenParams {
    /// Present this **view** — one that exists: a picker row, the view a close hands you on to,
    /// the one a link or a history step recorded. Outranks the path fields, which are ignored
    /// with it. With `kind`, the view's *file* as that kind: what `Space u` asks, naming the view
    /// it is leaving. Errors if the id names no view, live or dormant.
    ///
    /// There is no open by buffer: a buffer is what a view shows, and a client never holds one
    /// it did not reach through a view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<crate::ViewId>,
    /// With `view_id`: not that view's own buffer but the one its element `element` windows — the
    /// file a composed view's focused editor shows, presented as its own view (`Enter` in a
    /// review). Named through the view because a file at a revision has no path, and only the
    /// view windowing it can say which it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<u32>,
    /// Which view of a markdown file to present: its editor, or its reader. `None` leaves it to
    /// the server — the file's most recently used view, else one created per the app setting —
    /// except that a `jump_to` open lands in the editor, where a `line:col` means something. A
    /// client sends `Some` only when its route decided: `Space u` asks for the sibling, a followed
    /// `#anchor` for the reader, the web shell's `view=` URL for what it recorded. The file's view
    /// of that kind is reused when it has one, else created. Ignored for any other file, and for a
    /// view a driver built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::ui::ViewKind>,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// Open a file by absolute path, bypassing the `path_index`/`relative_path` workspace-root
    /// resolution. Set only by the workspace-aware open-from-path flow (`workspace/open_path`) and the
    /// goto-definition follow path, where the target may lie *outside* the active workspace's roots —
    /// an "external" buffer. Unlike root-relative opens (which are confined to the workspace boundary
    /// to block `../` traversal), an absolute-path open is allowed to land outside the roots; the
    /// server marks the resulting buffer external (trust-restricted LSP). Git is the wider test —
    /// a file outside every root but inside a repo one of them reaches keeps its baseline, so a
    /// sibling of your root in the same repo still shows its diff and stages. Mutually
    /// exclusive with `path_index`/`relative_path`. Ignored when `view_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute_path: Option<String>,
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
    /// When set together with `jump_to`, the cursor opens as a *selection* — anchor here, cursor at
    /// `jump_to` — instead of a point. Same coordinate conventions / clamping as `jump_to`. Used by
    /// the outline picker to land a symbol's identifier selected. Ignored without `jump_to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_to_anchor: Option<LogicalPosition>,
    /// Transient-view intent. `Some(true)`: if this open *creates* the view, mark it transient —
    /// the server closes it automatically once no viewport shows it anymore, unless it's been
    /// promoted first (an existing view is never demoted). `Some(false)`: promote the view to
    /// permanent. `None` (the default): leave the flag as it is. Views are also promoted by their
    /// first edit, a save, or a user-initiated reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
    /// Record the jump origin (the buffer the client is leaving) onto this client's nav history
    /// before switching — `nav/record` folded into the open, so result-style navigation (picker
    /// selections, goto-definition, fresh scratch) is one round-trip. Ignored if the buffer doesn't
    /// exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_nav_from: Option<BufferId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewOpenResult {
    pub buffer_id: BufferId,
    /// The view this open presented — what the client subscribes to, switches between and closes.
    /// The buffer's most recently used view, or the one this open created (see
    /// [`ViewOpenParams::kind`]). Where a result describes a buffer rather than an open — a
    /// focus move within a composed view — the buffer's most recently used view.
    #[serde(default)]
    pub view_id: crate::ViewId,
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
    /// Small per-workspace display number for a scratch buffer (`(scratch N)`); `None` for
    /// file-backed buffers. The client renders the label from this rather than `buffer_id`,
    /// so the numbers stay small and reset as scratches close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_number: Option<u32>,
    /// Server-side cursor state for this `(client, buffer)`. `CursorState::default` for a buffer
    /// the client hasn't touched yet; the prior position for a buffer the client is reopening.
    #[serde(default)]
    pub cursor: CursorState,
    /// Last scroll position recorded for this `(client, buffer)` on a prior viewport subscription
    /// for this buffer, so reopen restores the prior view. `None` when the client has never had a
    /// viewport on the buffer, or when this open carried a `jump_to` (grep nav, goto-definition,
    /// nav history) — the jump moves the cursor, so the saved scroll predates it and would frame
    /// the wrong region. On `None` the client frames the open cursor (centring on it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollPosition>,
    /// The language server backing this buffer, when one is configured for its language and a
    /// workspace root was found. `None` otherwise. Lets the client show *this buffer's* server
    /// health (servers are keyed by `(language, workspace_root)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_server: Option<crate::lsp::LspServerRef>,
    /// True while the **presented view** is transient (auto-closes once hidden — see
    /// [`ViewOpenParams::transient`]). Promotion mid-session is pushed via `view/state`.
    #[serde(default)]
    pub transient: bool,
    /// Display name for a **virtual** buffer — one with no path and no scratch number, whose
    /// content the server materialised from a revision (`git/show`: a commit's diff, or a file as
    /// of some commit). Rendered verbatim by the client, which otherwise labels a pathless buffer
    /// `(scratch N)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The buffer refuses edits, saves and reloads. Set for virtual buffers: their content is a
    /// snapshot of something immutable, so there is nothing an edit could mean. Enforced
    /// server-side (`apply_edit` and the save/reload handlers); clients surface it and decline
    /// early so a keystroke doesn't cost a round trip to be told no.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// This buffer is a **generated patch** (`git/show` on a commit), not merely read-only — a file
    /// at a revision is read-only too but is ordinary text.
    ///
    /// The client needs the distinction for one reason: `Enter` means "follow what's under the
    /// cursor", and in a patch that resolves through `git/follow_patch_line` rather than through
    /// the language server. Everything else about a patch is server-side.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_patch: bool,
}

// ---- view/close -------------------------------------------------------------------------------

pub struct ViewClose;
impl RpcMethod for ViewClose {
    const NAME: &'static str = "view/close";
    type Params = ViewCloseParams;
    type Result = ViewCloseResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewCloseParams {
    /// The **view** to close. Closing has always addressed the view rather than the text — a patch
    /// closes as a patch, not as one of the files it happens to window. A view whose buffer has
    /// others (a file's reader beside its editor) closes alone, and the buffer stays; the last
    /// view of a buffer closes the buffer with it. A dormant row's reserved view forgets the row.
    pub view_id: crate::ViewId,
    /// Also open the next view (the MRU successor, or a fresh scratch when none remain) and
    /// return it in `opened` — the close-then-attach client chain folded into one round-trip.
    #[serde(default)]
    pub open_next: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewCloseResult {
    /// The next-most-recently-used view in this workspace after the close. `None` when no views
    /// remain — the client should open a fresh scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_view_id: Option<crate::ViewId>,
    /// With `open_next`: the view the client should now show, fully opened (the MRU
    /// successor or a fresh scratch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<ViewOpenResult>,
}

// ---- view/closed (notification) ---------------------------------------------------------------

/// Pushed to a client when a view it currently presents is closed by *another* client (a plain
/// `view/close`, or a path/workspace deletion that tore the buffer down). The receiving client
/// switches to `next_view_id` (its MRU top after the close), or opens a fresh scratch when `None`
/// — the same convention as [`ViewCloseResult`]. Sent to clients with a viewport on the buffer
/// *and* to clients whose active workspace holds it in its MRU without viewing it — the latter is
/// what lets a tethered client, including the `ae --web` waiter, exit on a close it didn't witness;
/// non-matching pushes are ignored client-side, so the broad audience is safe. The client that
/// initiated the close learns the outcome from its RPC result instead.
pub struct ViewClosed;
impl NotificationMethod for ViewClosed {
    const NAME: &'static str = "view/closed";
    type Params = ViewClosedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewClosedParams {
    /// The view that was closed out from under this client.
    pub view_id: crate::ViewId,
    /// The buffer that went with it, when the view was its last — `None` when the buffer stays
    /// for its other views (a file's reader closed beside its editor). What a client's per-buffer
    /// bookkeeping (a prepared commit, a tether) keys on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// The view the client should switch to, or `None` to open a fresh scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_view_id: Option<crate::ViewId>,
    /// Where to switch to, as a **path** — preferred over `next_view_id` when present.
    ///
    /// A worktree rebind knows which file replaces which, but it can only name the replacement by
    /// reserving a *dormant* id, and a dormant id is not stable: the client that asked for the
    /// rebind activates immediately afterwards, and if its landing view is that same file it
    /// materialises the entry under a **different** id. Whoever opens second then asks for an id
    /// that no longer exists.
    ///
    /// A path has no such race. `view/open` on a path already open returns the existing view,
    /// so both clients converge on one buffer whichever order they arrive in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_path: Option<BufferLocation>,
}

// ---- view/set_transient -----------------------------------------------------------------------

pub struct ViewSetTransient;
impl RpcMethod for ViewSetTransient {
    const NAME: &'static str = "view/set_transient";
    type Params = ViewSetTransientParams;
    type Result = ViewSetTransientResult;
}

/// Set a **view's** transient flag explicitly — the `Space k` "keep" toggle.
///
/// Transient is a property of a view: a preview you opened and looked at closes itself once
/// nothing shows it, and a buffer lives exactly as long as some view uses it. Keeping a file's
/// reader leaves its editor as it was, and the other way round.
#[derive(Debug, Serialize, Deserialize)]
pub struct ViewSetTransientParams {
    pub view_id: crate::ViewId,
    /// The transient flag to set. `true` marks the view transient (it auto-closes once no
    /// viewport shows it); `false` pins it permanent. Unlike [`ViewOpenParams::transient`] —
    /// which only ever *promotes* — this flips the flag either way. The server applies it
    /// unconditionally; the client owns the policy that a view with unsaved edits is never marked
    /// transient (auto-close would discard them), mirroring how `view/close` leaves the discard
    /// decision to the client.
    pub transient: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewSetTransientResult {
    /// The view's transient flag after the change — echoes the request so the client can confirm.
    pub transient: bool,
}

// ---- view/state (notification) -----------------------------------------------------------------

/// Pushed to every client presenting a view whose transient flag just changed — the `Space k`
/// keep toggle, or the promotion an edit, a save or a user-initiated reload earns a preview.
///
/// It rides its own notification rather than `buffer/state` because transience is the *view's*
/// and nothing else here is: a file's reader is kept or dropped without touching its editor, and
/// one buffer's views can disagree. Addressed by view id, so a client applies it only to the view
/// it is presenting.
pub struct ViewState;
impl NotificationMethod for ViewState {
    const NAME: &'static str = "view/state";
    type Params = ViewStateParams;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewStateParams {
    pub view_id: crate::ViewId,
    /// True while the view is transient — it auto-closes once no viewport shows it. Flips to
    /// false when the view is promoted: its first edit, a save, a user-initiated reload, an
    /// explicit `view/open { transient: false }`, or `view/set_transient { transient: false }`.
    pub transient: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}
