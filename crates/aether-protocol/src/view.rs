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
    /// with it. Errors if the id names no view, live or dormant.
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
    /// How to present a markdown file to this client: as the rendered document (`Some(true)`) or
    /// as its source (`Some(false)`). Reading is a **per-client presentation mode** of the file,
    /// not a view of its own: one view, one buffer, and each client looking at it decides for
    /// itself. `None` leaves it to the server — the mode this client last had the file in, else
    /// the mode the file was last shown as by anyone, else the app setting — except that a
    /// `jump_to` open lands in the editor, where a `line:col` means something, unless the client
    /// has the file on screen and is reading it: a jump inside the document being read (its
    /// outline, a reference, a grep hit) stays on the page. A client sends `Some` only when its
    /// route decided: a followed `#anchor` asks to read, the web shell's `as=` URL for what it
    /// recorded, a history step for the mode it left. Ignored for any file that is not markdown,
    /// and for a view a driver built. `Space u` flips the mode in place with
    /// [`ViewSetRead`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
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
    /// Transient-view intent. **An open that says nothing creates a preview**: `None` (the
    /// default) leaves an existing view's flag alone, and a view this open *creates* is transient
    /// — the server closes it automatically once no viewport shows it anymore. `Some(false)` keeps
    /// the view: created permanent, or promoted if it already exists (an existing view is never
    /// demoted by an open). `Some(true)` restates the default for a creating open and does nothing
    /// to an existing one. A view is kept only because the user did something to it: an edit, a
    /// save, a user-initiated reload, the keep toggle (`view/set_transient`), a tethered launch, or
    /// a session row that recorded it kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
    /// Record the jump origin (the buffer the client is leaving) onto this client's nav history
    /// before switching — `nav/record` folded into the open, so result-style navigation (picker
    /// selections, goto-definition, fresh scratch) is one round-trip. Ignored if the buffer doesn't
    /// exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_nav_from: Option<BufferId>,
}

/// The buffer half of what an open answers with — everything about the text a view shows, and
/// nothing about the view. What a focus move within a composed view describes on its own
/// ([`crate::viewport::ViewportFocusElementResult::buffer`]): the element's file has all of this
/// and, as an element, no view of its own to speak of.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferDescription {
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
    /// Small per-workspace display number for a scratch buffer (`(scratch N)`); `None` for
    /// file-backed buffers. The client renders the label from this rather than `buffer_id`,
    /// so the numbers stay small and reset as scratches close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_number: Option<u32>,
    /// Server-side cursor state for this `(client, buffer)`. `CursorState::default` for a buffer
    /// the client hasn't touched yet; the prior position for a buffer the client is reopening.
    #[serde(default)]
    pub cursor: CursorState,
    /// The language server backing this buffer, when one is configured for its language and a
    /// workspace root was found. `None` otherwise. Lets the client show *this buffer's* server
    /// health (servers are keyed by `(language, workspace_root)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_server: Option<crate::lsp::LspServerRef>,
    /// Display name for a **virtual** buffer — one with no path and no scratch number, whose
    /// content the server materialised from a revision (`git/show`: a commit's diff, or a file as
    /// of some commit). Rendered verbatim by the client, which otherwise labels a pathless buffer
    /// `(scratch N)`.
    ///
    /// The name alone: for a file at a revision it is the file's repo-relative path, and the
    /// revision rides in [`Self::commit`] beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The revision this buffer's content is *as of*, abbreviated (`abc1234`) — set only for a
    /// **file at a revision**, whose [`Self::title`] is then the bare path.
    ///
    /// Its own field rather than part of the title because it is painted differently: every shell
    /// renders it muted, after the name (`src/main.rs abc1234`), in the status bar as in the
    /// buffers picker. A commit's *patch* carries `None` — its title is the commit, not a file
    /// shown at one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
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

/// What an open presented: the view, and the buffer it shows. The buffer's fields are flattened
/// onto the wire, so a `view/open` result is the one flat object it always was, and reached through
/// `Deref` here, so `open.buffer_id` is the buffer's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewOpenResult {
    /// The view this open presented — what the client subscribes to, switches between and closes.
    /// The buffer's most recently used view, or the one this open created (see
    /// [`ViewOpenParams::kind`]).
    #[serde(default)]
    pub view_id: crate::ViewId,
    /// Last scroll position recorded for this `(client, buffer)` on a prior viewport subscription
    /// for this buffer, so reopen restores the prior view. `None` when the client has never had a
    /// viewport on the buffer, or when this open carried a `jump_to` (grep nav, goto-definition,
    /// nav history) — the jump moves the cursor, so the saved scroll predates it and would frame
    /// the wrong region. On `None` the client frames the open cursor (centring on it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollPosition>,
    /// True while the **presented view** is transient (auto-closes once hidden — see
    /// [`ViewOpenParams::transient`]). Promotion mid-session is pushed via `view/state`.
    #[serde(default)]
    pub transient: bool,
    /// True when this client is **reading** the file — its window will carry the rendered
    /// document as one prose element rather than lines. Always false for anything but a markdown
    /// file presented on its own. Decided per client (see [`ViewOpenParams::read`]); a client
    /// changes it with [`ViewSetRead`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub read: bool,
    #[serde(flatten)]
    pub buffer: BufferDescription,
}

impl std::ops::Deref for ViewOpenResult {
    type Target = BufferDescription;
    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl std::ops::DerefMut for ViewOpenResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

// ---- view/follow_line -------------------------------------------------------------------------

/// Follow the line under the cursor to whatever it points at — `Enter` in a **composed** view.
///
/// One method, **total** over the kinds of generated content a view can be built from, so a client
/// never has to know which it is looking at: a patch line leads to the file it came from at the
/// revision that side of the diff belongs to (the logic [`crate::git::GitFollowPatchLine`] owns,
/// which stays a method of its own), a shell's transcript line leads to a `path:line:col` printed
/// in it, and anything else answers `None`.
///
/// No position rides here. The cursor is the server's — per `(client, buffer)`, in the focused
/// element's buffer — and it is the same convention every other position-bearing method follows:
/// a client that sent coordinates could disagree with the document they index.
pub struct ViewFollowLine;
impl RpcMethod for ViewFollowLine {
    const NAME: &'static str = "view/follow_line";
    type Params = ViewFollowLineParams;
    type Result = ViewFollowLineResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewFollowLineParams {
    /// The view being read. A view with no generated content answers `opened: None`.
    pub view_id: crate::ViewId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewFollowLineResult {
    /// The file opened, in the same shape every other open returns, with the cursor already on the
    /// place the line named.
    ///
    /// `None` when the line leads nowhere — a patch's metadata block, a line of shell output with
    /// no path in it, a path that doesn't exist. A quiet no-op rather than an error: `Enter` is a
    /// common key and being told off for pressing it on ordinary output would be noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<ViewOpenResult>,
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
    /// Also open the view the close lands on — where a history step back would have gone, or the
    /// MRU successor when the trail has nothing to say, or a fresh scratch when none remain — and
    /// return it in `opened`: the close-then-attach client chain folded into one round-trip.
    #[serde(default)]
    pub open_next: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewCloseResult {
    /// The next-most-recently-used view in this workspace after the close. `None` when no views
    /// remain — the client should open a fresh scratch. Always the MRU answer, whatever `opened`
    /// resolved to: this is what "anything left in this workspace?" is asked with (the ephemeral
    /// close, which leaves the context when nothing remains).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_view_id: Option<crate::ViewId>,
    /// With `open_next`: the view the client should now show, fully opened — the landing the
    /// closing client's own navigation history names, with its cursor restored, else the MRU
    /// successor or a fresh scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<ViewOpenResult>,
}

// ---- view/closed (notification) ---------------------------------------------------------------

/// Pushed to a client when a view it currently presents is closed by *another* client (a plain
/// `view/close`, or a path/workspace deletion that tore the buffer down). The receiving client
/// switches to `next_path` or `next_view_id` — where its *own* history says, when the close was a
/// plain one and the trail can be named in this payload, else its MRU top — or opens a fresh
/// scratch when both are `None`, the same convention as [`ViewCloseResult`]. Sent to clients with
/// a viewport on the buffer *and* to clients whose active workspace holds it in its MRU without
/// viewing it — the latter is what lets a tethered client, including the `ae --web` waiter, exit
/// on a close it didn't witness; non-matching pushes are ignored client-side, so the broad
/// audience is safe. The client that initiated the close learns the outcome from its RPC result
/// instead.
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

// ---- view/set_read ----------------------------------------------------------------------------

/// Flip how **this client** sees a markdown file: read it as the rendered document, or edit its
/// source — the `Space u` toggle.
///
/// A presentation mode of the file for one client, kept server-side beside the cursor (both are
/// "this client's relationship to this buffer"): the server composes the window, and since the
/// reader is one prose element carrying the parse, the server has to know before the first frame.
/// Kept there rather than on the viewport because a viewport is superseded on every switch and
/// would forget; the file is remembered as it was last shown, for this client while the file is
/// open and for everyone as the seed of the next client's first landing. The client re-subscribes
/// after the flip and adopts whatever window comes back, exactly as it does for a wrap toggle —
/// the content anchor it captured first is what keeps the same lines on screen.
///
/// Errors for a file that has no reader — anything but markdown presented on its own.
pub struct ViewSetRead;
impl RpcMethod for ViewSetRead {
    const NAME: &'static str = "view/set_read";
    type Params = ViewSetReadParams;
    type Result = ViewSetReadResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewSetReadParams {
    pub view_id: crate::ViewId,
    /// `true` to read the rendered document, `false` to edit its source.
    pub read: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSetReadResult {
    /// The mode after the change — echoes the request so the client can confirm.
    pub read: bool,
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

// ---- view/submit_input -------------------------------------------------------------------------

/// Submit the focused view's input — `Enter` in an input element.
///
/// **Total over the kinds of composed view**, which is the point: a shell and an agent view both
/// end in an element you type into, and the client cannot tell them apart — deliberately, because
/// the window marks the input by [`crate::ui::ElementRole`] and carries no view kind at all. So the
/// client asks one question and the server decides what submitting means here, exactly as
/// [`ViewFollowLine`] decides what `Enter` on a line means. A view with no input answers
/// `submitted: false` rather than erroring, so a stale route costs nothing.
///
/// `shell/run` and `agent/prompt` remain methods in their own right: the shapes differ, and the
/// tests that pin them are about those shapes rather than about the key that reaches them.
pub struct ViewSubmitInput;
impl RpcMethod for ViewSubmitInput {
    const NAME: &'static str = "view/submit_input";
    type Params = ViewSubmitInputParams;
    type Result = ViewSubmitInputResult;
    // The document this edits is the input, which is an ordinary one; the buffer the view presents
    // is read-only. Declaring a mutation would have the client decline every `Enter` locally.
    const MUTATES_TEXT: bool = false;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewSubmitInputParams {
    pub view_id: crate::ViewId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSubmitInputResult {
    /// False when there was nothing to submit — an empty input, or a view with none.
    pub submitted: bool,
    /// Which recall list the submitted line belongs to, so the client can file it without knowing
    /// what sort of view it was in. `None` when nothing was submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<crate::history::HistoryKind>,
}

// ---- view/interrupt ------------------------------------------------------------------------------

/// Stop whatever the view is running — `Space v c`.
///
/// **Total over the kinds of composed view**, the counterpart of [`ViewSubmitInput`]: a shell has
/// a run and an agent has a turn, the client cannot tell the two apart (the window marks the input
/// by [`crate::ui::ElementRole`] and carries no view kind), so it asks one question and the server
/// decides what stopping means here. A view running nothing — a file, an idle shell, an idle
/// conversation — answers `interrupted: false` rather than erroring, and the client says "nothing
/// is running here" off that one answer instead of guessing from what it thinks the view is.
///
/// [`crate::shell::ShellCancel`] and [`crate::agent::AgentCancel`] remain methods in their own
/// right, for the reason `shell/run` and `agent/prompt` do: the shapes differ, and the tests that
/// pin them are about those shapes rather than about the key that reaches them.
pub struct ViewInterrupt;
impl RpcMethod for ViewInterrupt {
    const NAME: &'static str = "view/interrupt";
    type Params = ViewInterruptParams;
    type Result = ViewInterruptResult;
    // Stopping a run edits nothing: the transcript grows by the runner's own writes, and the
    // buffer the view presents is read-only either way.
    const MUTATES_TEXT: bool = false;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewInterruptParams {
    pub view_id: crate::ViewId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewInterruptResult {
    /// False when nothing was running — including a view that could never run anything.
    pub interrupted: bool,
}
