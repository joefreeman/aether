//! Application state and message loop.
//!
//! Mirrors the TUI's `app.rs` in miniature, restructured for iced's architecture: key events
//! resolve through `keymap` to `Action`s, actions become RPC `Task`s, and responses /
//! server notifications come back as `Message`s that update state. The scroll model is the web
//! client's: a pixel offset into the full document height, with window fetches when the view
//! nears the loaded range's edge.

use crate::chips::{self, ChipEditor, ChipEditorField, ChipId};
use crate::connection::Handle;
use crate::editor::{self, ClickKind, EditorEvent, GUTTER_COLS, PAD};
use crate::grid;
use crate::picker::{item_key, PickerMsg, PickerState, Reveal, FETCH_LIMIT};
use crate::keymap::{self, Action, InsertWhere, KeyCode, KeyContext, Mods, ScrollDir, ScrollUnit};
use crate::theme;
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferClosed, BufferClosedParams, BufferCopy,
    BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen, BufferOpenParams,
    BufferOpenResult, BufferReload, BufferReloadParams, BufferReloadResult, BufferSave,
    BufferSaveParams, BufferSaveResult, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo,
    CursorUndoParams, Direction, Granularity, Motion,
};
use aether_protocol::cursor::{CursorBufferOnlyParams, CursorContract, CursorExpand};
use aether_protocol::envelope::{NotificationMethod, RpcMethod};
use aether_protocol::git::{
    ApplyHunkStatus, CommitInfo, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult,
    GitBlameLine, GitBlameLineParams, GitCommitInfo, GitCommitInfoParams, GitNavigateHunk,
    GitNavigateHunkParams, GitNavigateHunkResult, GitSetDiffView, GitSetDiffViewParams,
    HunkAction, HunkDirection,
};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputBackspace, InputChangeLine, InputDedent, InputDelete,
    InputDeleteLine, InputIndent, InputJoinLines, InputMoveLines, InputMoveLinesParams,
    InputNewlineAndIndent, InputRedo, InputReplaceLine, InputReplaceLineParams, InputSurround,
    InputSurroundParams, InputText, InputTextParams, InputToggleComment, InputUndo,
    InputUnsurround, InputUnsurroundParams, SurroundTarget, UndoResult,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspFormat, LspFormatResult, LspGotoDefinition,
    LspGotoDefinitionResult, LspHover, LspHoverResult, LspLocation, LspNavigateDiagnostic,
    LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspRestartServer,
    LspRestartServerParams, LspServerRef, LspServerStatus, LspStatus, LspStatusChanged,
};
use aether_protocol::viewport::DiagnosticSeverity;
use aether_protocol::nav::{
    NavBack, NavForward, NavRecord, NavRecordParams, NavStepParams, NavStepResult,
};
use aether_protocol::directory::{DirectoryList, DirectoryListParams, DirectoryListResult};
use aether_protocol::picker::{
    PickerFilters, PickerGrepFileJump, PickerGrepFileJumpParams, PickerGrepNavigate,
    PickerGrepNavigateParams, PickerHide, PickerHideParams, PickerItem, PickerKind, PickerQuery,
    PickerQueryParams, PickerSelect, PickerSelectParams, PickerSelectResult, PickerUpdate,
    PickerUpdateParams, PickerView, PickerViewParams, PickerViewResult, ScopedPath,
};
use aether_protocol::project::{ProjectActivate, ProjectActivateParams, ProjectInfo};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNavResult, SearchNext, SearchPrev,
    SearchSet, SearchSetParams, SearchSetResult, SearchStateChanged, SearchSummary,
};
use aether_protocol::viewport::{
    ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams, ViewportResize,
    ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportScrollToRow,
    ViewportScrollToRowParams, ViewportSetWrap, ViewportSetWrapParams, ViewportSubscribe,
    ViewportSubscribeParams, ViewportSubscribeResult, ViewportWindowResult,
    Window, WrapMode,
};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};
use iced::widget::{column, container, row, text};
use iced::{keyboard, Element, Event, Length, Size, Subscription, Task};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

const TAB_WIDTH: u32 = 4;

pub type NotifRx = Arc<Mutex<mpsc::UnboundedReceiver<aether_protocol::envelope::Notification>>>;

/// What `main` resolves before iced starts. With a project on the CLI, a live connection and
/// an opened buffer ([`SessionBootstrap`]); without one, just the connection — the app opens
/// the project picker and builds the session over it when the user picks ([`ChooseBootstrap`]).
#[derive(Clone)]
pub enum Bootstrap {
    Session(Box<SessionBootstrap>),
    Choose(ChooseBootstrap),
}

/// The live connection and opened buffer for the window's session.
#[derive(Clone)]
pub struct SessionBootstrap {
    pub handle: Handle,
    pub notifications: NotifRx,
    pub client_version: String,
    /// The daemon's start stamp from discovery — reconnects compare it to tell "same daemon,
    /// connection blipped" from "daemon restarted" (where unsaved buffer state died with it).
    pub server_started_at: u64,
    pub project: String,
    pub project_paths: Vec<String>,
    pub buffer: BufferInfo,
}

/// A bare connection for the no-args start: the project picker browses on it, and the picked
/// project's session is built over it.
#[derive(Clone)]
pub struct ChooseBootstrap {
    pub handle: Handle,
    pub notifications: NotifRx,
    pub client_version: String,
    pub server_started_at: u64,
}

/// Pre-session state: the project chooser shown on a no-args start. Owns the connection the
/// session will be built over; all input routes through `update_boot` while this is set.
struct Boot {
    handle: Handle,
    notifications: NotifRx,
    picker: PickerState,
    /// A project was picked and its activation is in flight — input is parked meanwhile.
    opening: bool,
    /// The connection died; a retry loop is dialling. Input is parked until it lands.
    down: bool,
}

/// The session's connection lifecycle. The server is authoritative, so a dead socket just
/// freezes the window: the last buffer view stays rendered, editing input is suspended, and a
/// retry loop —
/// re-running discovery each attempt, since a restarted daemon gets a fresh port — rebuilds
/// the session when the server is back. On localhost the only real disconnect cause *is* a
/// daemon restart, so this is what makes "restart the daemon" seamless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Connected,
    /// The socket died; a backoff retry is in flight. `had_unsaved` remembers whether edits
    /// were pending at disconnect — landing on a *restarted* daemon then means they're gone
    /// (buffers live in daemon memory), which warrants a warning.
    Reconnecting { attempt: u32, had_unsaved: bool },
    /// A live server answered but the session couldn't be re-established (the project is
    /// gone). Terminal — the window stays frozen.
    Failed,
}

/// Backoff before reconnect attempt `attempt`: 250ms doubling to a 5s ceiling, retrying
/// indefinitely — a failed localhost dial is instant and free, and the daemon coming back is
/// the expected outcome, not the exception.
fn reconnect_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((250u64 << attempt.min(5)).min(5000))
}

/// Everything a successful reconnect hands back to rebuild the session.
pub struct Reestablished {
    handle: Handle,
    notifications: NotifRx,
    project: ProjectInfo,
    open: BufferOpenResult,
    server_url: String,
    server_started_at: u64,
}

impl std::fmt::Debug for Reestablished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reestablished").finish_non_exhaustive()
    }
}

/// Why a reconnect attempt didn't produce a session.
#[derive(Debug)]
pub enum ReconnectError {
    /// No daemon reachable (discovery/dial failed) — retry, silently.
    NotUp,
    /// A server answered but re-establishing failed — terminal.
    Fatal(String),
}

/// A fresh boot-chooser connection after its socket died.
pub struct BootConn {
    handle: Handle,
    notifications: NotifRx,
    server_started_at: u64,
}

impl std::fmt::Debug for BootConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootConn").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct BufferInfo {
    pub buffer_id: BufferId,
    pub label: String,
    /// Canonical absolute path on disk; `None` for scratch buffers.
    pub path: Option<String>,
    pub language: Option<String>,
    pub revision: u64,
    pub saved_revision: u64,
    pub cursor: CursorState,
    pub scroll: Option<ScrollPosition>,
    pub transient: bool,
    /// The language server backing this buffer, if any — keys `lsp/status_changed` updates.
    pub lsp_server: Option<LspServerRef>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    Insert,
    Search,
}

/// Client-side search-prompt state; the query/match list itself is server-owned.
#[derive(Default)]
struct SearchState {
    query: String,
    /// Byte cursor within `query`.
    cursor: usize,
    /// A committed search exists (highlights shown, `n`/`Alt-n` cycle it).
    active: bool,
    summary: Option<SearchSummary>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    /// The `?` variant: grow the selection from the entry point to each incremental match.
    extend_to_cursor: bool,
    /// State to restore on Esc, snapshotted when the prompt opens.
    snapshot: Option<SearchSnapshot>,
}

struct SearchSnapshot {
    cursor: CursorState,
    scroll_px: f32,
    query: String,
    active: bool,
}

/// A modal dialog owning the keyboard: the `[y/N]`-style confirmation or the save-as path
/// input. Mirrors the web client's `modal.ts` (Enter/`y` accepts, Esc declines, a click on the
/// editor behind it cancels).
#[derive(Debug)]
enum Prompt {
    Confirm { message: String, action: ConfirmAction },
    SaveAs { path_index: u32, input: String, cursor: usize },
    /// LSP server detail (from the LspServers picker): info rows + `r` to restart.
    LspInfo(Box<LspServerStatus>),
}

/// What accepting a confirmation does.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Retry `buffer/save` with `overwrite: true`; `target` carries the save-as path (None for
    /// the in-place save).
    Save { target: Option<(u32, String)> },
    /// Retry `buffer/reload` with `force: true`.
    ReloadDiscard,
    /// Close the buffer despite unsaved changes.
    CloseDiscard,
}

/// Outcome of a `buffer/save` attempt: saved, or refused pending user confirmation.
#[derive(Debug)]
pub enum SaveTry {
    Saved {
        result: BufferSaveResult,
        target: Option<(u32, String)>,
    },
    NeedsConfirm {
        message: String,
        action: ConfirmAction,
    },
}

/// Outcome of a `buffer/reload` attempt.
#[derive(Debug)]
pub enum ReloadTry {
    Reloaded(BufferReloadResult),
    NeedsConfirm,
}

/// The prompt buttons' message space (buttons need `Clone`, the app `Message` isn't).
#[derive(Debug, Clone, Copy)]
enum PromptMsg {
    Accept,
    Cancel,
}

/// One paragraph of the hover popover; diagnostics colour theirs by severity.
#[derive(Debug)]
struct HoverBlock {
    severity: Option<DiagnosticSeverity>,
    text: String,
}

/// The hover popover's body: plain severity-coloured blocks (diagnostics, commit details) or
/// rendered markdown (LSP hover).
enum HoverContent {
    Blocks(Vec<HoverBlock>),
    Markdown {
        items: Vec<iced::widget::markdown::Item>,
        /// Source line count, for the place-above-or-below estimate.
        est_lines: usize,
    },
}

/// What `Space o`'s blame → commit-info chain resolved to.
#[derive(Debug)]
pub enum CommitDetails {
    Info(Box<CommitInfo>),
    /// No popup — a transient note instead (uncommitted line, no blame, commit not found).
    Note(&'static str),
}

/// An in-flight smooth scroll: `scroll_px` eases from `from` to `to` over
/// [`SCROLL_ANIM_MS`], driven by frame ticks. Mirrors the web client's `scrollTopTo`:
/// only near jumps animate (≤ ~1.5 viewports — long glides would sail over unloaded
/// rows and storm the server with window fetches), wheel input snaps it off.
struct ScrollAnim {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

const SCROLL_ANIM_MS: f32 = 180.0;

#[derive(Clone, Copy, Debug)]
enum Pending {
    None,
    Leader,
    Find {
        dir: Direction,
        till: bool,
        extend: bool,
        count: u32,
    },
    /// `Ctrl-s` armed: the next keystroke names the surround delimiter.
    Surround(SurroundTarget),
}

/// What `r` replays — the TUI's `RepeatTarget`: the binding intent for table actions, the
/// resolved motion (with its target char) for find.
#[derive(Debug, Clone)]
enum RepeatTarget {
    Action { action: Action, count: u32 },
    Find(Motion),
}

#[derive(Debug, Clone, Copy)]
pub enum PasteKind {
    /// Normal-mode `Ctrl-v`: collapse to selection start, insert, select pasted.
    Before { count: u32 },
    /// Normal-mode `Ctrl-r`: insert over the selection (the server replaces it), select pasted.
    Replace { count: u32 },
    /// Insert-mode `Ctrl-v`: plain insert at the caret.
    AtCursor,
    /// Insert-mode `Ctrl-r`: replace the whole line.
    Line,
}

/// Web-client toast kinds; the colour of the toast's accent bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
    Warning,
    Success,
}

#[derive(Debug)]
struct Toast {
    id: u64,
    message: String,
    kind: ToastKind,
}

#[derive(Debug)]
pub enum Message {
    /// The boot chooser's pick resolved: the activated project + the buffer to land on.
    SessionReady(Result<Box<(ProjectInfo, BufferOpenResult)>, String>),
    Editor(EditorEvent),
    Key {
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    },
    ToastExpired(u64),
    /// Fire-and-forget RPC completed (e.g. `search/clear`); result ignored.
    Noop,
    /// Frame tick while a smooth scroll is in flight.
    AnimTick(std::time::Instant),
    /// Incremental `search/set` (cursor follows the match; zero matches revert it).
    SearchApplied(Result<SearchSetResult, String>),
    /// Non-incremental `search/set` (abort-restore, search-from-selection revive): summary
    /// only, the cursor wasn't moved server-side.
    SearchRestored(Result<SearchSetResult, String>),
    SearchNav(Result<SearchNavResult, String>),
    SearchFromSel(Result<Option<(String, SearchSetResult)>, String>),
    /// A buffer switch resolved (nav step, goto-def, close, new scratch, `buffer/closed`): the
    /// `buffer/open` result to rebind the window to.
    Switched(Result<BufferOpenResult, String>),
    /// A grep-driven switch: like [`Message::Switched`] but priming the buffer search with the
    /// grep query so `n`/`Alt-n` step matches. `Ok(None)` = no more hits.
    SwitchedPrimed(Result<Option<(String, BufferOpenResult)>, String>),
    NavDone {
        forward: bool,
        result: Result<NavStepResult, String>,
    },
    Definition(Result<LspGotoDefinitionResult, String>),
    DiagNav(Result<LspNavigateDiagnosticResult, String>),
    HoverInfo(Result<LspHoverResult, String>),
    BlameLine {
        buffer_id: BufferId,
        line: u32,
        result: Result<aether_protocol::git::GitBlameLineResult, String>,
    },
    FormatDone(Result<LspFormatResult, String>),
    CommitLookup(Result<CommitDetails, String>),
    HunkNav(Result<GitNavigateHunkResult, String>),
    HunkApplied {
        action: HunkAction,
        result: Result<GitApplyHunkResult, String>,
    },
    DiffViewSet {
        enabled: bool,
        result: Result<ViewportWindowResult, String>,
    },
    Subscribed(Result<ViewportSubscribeResult, String>),
    WindowUpdate(Result<ViewportWindowResult, String>),
    CursorMsg(Result<CursorState, String>),
    EditDone(Result<EditResult, String>),
    UndoRedoDone(Result<UndoResult, String>),
    SaveTried(Result<SaveTry, String>),
    ReloadTried(Result<ReloadTry, String>),
    PickerViewed {
        initial: bool,
        result: Result<PickerViewResult, String>,
    },
    PickerSelected {
        /// Grep selections prime the opened buffer's search with the picker query.
        prime: Option<String>,
        result: Result<PickerSelectResult, String>,
    },
    /// A picker row was clicked (absolute index) — highlight it and accept.
    PickerClicked(u32),
    /// A filter chip was clicked — select it (virtual selection, like the keyboard path).
    PickerChipClicked(usize),
    /// `directory/list` for the dir-chip editor resolved; `abs` is the staleness key.
    PickerChipListing {
        abs: String,
        result: Result<DirectoryListResult, String>,
    },
    /// `picker/grep_file_jump` resolved: the next/prev file's first hit (None at the ends).
    GrepFileJumped(Result<Option<PickerItem>, String>),
    /// The picker's results list scrolled natively (absolute y in px).
    PickerScrolled(f32),
    /// Pointer entered (`Some(abs)`) or left (`None`-if-still-current, see mapping) a row.
    PickerHovered(Option<u32>),
    PickerUnhovered(u32),
    /// Project switch resolved: the activated project + the buffer to land on.
    ProjectActivated(Result<(ProjectInfo, BufferOpenResult), String>),
    /// The prompt's Yes/Save button (keyboard accept routes through `on_prompt_key`).
    PromptAccept,
    PromptCancel,
    CopyDone(Result<BufferCopyResult, String>),
    CutDone(Result<BufferCutResult, String>),
    ClipboardRead(PasteKind, Option<String>),
    Notified(Option<aether_protocol::envelope::Notification>),
    /// A reconnect attempt resolved (the backoff sleep rides inside the attempt task).
    Reconnected(Result<Box<Reestablished>, ReconnectError>),
    /// The boot chooser's reconnect attempt resolved.
    BootReconnected(Result<BootConn, String>),
}

impl Session {
    fn new(
        handle: Handle,
        notifications: NotifRx,
        project: String,
        project_paths: Vec<String>,
        buffer: BufferInfo,
    ) -> Self {
        Session {
            handle,
            notifications,
            project,
            project_paths,
            buffer,
            mode: Mode::Normal,
            pending: Pending::None,
            count: None,
            last_repeat: None,
            search: SearchState::default(),
            viewport_id: None,
            window: None,
            wrap: WrapMode::Soft,
            diff_view: false,
            diagnostics: DiagnosticCounts::default(),
            lsp: None,
            externally_modified: false,
            externally_deleted: false,
            sent_grid: None,
            subscribe_scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            scroll_px: 0.0,
            scroll_x_px: 0.0,
            scroll_anim: None,
            fetch_in_flight: false,
            refetch_queued: false,
            reveal_after_fetch: false,
            drag: None,
            hover: None,
            blame: None,
            blame_requested: None,
            prompt: None,
            picker: None,
            conn: ConnState::Connected,
        }
    }

    /// An inert stand-in for the boot chooser (no project picked yet): never rendered and
    /// never addressed — `update_boot` owns every message while `App.boot` is set.
    fn placeholder(handle: Handle, notifications: NotifRx) -> Self {
        Session::new(
            handle,
            notifications,
            String::new(),
            Vec::new(),
            BufferInfo {
                buffer_id: 0,
                label: String::new(),
                path: None,
                language: None,
                revision: 0,
                saved_revision: 0,
                cursor: CursorState::default(),
                scroll: None,
                transient: false,
                lsp_server: None,
            },
        )
    }
}

/// The window's editing context over its server connection — exactly what the server calls a
/// client. `App` holds the window-level shell (chrome, toasts, metrics) around it.
pub struct Session {

    handle: Handle,
    notifications: NotifRx,
    project: String,
    project_paths: Vec<String>,
    buffer: BufferInfo,
    mode: Mode,
    pending: Pending,
    count: Option<u32>,
    last_repeat: Option<RepeatTarget>,
    search: SearchState,

    viewport_id: Option<ViewportId>,
    window: Option<Window>,
    wrap: WrapMode,
    /// Inline diff view toggle — sticky across buffer switches (re-enabled after each
    /// subscribe), like the TUI's `ViewSettings`.
    diff_view: bool,
    diagnostics: DiagnosticCounts,
    lsp: Option<LspServerStatus>,
    externally_modified: bool,
    externally_deleted: bool,
    sent_grid: Option<(u32, u32)>, // (cols, rows) last sent to the server
    /// The scroll position the in-flight `viewport/subscribe` asked for; seeds `scroll_px`
    /// when the result arrives.
    subscribe_scroll: ScrollPosition,
    scroll_px: f32,
    /// Horizontal scroll in px (`wrap: none` only; soft wrap always fits the viewport).
    scroll_x_px: f32,
    scroll_anim: Option<ScrollAnim>,
    fetch_in_flight: bool,
    refetch_queued: bool,
    reveal_after_fetch: bool,
    drag: Option<(LogicalPosition, Granularity)>,
    /// The hover popover (hover info / diagnostics-at-cursor / commit details), anchored at
    /// the cursor. Dismissed by any key, click, or scroll.
    hover: Option<HoverContent>,
    /// Cursor-line blame, rendered as dim text after the line: `(line, "author · age")`.
    blame: Option<(u32, String)>,
    /// The `(line, revision)` the in-flight/most-recent blame request was for.
    blame_requested: Option<(u32, u64)>,
    /// A modal confirm / save-as dialog; owns the keyboard while open.
    prompt: Option<Prompt>,
    /// An open picker overlay; owns the keyboard while open.
    picker: Option<PickerState>,
    conn: ConnState,
}

pub struct App {
    /// The project chooser (no-args start). While set, `session` is an inert placeholder and
    /// all messages route through `update_boot`; picking a project builds the real session
    /// over the boot connection and clears this.
    boot: Option<Boot>,
    /// The window's one editing context (one connection — the server's client).
    session: Session,
    client_version: String,
    /// The connected daemon instance's start stamp (see [`TabBootstrap::server_started_at`]).
    server_started_at: u64,
    cell: Option<Size>,
    view_size: Size,

    // Transient messages are toasts; the status bar shows persistent state only (web client
    // convention).
    toasts: Vec<Toast>,
    next_toast: u64,
}

impl App {
    pub fn new(b: Bootstrap) -> (Self, Task<Message>) {
        let shell = |boot: Option<Boot>, session: Session, client_version: String,
                     server_started_at: u64| App {
            boot,
            session,
            client_version,
            server_started_at,
            cell: None,
            view_size: Size::ZERO,
            toasts: Vec::new(),
            next_toast: 0,
        };
        match b {
            Bootstrap::Session(b) => {
                let pump = pump(b.notifications.clone());
                let session = Session::new(
                    b.handle,
                    b.notifications,
                    b.project,
                    b.project_paths,
                    b.buffer,
                );
                (
                    shell(None, session, b.client_version, b.server_started_at),
                    pump,
                )
            }
            Bootstrap::Choose(b) => {
                // Open the Projects picker on the boot connection; the session is built over
                // that same connection when the user picks a project (`SessionReady`). Until
                // then `session` is an inert placeholder — `update_boot` owns every message.
                let pump = pump(b.notifications.clone());
                let handle = b.handle.clone();
                let view = Task::perform(
                    async move {
                        handle
                            .rpc::<PickerView>(PickerViewParams {
                                kind: PickerKind::Projects,
                                reset: true,
                                offset: 0,
                                limit: FETCH_LIMIT,
                                center_on: None,
                                center_on_cursor_grep_hit: None,
                                directory_path: None,
                                explorer_roots: false,
                                buffer_id: None,
                                filters: None,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    |result| Message::PickerViewed {
                        initial: true,
                        result,
                    },
                );
                let placeholder = Session::placeholder(b.handle.clone(), b.notifications.clone());
                let boot = Boot {
                    handle: b.handle,
                    notifications: b.notifications,
                    picker: PickerState::new(PickerKind::Projects),
                    opening: false,
                    down: false,
                };
                (
                    shell(Some(boot), placeholder, b.client_version, b.server_started_at),
                    Task::batch([pump, view]),
                )
            }
        }
    }

    /// `[project] file` — mirrors the web client's page title and the TUI's terminal title.
    pub fn title(&self) -> String {
        if self.boot.is_some() {
            return "Aether".into();
        }
        format!("[{}] {}", self.session.project, self.session.buffer.label)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let keys = iced::event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => keymap::keycode(&key).map(|code| Message::Key {
                code,
                mods: modifiers.into(),
                text: text.map(|t| t.to_string()),
            }),
            _ => None,
        });
        if self.boot.is_none() && self.session.scroll_anim.is_some() {
            Subscription::batch([keys, iced::window::frames().map(Message::AnimTick)])
        } else {
            keys
        }
    }

    // ---- update ---------------------------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        // Pre-session: the project chooser owns every message; `SessionReady` is the
        // hand-off back to the normal path.
        if self.boot.is_some() {
            return self.update_boot(message);
        }
        self.update_inner(message)
    }

    /// Message handling while the project chooser is up: a reduced picker vocabulary (type to
    /// filter, Alt-j/k, Enter/click to pick, Esc quits), plus the `SessionReady` hand-off that
    /// builds the session over the boot connection.
    fn update_boot(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key { code, mods, text } => self.on_boot_key(code, mods, text),
            Message::PickerViewed { initial, result } => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                match result {
                    Ok(r) => {
                        boot.picker.offset = r.effective_offset;
                        if initial {
                            boot.picker.generation = r.generation;
                            boot.picker.total_candidates = r.total_candidates;
                        }
                        Task::none()
                    }
                    Err(e) => self.error(format!("project list failed: {e}")),
                }
            }
            Message::Notified(Some(n)) => {
                if let Some(boot) = &mut self.boot {
                    if n.method == PickerUpdate::NAME {
                        if let Ok(u) = serde_json::from_value::<PickerUpdateParams>(n.params) {
                            if boot.picker.apply_update(u) {
                                tracing::debug!(
                                    projects = boot.picker.total_matches,
                                    "project chooser updated"
                                );
                            }
                        }
                    }
                    return pump(boot.notifications.clone());
                }
                Task::none()
            }
            // The boot connection died under the chooser — dial again until a daemon is back
            // (the retry task re-reads discovery, so a restarted daemon's fresh port is found).
            Message::Notified(None) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                if boot.down {
                    return Task::none();
                }
                boot.down = true;
                let note =
                    self.toast("server disconnected — reconnecting…", ToastKind::Warning);
                Task::batch([note, self.boot_reconnect()])
            }
            Message::BootReconnected(Ok(c)) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                boot.handle = c.handle.clone();
                boot.notifications = c.notifications.clone();
                boot.picker = PickerState::new(PickerKind::Projects);
                boot.opening = false;
                boot.down = false;
                self.server_started_at = c.server_started_at;
                let handle = c.handle;
                let view = Task::perform(
                    async move {
                        handle
                            .rpc::<PickerView>(PickerViewParams {
                                kind: PickerKind::Projects,
                                reset: true,
                                offset: 0,
                                limit: FETCH_LIMIT,
                                center_on: None,
                                center_on_cursor_grep_hit: None,
                                directory_path: None,
                                explorer_roots: false,
                                buffer_id: None,
                                filters: None,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    |result| Message::PickerViewed {
                        initial: true,
                        result,
                    },
                );
                let note = self.toast("reconnected", ToastKind::Success);
                Task::batch([pump(c.notifications), view, note])
            }
            Message::BootReconnected(Err(_)) => self.boot_reconnect(),
            Message::SessionReady(Ok(r)) => {
                // The pick resolved: the boot connection becomes the session's. The running
                // pump carries on — same notification channel, now read by the main handler.
                let Some(boot) = self.boot.take() else {
                    return Task::none();
                };
                let (project, open) = *r;
                let buffer = buffer_info(open, &project.paths);
                self.session = Session::new(
                    boot.handle,
                    boot.notifications,
                    project.name,
                    project.paths,
                    buffer,
                );
                // The editor's first Layout event subscribes the viewport (cell metrics are
                // only published once it renders).
                Task::none()
            }
            Message::SessionReady(Err(e)) => {
                if let Some(boot) = &mut self.boot {
                    boot.opening = false;
                }
                self.error(format!("open failed: {e}"))
            }
            Message::PickerClicked(abs) => {
                if let Some(boot) = &mut self.boot {
                    boot.picker.selected = abs;
                }
                self.boot_accept()
            }
            Message::PickerScrolled(y) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                boot.picker.scroll_y = y;
                match boot.picker.scrolled_refetch(y) {
                    Some(offset) => self.boot_refetch(offset),
                    None => Task::none(),
                }
            }
            Message::PickerHovered(h) => {
                if let Some(boot) = &mut self.boot {
                    boot.picker.hovered = h;
                }
                Task::none()
            }
            Message::PickerUnhovered(abs) => {
                if let Some(boot) = &mut self.boot {
                    if boot.picker.hovered == Some(abs) {
                        boot.picker.hovered = None;
                    }
                }
                Task::none()
            }
            Message::ToastExpired(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            _ => Task::none(),
        }
    }

    /// Keys while the project chooser is up.
    fn on_boot_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        if boot.opening || boot.down {
            return Task::none(); // a pick / reconnect is in flight — park input until it lands
        }
        let no_chord = !mods.ctrl && !mods.alt;
        match code {
            KeyCode::Esc => return iced::exit(), // nothing behind the chooser to fall back to
            KeyCode::Enter => return self.boot_accept(),
            KeyCode::Char('k') if mods.alt && !mods.ctrl => return self.boot_move(-1),
            KeyCode::Char('j') if mods.alt && !mods.ctrl => return self.boot_move(1),
            KeyCode::PageUp => {
                return self.boot_move(-(crate::picker::VISIBLE_ROWS as i64 - 1));
            }
            KeyCode::PageDown => {
                return self.boot_move(crate::picker::VISIBLE_ROWS as i64 - 1);
            }
            KeyCode::Backspace if no_chord => {
                let p = &mut boot.picker;
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.query.remove(i);
                    p.cursor = i;
                    return self.boot_query_changed();
                }
                return Task::none();
            }
            KeyCode::Left if no_chord => {
                let p = &mut boot.picker;
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.cursor = i;
                }
                return Task::none();
            }
            KeyCode::Right if no_chord => {
                let p = &mut boot.picker;
                if let Some(c) = p.query[p.cursor..].chars().next() {
                    p.cursor += c.len_utf8();
                }
                return Task::none();
            }
            _ => {}
        }
        if no_chord {
            if let Some(t) = text {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    let p = &mut boot.picker;
                    let at = p.cursor;
                    p.query.insert_str(at, &t);
                    p.cursor = at + t.len();
                    return self.boot_query_changed();
                }
            }
        }
        Task::none()
    }

    /// Enter / click in the chooser: activate the picked project over the boot connection
    /// and open its last buffer (or a fresh transient scratch) — the bootstrap convention.
    fn boot_accept(&mut self) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        let Some(PickerItem::Project { name, .. }) = boot.picker.selected_item() else {
            return Task::none();
        };
        let name = name.clone();
        boot.opening = true;
        let handle = boot.handle.clone();
        Task::perform(
            async move {
                let activated = handle
                    .rpc::<ProjectActivate>(ProjectActivateParams { name })
                    .await
                    .map_err(|e| e.to_string())?;
                let open = handle
                    .rpc::<BufferOpen>(BufferOpenParams {
                        buffer_id: activated.last_buffer_id,
                        transient: if activated.last_buffer_id.is_none() {
                            Some(true)
                        } else {
                            None
                        },
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Box::new((activated.project, open)))
            },
            Message::SessionReady,
        )
    }

    /// An RPC on the boot connection, landing as a plain message.
    fn boot_rpc<M>(
        &self,
        params: M::Params,
        f: impl Fn(Result<M::Result, String>) -> Message + Send + 'static,
    ) -> Task<Message>
    where
        M: RpcMethod + 'static,
        M::Params: Send,
        M::Result: Send,
    {
        let Some(boot) = &self.boot else {
            return Task::none();
        };
        let handle = boot.handle.clone();
        Task::perform(
            async move { handle.rpc::<M>(params).await.map_err(|e| e.to_string()) },
            f,
        )
    }

    fn boot_query_changed(&mut self) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        let p = &mut boot.picker;
        p.generation += 1;
        p.selected = 0;
        p.offset = 0;
        p.scroll_y = 0.0;
        let (query, generation) = (p.query.clone(), p.generation);
        let q = self.boot_rpc::<PickerQuery>(
            PickerQueryParams {
                kind: PickerKind::Projects,
                query,
                generation,
                filters: Default::default(),
            },
            |_| Message::Noop,
        );
        Task::batch([q, self.boot_refetch(0)])
    }

    fn boot_refetch(&mut self, offset: u32) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        boot.picker.offset = offset;
        boot.picker.items.clear();
        self.boot_rpc::<PickerView>(
            PickerViewParams {
                kind: PickerKind::Projects,
                reset: false,
                offset,
                limit: FETCH_LIMIT,
                center_on: None,
                center_on_cursor_grep_hit: None,
                directory_path: None,
                explorer_roots: false,
                buffer_id: None,
                filters: None,
            },
            |result| Message::PickerViewed {
                initial: false,
                result,
            },
        )
    }

    fn boot_move(&mut self, delta: i64) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        match boot.picker.move_selection(delta) {
            Some(offset) => self.boot_refetch(offset),
            None => reveal_picker_selection(&mut boot.picker, Reveal::Minimal),
        }
    }

    /// One paced boot-reconnect attempt: sleep, re-read discovery, dial. Failures loop back
    /// through [`Message::BootReconnected`] — indefinitely, like the session's retry.
    fn boot_reconnect(&self) -> Task<Message> {
        let version = self.client_version.clone();
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let info = crate::discovery::read().map_err(|e| e.to_string())?;
                let server_url = format!("ws://127.0.0.1:{}", info.port);
                let (handle, rx) = crate::connection::connect(&server_url, &version)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(BootConn {
                    handle,
                    notifications: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
                    server_started_at: info.started_at_unix_ms,
                })
            },
            Message::BootReconnected,
        )
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            // Boot-only message that slipped past a finished boot — nothing to do.
            Message::SessionReady(_) => Task::none(),
            Message::Editor(ev) => self.on_editor_event(ev),
            Message::Key { code, mods, text } => self.on_key(code, mods, text),

            Message::Subscribed(Ok(res)) => {
                tracing::info!(
                    viewport_id = res.viewport_id,
                    lines = res.window.lines.len(),
                    total_visual_rows = res.window.total_visual_rows,
                    "viewport subscribed"
                );
                self.session.viewport_id = Some(res.viewport_id);
                self.session.diagnostics = res.buffer_status.diagnostics;
                self.session.lsp = res.buffer_status.lsp_status;
                self.session.externally_modified = res.buffer_status.externally_modified;
                self.session.externally_deleted = res.buffer_status.externally_deleted;
                // Position the view at the scroll the subscribe asked for (restored or
                // cursor-centred), now the window geometry is known, then make sure the cursor
                // is on-screen (it may sit below a restored scroll after a `jump_to` open).
                if let Some(cell) = self.cell {
                    let scroll = self.session.subscribe_scroll;
                    if let Some(rel) = grid::rows_before_line(&res.window, scroll.logical_line) {
                        let row = res.window.first_visual_row + rel;
                        self.session.scroll_px = (row as f32 + scroll.sub_row) * cell.height;
                    }
                }
                self.apply_window(res.window);
                self.reveal_cursor();
                // The diff-view toggle is sticky across buffer switches, but a fresh viewport
                // starts with it off — re-enable server-side.
                if self.session.diff_view {
                    let enabled = true;
                    return self.rpc::<GitSetDiffView>(
                        GitSetDiffViewParams {
                            viewport_id: res.viewport_id,
                            enabled,
                        },
                        move |result| Message::DiffViewSet { enabled, result },
                    );
                }
                Task::none()
            }
            Message::Subscribed(Err(e)) => self.error(format!("subscribe failed: {e}")),

            Message::WindowUpdate(Ok(res)) => {
                self.session.fetch_in_flight = false;
                self.apply_window(res.window);
                let mut task = Task::none();
                if self.session.reveal_after_fetch {
                    self.session.reveal_after_fetch = false;
                    self.reveal_cursor();
                }
                if self.session.refetch_queued {
                    self.session.refetch_queued = false;
                    task = self.maybe_fetch();
                }
                task
            }
            Message::WindowUpdate(Err(e)) => {
                self.session.fetch_in_flight = false;
                self.session.refetch_queued = false;
                self.error(format!("viewport update failed: {e}"))
            }

            Message::CursorMsg(Ok(cursor)) => {
                self.session.buffer.cursor = cursor;
                self.ensure_cursor_visible()
            }
            Message::CursorMsg(Err(e)) => self.error(e),

            Message::EditDone(Ok(r)) => {
                self.session.buffer.revision = r.revision;
                self.session.buffer.cursor = r.cursor;
                self.ensure_cursor_visible()
            }
            Message::EditDone(Err(e)) => self.error(e),

            Message::UndoRedoDone(Ok(r)) => {
                self.session.buffer.revision = r.revision;
                self.session.buffer.cursor = r.cursor;
                let note = if r.applied {
                    Task::none()
                } else {
                    self.toast("nothing to undo/redo", ToastKind::Info)
                };
                Task::batch([note, self.ensure_cursor_visible()])
            }
            Message::UndoRedoDone(Err(e)) => self.error(e),

            Message::SaveTried(Ok(SaveTry::Saved { result, target })) => {
                self.session.buffer.revision = result.revision;
                self.session.buffer.saved_revision = result.revision;
                self.session.buffer.transient = false; // saving promotes a transient buffer
                self.session.externally_modified = false;
                self.session.externally_deleted = false;
                let note = match target {
                    Some((path_index, rel)) => {
                        // Save-as: the buffer's identity changed — adopt the new path/label.
                        let root = self.session.project_paths.get(path_index as usize);
                        self.session.buffer.path =
                            root.map(|r| format!("{}/{rel}", r.trim_end_matches('/')));
                        self.session.buffer.label = rel.clone();
                        format!("saved as {rel} (rev {})", result.revision)
                    }
                    None => format!("saved (rev {})", result.revision),
                };
                self.toast(note, ToastKind::Success)
            }
            Message::SaveTried(Ok(SaveTry::NeedsConfirm { message, action })) => {
                self.session.prompt = Some(Prompt::Confirm { message, action });
                Task::none()
            }
            Message::SaveTried(Err(e)) => self.error(format!("save failed: {e}")),

            Message::ReloadTried(Ok(ReloadTry::Reloaded(r))) => {
                self.session.buffer.revision = r.revision;
                self.session.buffer.saved_revision = r.revision;
                self.session.buffer.transient = false; // reloading promotes, like save
                self.session.externally_modified = false;
                self.session.externally_deleted = false;
                self.toast(format!("reloaded (rev {})", r.revision), ToastKind::Success)
            }
            Message::ReloadTried(Ok(ReloadTry::NeedsConfirm)) => {
                self.session.prompt = Some(Prompt::Confirm {
                    message: "discard local changes and reload".into(),
                    action: ConfirmAction::ReloadDiscard,
                });
                Task::none()
            }
            Message::ReloadTried(Err(e)) => self.error(format!("reload failed: {e}")),

            Message::PromptAccept => self.accept_prompt(),
            Message::PromptCancel => {
                self.decline_prompt();
                Task::none()
            }

            Message::PickerViewed { initial, result } => match result {
                Ok(r) => {
                    if let Some(p) = &mut self.session.picker {
                        p.offset = r.effective_offset;
                        if let Some(center) = r.effective_center_on {
                            p.pending_center = Some(center);
                            // Grep centering (cursor-hit opens, file jumps) aligns the target
                            // to the top — there's context below to read. Everything else
                            // scrolls the minimum.
                            p.reveal_on_update = Some(if p.kind == PickerKind::Grep {
                                Reveal::Top
                            } else {
                                Reveal::Minimal
                            });
                        }
                        p.directory = r.directory_path;
                        p.directory_parent = r.directory_parent;
                        if initial {
                            // Adopt the resumed query (grep preserves it across opens).
                            p.generation = r.generation;
                            p.cursor = r.query.len();
                            p.query = r.query;
                            p.total_candidates = r.total_candidates;
                            // Adopt the persisted filters next to the persisted query (grep
                            // resume restores its chips; reset kinds come back all-default;
                            // seeded opens get their seed echoed back).
                            p.adopt_filters(&r.filters);
                        }
                    }
                    Task::none()
                }
                Err(e) => {
                    self.session.picker = None;
                    self.error(format!("picker failed: {e}"))
                }
            },

            // Selections open in place: the window shows one buffer, and the one being
            // replaced is a `Space b` away (buffers persist server-side). Opens are
            // transient previews — switching away from one closes it.
            Message::PickerSelected {
                prime,
                result: Ok(result),
            } => match result {
                PickerSelectResult::File { path } => self.open_path_primed(path, None, prime),
                PickerSelectResult::FileAt { path, position } => {
                    self.open_path_primed(path, Some(position), prime)
                }
                PickerSelectResult::Buffer { buffer_id } => {
                    if buffer_id == self.session.buffer.buffer_id {
                        return Task::none(); // already showing it
                    }
                    let handle = self.session.handle.clone();
                    let from = self.session.buffer.buffer_id;
                    self.task(
                        async move {
                            let _ = handle
                                .rpc::<NavRecord>(NavRecordParams { buffer_id: from })
                                .await;
                            handle
                                .rpc::<BufferOpen>(BufferOpenParams {
                                    buffer_id: Some(buffer_id),
                                    ..Default::default()
                                })
                                .await
                                .map_err(|e| e.to_string())
                        },
                        Message::Switched,
                    )
                }
                PickerSelectResult::Project { name } => {
                    // Activate, then land on the project's last buffer (or a fresh
                    // transient scratch) — the bootstrap convention.
                    let handle = self.session.handle.clone();
                    self.task(
                        async move {
                            let activated = handle
                                .rpc::<ProjectActivate>(ProjectActivateParams { name })
                                .await
                                .map_err(|e| e.to_string())?;
                            let open = handle
                                .rpc::<BufferOpen>(BufferOpenParams {
                                    buffer_id: activated.last_buffer_id,
                                    transient: if activated.last_buffer_id.is_none() {
                                        Some(true)
                                    } else {
                                        None
                                    },
                                    ..Default::default()
                                })
                                .await
                                .map_err(|e| e.to_string())?;
                            Ok((activated.project, open))
                        },
                        Message::ProjectActivated,
                    )
                }
            },
            Message::PickerSelected {
                result: Err(e), ..
            } => self.error(format!("select failed: {e}")),

            Message::ProjectActivated(Ok((project, open))) => {
                self.session.project = project.name;
                self.session.project_paths = project.paths;
                self.switch_to(open)
            }
            Message::ProjectActivated(Err(e)) => self.error(format!("project switch failed: {e}")),

            Message::PickerClicked(abs) => {
                if let Some(p) = &mut self.session.picker {
                    p.selected = abs;
                }
                self.picker_accept()
            }

            Message::PickerChipClicked(i) => {
                if let Some(p) = &mut self.session.picker {
                    p.chip_selected = Some(i);
                }
                Task::none()
            }

            Message::PickerChipListing { abs, result } => {
                // Stale responses (the editor moved to another directory, or closed) are
                // dropped by the abs-path staleness key.
                if let Some(ed) = self.session
                    .picker
                    .as_mut()
                    .and_then(|p| p.chip_editor.as_mut())
                {
                    if ed.listing_dir_abs == abs {
                        match result {
                            Ok(r) => ed.set_dir_listing(r.entries),
                            // Typed-but-nonexistent segment, or outside the boundary — the
                            // path renders invalid until the next change re-syncs.
                            Err(_) => ed.set_dir_listing_failed(),
                        }
                    }
                }
                Task::none()
            }

            Message::GrepFileJumped(Ok(None)) => Task::none(), // already at the first/last file
            Message::GrepFileJumped(Ok(Some(target))) => {
                let Some(p) = &mut self.session.picker else {
                    return Task::none();
                };
                // In the loaded window → purely local move, no refetch; the target aligns to
                // the top so the file reads from its first hit.
                let key = item_key(&target);
                if let Some(idx) = p.items.iter().position(|i| item_key(i) == key) {
                    p.selected = p.offset + idx as u32;
                    return self.picker_reveal_selected_with(Reveal::Top);
                }
                // Past the window → re-frame around the target; the arriving push lands the
                // highlight via the `effective_center_on` echo (Reveal::Top for grep).
                let kind = p.kind;
                self.rpc::<PickerView>(
                    PickerViewParams {
                        kind,
                        reset: false,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: Some(target),
                        center_on_cursor_grep_hit: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        filters: None,
                    },
                    |result| Message::PickerViewed {
                        initial: false,
                        result,
                    },
                )
            }
            Message::GrepFileJumped(Err(e)) => self.error(format!("file jump failed: {e}")),

            Message::PickerHovered(h) => {
                if let Some(p) = &mut self.session.picker {
                    p.hovered = h;
                }
                Task::none()
            }
            Message::PickerUnhovered(abs) => {
                if let Some(p) = &mut self.session.picker {
                    if p.hovered == Some(abs) {
                        p.hovered = None;
                    }
                }
                Task::none()
            }

            Message::PickerScrolled(y) => {
                let Some(p) = &mut self.session.picker else {
                    return Task::none();
                };
                p.scroll_y = y;
                match p.scrolled_refetch(y) {
                    Some(offset) => self.picker_refetch(offset),
                    None => Task::none(),
                }
            }

            Message::CopyDone(Ok(r)) => {
                let note = self.toast(format!("copied {} bytes", r.text.len()), ToastKind::Success);
                Task::batch([note, iced::clipboard::write(r.text)])
            }
            Message::CopyDone(Err(e)) => self.error(format!("copy failed: {e}")),

            Message::CutDone(Ok(r)) => {
                self.session.buffer.revision = r.revision;
                self.session.buffer.cursor = r.cursor;
                let note = self.toast(format!("cut {} bytes", r.text.len()), ToastKind::Success);
                Task::batch([
                    note,
                    iced::clipboard::write(r.text),
                    self.ensure_cursor_visible(),
                ])
            }
            Message::CutDone(Err(e)) => self.error(format!("cut failed: {e}")),

            Message::ToastExpired(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            Message::Noop => Task::none(),

            Message::AnimTick(now) => {
                let Some(anim) = &self.session.scroll_anim else {
                    return Task::none();
                };
                let t = ((now - anim.started).as_secs_f32() * 1000.0 / SCROLL_ANIM_MS).min(1.0);
                let eased = 1.0 - (1.0 - t).powi(3); // cubic ease-out
                self.session.scroll_px = anim.from + (anim.to - anim.from) * eased;
                if t >= 1.0 {
                    self.session.scroll_anim = None;
                }
                self.clamp_scroll();
                self.maybe_fetch()
            }

            Message::SearchApplied(Ok(r)) => {
                self.session.buffer.cursor = r.cursor;
                let zero = r.summary.total == 0;
                self.session.search.summary = Some(r.summary);
                if zero {
                    // A failed keystroke shouldn't strand the user wherever the previous query
                    // had jumped them.
                    self.revert_to_snapshot_cursor()
                } else {
                    self.ensure_cursor_visible()
                }
            }
            Message::SearchApplied(Err(_)) => {
                // Most commonly an invalid regex mid-type (e.g. a trailing `\`): treat as a
                // transient zero-match state.
                self.session.search.summary = Some(SearchSummary {
                    buffer_id: self.session.buffer.buffer_id,
                    total: 0,
                    truncated: false,
                    current_index: 0,
                });
                let note = self.toast("invalid regex", ToastKind::Warning);
                Task::batch([note, self.revert_to_snapshot_cursor()])
            }

            Message::SearchRestored(Ok(r)) => {
                self.session.search.summary = Some(r.summary);
                Task::none()
            }
            Message::SearchRestored(Err(e)) => self.error(e),

            Message::SearchNav(Ok(r)) => {
                self.session.buffer.cursor = r.cursor;
                self.session.search.summary = Some(r.summary);
                self.ensure_cursor_visible()
            }
            Message::SearchNav(Err(e)) => self.error(e),

            Message::SearchFromSel(Ok(Some((query, r)))) => {
                self.session.search.cursor = query.len();
                self.session.search.query = query.clone();
                self.session.search.active = true;
                self.session.search.summary = Some(r.summary);
                self.push_history(query);
                Task::none()
            }
            Message::SearchFromSel(Ok(None)) => Task::none(), // empty selection
            Message::SearchFromSel(Err(e)) => self.error(e),

            Message::Switched(Ok(open)) => self.switch_to(open),
            Message::Switched(Err(e)) => self.error(e),

            Message::SwitchedPrimed(Ok(Some((query, open)))) => {
                let task = self.switch_to(open);
                // switch_to reset the search state; adopt the primed query (the server-side
                // search was already set in the open chain).
                self.session.search.cursor = query.len();
                self.session.search.query = query.clone();
                self.session.search.active = true;
                self.push_history(query);
                task
            }
            Message::SwitchedPrimed(Ok(None)) => self.toast("no more grep hits", ToastKind::Info),
            Message::SwitchedPrimed(Err(e)) => self.error(e),

            Message::NavDone { forward, result } => match result {
                Ok(NavStepResult { target: Some(open) }) => self.switch_to(open),
                Ok(_) => self.toast(
                    if forward {
                        "no later location in history"
                    } else {
                        "no earlier location in history"
                    },
                    ToastKind::Info,
                ),
                Err(e) => self.error(e),
            },

            Message::Definition(Ok(r)) => match r.location {
                Some(location) => self.open_location(location),
                None => self.toast("No definition found", ToastKind::Info),
            },
            Message::Definition(Err(e)) => self.error(e),

            Message::DiagNav(Ok(r)) => {
                self.session.buffer.cursor = r.cursor;
                let note = if r.moved {
                    Task::none()
                } else {
                    self.toast("no more diagnostics", ToastKind::Info)
                };
                Task::batch([note, self.ensure_cursor_visible()])
            }
            Message::DiagNav(Err(e)) => self.error(e),

            Message::HoverInfo(Ok(r)) => match r.contents {
                Some(text) => {
                    let est_lines = text.lines().count().max(1);
                    self.session.hover = Some(HoverContent::Markdown {
                        items: iced::widget::markdown::parse(&text).collect(),
                        est_lines,
                    });
                    Task::none()
                }
                None => {
                    self.session.hover = None;
                    self.toast("No hover info", ToastKind::Info)
                }
            },
            Message::HoverInfo(Err(e)) => self.error(format!("hover failed: {e}")),

            Message::FormatDone(Ok(r)) => {
                self.session.buffer.cursor = r.cursor;
                // Specific feedback per outcome — "nothing happened" has several causes.
                let note = match r.status {
                    FormatStatus::Applied => None,
                    FormatStatus::NoChange => Some("already formatted".to_string()),
                    FormatStatus::NotReady => Some("language server still starting".to_string()),
                    FormatStatus::Unavailable => Some("language server unavailable".to_string()),
                    FormatStatus::Unsupported => Some(match self.session.buffer.language.as_deref() {
                        Some(lang) => format!("no formatter for {lang}"),
                        None => "no formatter for this file".to_string(),
                    }),
                };
                let note = match note {
                    Some(n) => self.toast(n, ToastKind::Info),
                    None => Task::none(),
                };
                Task::batch([note, self.ensure_cursor_visible()])
            }
            Message::FormatDone(Err(e)) => self.error(format!("format failed: {e}")),

            Message::CommitLookup(Ok(CommitDetails::Info(info))) => {
                // Mirror `git show`'s header: commit / Author / Date, blank line, message.
                let text = format!(
                    "commit {}\nAuthor: {} <{}>\nDate:   {}\n\n{}",
                    info.commit, info.author, info.email, info.date, info.message
                );
                self.session.hover = Some(HoverContent::Blocks(vec![HoverBlock {
                    severity: None,
                    text,
                }]));
                Task::none()
            }
            Message::CommitLookup(Ok(CommitDetails::Note(note))) => {
                self.toast(note, ToastKind::Info)
            }
            Message::CommitLookup(Err(e)) => self.error(format!("commit info failed: {e}")),

            Message::BlameLine {
                buffer_id,
                line,
                result,
            } => {
                if buffer_id == self.session.buffer.buffer_id && line == self.session.buffer.cursor.position.line
                {
                    self.session.blame = match result.ok().and_then(|r| r.blame) {
                        Some(b) if b.is_uncommitted => Some((line, "uncommitted".into())),
                        Some(b) => Some((line, format!("{} · {}", b.author, time_ago(b.timestamp)))),
                        None => None,
                    };
                }
                Task::none()
            }

            Message::HunkNav(Ok(r)) => {
                self.session.buffer.cursor = r.cursor;
                let note = if r.moved {
                    Task::none()
                } else {
                    self.toast("no more changes", ToastKind::Info)
                };
                Task::batch([note, self.ensure_cursor_visible()])
            }
            Message::HunkNav(Err(e)) => self.error(e),

            Message::HunkApplied { action, result } => match result {
                Ok(r) => {
                    self.session.buffer.cursor = r.cursor;
                    let (msg, kind) = match r.status {
                        ApplyHunkStatus::Staged => ("staged change", ToastKind::Success),
                        ApplyHunkStatus::Unstaged => ("unstaged change", ToastKind::Success),
                        ApplyHunkStatus::Reverted => ("reverted change", ToastKind::Success),
                        ApplyHunkStatus::NoChange => (
                            match action {
                                HunkAction::Toggle => "no change here",
                                HunkAction::Revert => "no change to revert here",
                            },
                            ToastKind::Info,
                        ),
                        ApplyHunkStatus::DirtyBuffer => {
                            ("unsaved changes — save first", ToastKind::Warning)
                        }
                        ApplyHunkStatus::Unavailable => {
                            ("not in a git repository", ToastKind::Info)
                        }
                    };
                    self.toast(msg, kind)
                }
                Err(e) => self.error(e),
            },

            Message::DiffViewSet { enabled, result } => match result {
                Ok(r) => {
                    self.session.diff_view = enabled;
                    self.apply_window(r.window);
                    self.reveal_cursor();
                    self.toast(
                        format!("diff: {}", if enabled { "on" } else { "off" }),
                        ToastKind::Info,
                    )
                }
                Err(e) => self.error(e),
            },

            Message::ClipboardRead(kind, text) => {
                let Some(text) = text.filter(|t| !t.is_empty()) else {
                    return self.error("clipboard is empty".into());
                };
                self.paste(kind, text)
            }

            Message::Notified(Some(n)) => {
                let task = self.on_notification(n);
                Task::batch([task, pump(self.session.notifications.clone())])
            }
            Message::Notified(None) => {
                let s = &mut self.session;
                if s.conn != ConnState::Connected {
                    return Task::none(); // already reconnecting (a late echo from the old pump)
                }
                s.conn = ConnState::Reconnecting {
                    attempt: 0,
                    had_unsaved: s.buffer.revision != s.buffer.saved_revision,
                };
                tracing::warn!(buffer = %s.buffer.label, "connection lost; reconnecting");
                let note =
                    self.toast("server disconnected — reconnecting…", ToastKind::Warning);
                Task::batch([note, self.try_reconnect(0)])
            }

            Message::Reconnected(Ok(r)) => self.adopt_reconnect(*r),
            Message::Reconnected(Err(ReconnectError::NotUp)) => {
                let s = &mut self.session;
                if let ConnState::Reconnecting { attempt, .. } = &mut s.conn {
                    *attempt += 1;
                    let next = *attempt;
                    return self.try_reconnect(next);
                }
                Task::none()
            }
            Message::Reconnected(Err(ReconnectError::Fatal(e))) => {
                self.session.conn = ConnState::Failed;
                self.error(format!("reconnect failed: {e}"))
            }
            // Boot-only message that slipped past a finished boot — nothing to do.
            Message::BootReconnected(_) => Task::none(),
        }
    }

    fn toast(&mut self, message: impl Into<String>, kind: ToastKind) -> Task<Message> {
        let message = message.into();
        // Don't stack identical toasts (incremental search can re-report "invalid regex" on
        // every keystroke).
        if self.toasts.last().is_some_and(|t| t.message == message) {
            return Task::none();
        }
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, message, kind });
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(3600)).await;
                id
            },
            Message::ToastExpired,
        )
    }

    fn error(&mut self, message: String) -> Task<Message> {
        self.toast(message, ToastKind::Error)
    }

    // ---- editor (widget) events ------------------------------------------------------------

    fn on_editor_event(&mut self, ev: EditorEvent) -> Task<Message> {
        // While the connection is down, mouse/wheel input is suspended like the keyboard; the
        // Layout event still records metrics (the reconnect's resubscribe reads them) but
        // fires no RPC.
        if self.session.conn != ConnState::Connected {
            if let EditorEvent::Layout { cell, size } = ev {
                self.cell = Some(cell);
                self.view_size = size;
            }
            return Task::none();
        }
        match ev {
            EditorEvent::Layout { cell, size } => {
                self.cell = Some(cell);
                self.view_size = size;
                let cols = ((size.width / cell.width) as u32).saturating_sub(GUTTER_COLS);
                let rows = ((size.height - PAD) / cell.height).max(1.0) as u32;
                if cols == 0 || rows == 0 {
                    return Task::none();
                }
                if self.session.viewport_id.is_none() {
                    if self.session.sent_grid.is_some() {
                        return Task::none(); // subscribe in flight
                    }
                    self.session.sent_grid = Some((cols, rows));
                    self.subscribe_task()
                } else if self.session.sent_grid != Some((cols, rows)) {
                    self.session.sent_grid = Some((cols, rows));
                    let viewport_id = self.session.viewport_id.unwrap();
                    self.rpc::<ViewportResize>(
                        ViewportResizeParams {
                            viewport_id,
                            cols,
                            rows,
                        },
                        Message::WindowUpdate,
                    )
                } else {
                    Task::none()
                }
            }
            EditorEvent::Wheel { delta_px, delta_x_px } => {
                self.session.hover = None;
                // With a picker open, its scrollable owns wheel input over the list; wheel
                // over the backdrop shouldn't scroll the editor behind it either.
                if self.session.picker.is_some() {
                    return Task::none();
                }
                self.scroll_by(delta_px);
                self.scroll_x_by(delta_x_px);
                self.maybe_fetch()
            }
            EditorEvent::Pressed {
                row,
                dcol,
                kind,
                shift,
            } => {
                self.session.hover = None;
                // A click outside the dialog/picker cancels it (the web's backdrop-click
                // behaviour); the click doesn't also move the cursor.
                if self.session.prompt.is_some() {
                    self.decline_prompt();
                    return Task::none();
                }
                if self.session.picker.is_some() {
                    return self.close_picker();
                }
                let Some(window) = &self.session.window else {
                    return Task::none();
                };
                let Some(pos) = grid::hit_test(window, row, dcol, TAB_WIDTH) else {
                    return Task::none();
                };
                let granularity = match kind {
                    ClickKind::Single => Granularity::Char,
                    ClickKind::Double => Granularity::Word,
                    ClickKind::Triple => Granularity::Line,
                };
                let anchor = if shift { self.session.buffer.cursor.anchor } else { pos };
                self.session.drag = Some((anchor, granularity));
                self.rpc::<CursorSet>(
                    CursorSetParams {
                        buffer_id: self.session.buffer.buffer_id,
                        position: pos,
                        anchor,
                        granularity,
                    },
                    Message::CursorMsg,
                )
            }
            EditorEvent::Dragged { row, dcol } => {
                let Some((anchor, granularity)) = self.session.drag else {
                    return Task::none();
                };
                let Some(window) = &self.session.window else {
                    return Task::none();
                };
                let Some(pos) = grid::hit_test(window, row, dcol, TAB_WIDTH) else {
                    return Task::none();
                };
                self.rpc::<CursorSet>(
                    CursorSetParams {
                        buffer_id: self.session.buffer.buffer_id,
                        position: pos,
                        anchor,
                        granularity,
                    },
                    Message::CursorMsg,
                )
            }
            EditorEvent::Released => {
                self.session.drag = None;
                Task::none()
            }
        }
    }

    // ---- keyboard --------------------------------------------------------------------------

    fn on_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        if self.session.conn != ConnState::Connected {
            return Task::none(); // editing input is suspended while the connection is down
        }

        // An open modal prompt owns the keyboard outright; a picker likewise.
        if self.session.prompt.is_some() {
            return self.on_prompt_key(code, mods, text);
        }
        if self.session.picker.is_some() {
            return self.on_picker_key(code, mods, text);
        }

        // Any keystroke dismisses an open hover popover; Esc is consumed by the dismissal
        // (matching the web client), everything else still acts.
        if self.session.hover.is_some() {
            self.session.hover = None;
            if code == KeyCode::Esc {
                return Task::none();
            }
        }

        // Search mode owns the keyboard: control keys via its table, anything printable is
        // query text (case-preserved — no normalisation of the literal query).
        if self.session.mode == Mode::Search {
            return self.on_search_key(code, mods, text);
        }

        // Stateful captures run before table lookup, like the TUI.
        match self.session.pending {
            Pending::Find {
                dir,
                till,
                extend,
                count,
            } => {
                self.session.pending = Pending::None;
                if code == KeyCode::Esc {
                    return Task::none();
                }
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(ch) = ch.filter(|c| !c.is_control()) else {
                    return Task::none();
                };
                let motion = Motion::FindChar {
                    ch,
                    direction: dir,
                    count,
                    till,
                };
                // `BeginFind` only armed the capture; the repeatable thing is this resolved
                // find (with its target char), so record it here.
                self.session.last_repeat = Some(RepeatTarget::Find(motion.clone()));
                return self.move_motion(motion, extend);
            }
            Pending::Surround(target) => {
                self.session.pending = Pending::None;
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(delimiter) = ch.filter(|c| !c.is_control()) else {
                    return Task::none(); // Esc / non-char cancels
                };
                return self.edit::<InputSurround>(InputSurroundParams {
                    buffer_id: self.session.buffer.buffer_id,
                    delimiter,
                    target,
                });
            }
            Pending::Leader => {
                self.session.pending = Pending::None;
                if let Some(b) = keymap::lookup(KeyContext::Leader, code, mods) {
                    return self.run_action(b.action, 1, mods.shift);
                }
                return Task::none();
            }
            Pending::None => {}
        }

        // Count lexer (Normal mode): digits accumulate; `0` only continues a count (it's
        // line-start otherwise).
        if self.session.mode == Mode::Normal && !mods.ctrl && !mods.alt {
            if let KeyCode::Char(c) = code {
                if c.is_ascii_digit() && (c != '0' || self.session.count.is_some()) {
                    let d = c.to_digit(10).unwrap();
                    self.session.count = Some(self.session.count.unwrap_or(0).saturating_mul(10) + d);
                    return Task::none();
                }
            }
        }
        let count = self.session.count.take().unwrap_or(1).max(1);
        let extend = mods.shift;

        // Global table first (mode-identical Ctrl shortcuts), then the mode's own.
        let ctx = match self.session.mode {
            Mode::Normal => KeyContext::Normal,
            Mode::Insert => KeyContext::Insert,
            Mode::Search => return Task::none(), // handled above
        };
        if let Some(b) =
            keymap::lookup(KeyContext::Global, code, mods).or_else(|| keymap::lookup(ctx, code, mods))
        {
            return self.run_action(b.action, count, extend);
        }

        // Insert mode: unmatched printable input is text.
        if self.session.mode == Mode::Insert && !mods.ctrl && !mods.alt {
            if let Some(t) = text {
                let t: String = t.chars().filter(|c| !c.is_control() || *c == '\t').collect();
                if !t.is_empty() {
                    return self.edit::<InputText>(InputTextParams {
                        buffer_id: self.session.buffer.buffer_id,
                        text: t,
                        select_pasted: false,
                    });
                }
            }
        }
        Task::none()
    }

    // ---- actions ----------------------------------------------------------------------------

    fn run_action(&mut self, action: Action, count: u32, extend: bool) -> Task<Message> {
        let task = self.dispatch_action(action, count, extend);
        // Remember the action for `r`/`Shift-r` to replay. Recorded at dispatch (the TUI records
        // after a successful await; here the RPC is still in flight — a failed motion leaves a
        // harmless no-op target). `RepeatMotion` itself isn't repeatable, so it never overwrites
        // the target with itself; find records its resolved motion at the capture site instead.
        if action.is_repeatable() {
            self.session.last_repeat = Some(RepeatTarget::Action { action, count });
        }
        task
    }

    fn dispatch_action(&mut self, action: Action, count: u32, extend: bool) -> Task<Message> {
        use Action as A;
        let buffer_id = self.session.buffer.buffer_id;
        match action {
            // ---- motions ----
            A::MoveChar(direction) => {
                self.move_motion(Motion::Char { direction, count }, extend)
            }
            A::MoveWord { dir, boundary } => self.move_motion(
                Motion::Word {
                    direction: dir,
                    count,
                    boundary,
                    exclusive: dir == Direction::Forward && extend,
                },
                extend,
            ),
            A::MoveWordEnd { dir, boundary } => self.move_motion(
                Motion::WordEnd {
                    direction: dir,
                    count,
                    boundary,
                },
                extend,
            ),
            A::MoveVisualLine(direction) => {
                let Some(viewport_id) = self.session.viewport_id else {
                    return Task::none();
                };
                self.move_motion(
                    Motion::VisualLine {
                        viewport_id,
                        direction,
                        count,
                    },
                    extend,
                )
            }
            A::MoveLogicalLine(direction) => self.move_motion(
                Motion::LogicalLine {
                    direction,
                    count,
                    preserve_col: true,
                },
                extend,
            ),
            A::MoveLineStart => self.move_motion(Motion::LineStart, extend),
            A::MoveLineEnd => self.move_motion(Motion::LineEnd, extend),
            A::MoveLineFirstNonblank => self.move_motion(Motion::LineFirstNonblank, extend),
            A::MoveLogicalLineFirstNonblank(direction) => self.move_motion(
                Motion::LogicalLineFirstNonblank { direction, count },
                extend,
            ),
            A::GotoLine { last } => {
                let line = if last {
                    self.session.window
                        .as_ref()
                        .map(|w| w.line_count.saturating_sub(1))
                        .unwrap_or(0)
                } else {
                    count.saturating_sub(1)
                };
                self.move_motion(
                    Motion::Goto {
                        position: LogicalPosition { line, col: 0 },
                    },
                    extend,
                )
            }
            A::MatchBracket { inner } => self.move_motion(Motion::MatchBracket { inner }, extend),
            A::PageMotion { dir, half } => {
                let Some(viewport_id) = self.session.viewport_id else {
                    return Task::none();
                };
                let rows = self.visible_rows();
                let span = if half { (rows / 2).max(1) } else { rows.max(1) };
                self.move_motion(
                    Motion::VisualLine {
                        viewport_id,
                        direction: dir,
                        count: count.saturating_mul(span),
                    },
                    extend,
                )
            }
            A::NavUnit(Direction::Forward) => self.move_motion(Motion::NextNavigationUnit, false),
            A::NavUnit(Direction::Backward) => self.move_motion(Motion::PrevNavigationUnit, false),
            A::NavUnitEdge { start: false } => self.move_motion(Motion::EndOfNavigationUnit, true),
            A::NavUnitEdge { start: true } => self.move_motion(Motion::StartOfNavigationUnit, true),
            A::BeginFind { dir, till } => {
                self.session.pending = Pending::Find {
                    dir,
                    till,
                    extend,
                    count,
                };
                Task::none()
            }

            // ---- selection ----
            A::SelectLine(direction) => {
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let mut last = Err("select_line: no iterations".to_string());
                        for _ in 0..count.max(1) {
                            last = handle
                                .rpc::<CursorSelectLine>(CursorSelectLineParams {
                                    buffer_id,
                                    direction,
                                    extend,
                                })
                                .await
                                .map_err(|e| e.to_string());
                            if last.is_err() {
                                break;
                            }
                        }
                        last
                    },
                    Message::CursorMsg,
                )
            }
            A::SwapAnchor => self.rpc::<CursorSwapAnchor>(
                CursorSwapAnchorParams { buffer_id },
                Message::CursorMsg,
            ),
            A::CollapseSelection => {
                if self.session.buffer.cursor.is_point() {
                    return Task::none();
                }
                let pos = self.session.buffer.cursor.position;
                self.rpc::<CursorSet>(
                    CursorSetParams {
                        buffer_id,
                        position: pos,
                        anchor: pos,
                        granularity: Granularity::Char,
                    },
                    Message::CursorMsg,
                )
            }
            A::TreeExpand => self.repeat_cursor::<CursorExpand>(count),
            A::TreeContract => self.repeat_cursor::<CursorContract>(count),
            A::MotionUndo => self.motion_history::<CursorUndo>(count),
            A::MotionRedo => self.motion_history::<CursorRedo>(count),
            A::RepeatMotion => {
                // `r`'s own count is how many times to replay; the stored target keeps the
                // original count baked in. Chained so the replays run sequentially.
                let Some(target) = self.session.last_repeat.clone() else {
                    return Task::none();
                };
                let mut task = Task::none();
                for _ in 0..count.max(1) {
                    let step = match &target {
                        RepeatTarget::Action { action, count } => {
                            self.dispatch_action(*action, *count, extend)
                        }
                        RepeatTarget::Find(motion) => self.move_motion(motion.clone(), extend),
                    };
                    task = task.chain(step);
                }
                task
            }
            A::CenterCursor => {
                self.center_cursor();
                self.maybe_fetch()
            }
            A::NavBack | A::NavForward => {
                let forward = matches!(action, A::NavForward);
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let res = if forward {
                            handle.rpc::<NavForward>(NavStepParams { buffer_id }).await
                        } else {
                            handle.rpc::<NavBack>(NavStepParams { buffer_id }).await
                        };
                        res.map_err(|e| e.to_string())
                    },
                    move |result| Message::NavDone { forward, result },
                )
            }

            // ---- viewport ----
            A::Scroll { dir, unit } => {
                let Some(cell) = self.cell else {
                    return Task::none();
                };
                let rows = self.visible_rows() as f32;
                let mag = match unit {
                    ScrollUnit::Line => 1.0,
                    ScrollUnit::Half => (rows / 2.0).max(1.0),
                    ScrollUnit::Page => rows.max(1.0),
                };
                match dir {
                    ScrollDir::Up => {
                        self.scroll_to_px(self.scroll_target() - mag * cell.height, true)
                    }
                    ScrollDir::Down => {
                        self.scroll_to_px(self.scroll_target() + mag * cell.height, true)
                    }
                    ScrollDir::Left => self.scroll_x_by(-cell.width),
                    ScrollDir::Right => self.scroll_x_by(cell.width),
                }
                self.maybe_fetch()
            }
            A::ToggleWrap => {
                let Some(viewport_id) = self.session.viewport_id else {
                    return Task::none();
                };
                self.session.wrap = match self.session.wrap {
                    WrapMode::Soft => WrapMode::None,
                    WrapMode::None => WrapMode::Soft,
                };
                self.session.scroll_x_px = 0.0;
                let wrap = self.session.wrap;
                self.rpc::<ViewportSetWrap>(
                    ViewportSetWrapParams { viewport_id, wrap },
                    Message::WindowUpdate,
                )
            }

            // ---- mode transitions ----
            A::EnterInsert(where_) => {
                self.session.mode = Mode::Insert;
                self.enter_insert_at(where_)
            }
            A::LeaveInsert => {
                self.session.mode = Mode::Normal;
                Task::none()
            }
            A::BeginLeader => {
                self.session.pending = Pending::Leader;
                Task::none()
            }

            // ---- edits ----
            A::Backspace => self.edit::<InputBackspace>(BufferOnlyParams { buffer_id }),
            A::NewlineIndent => self.edit::<InputNewlineAndIndent>(BufferOnlyParams { buffer_id }),
            A::InsertTab => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: "\t".into(),
                select_pasted: false,
            }),
            A::DeletePoint => self.edit::<InputDelete>(BufferOnlyParams { buffer_id }),
            A::DeleteSelection => self.repeat_edit::<InputDelete>(count),
            A::DeleteLine => self.edit::<InputDeleteLine>(BufferOnlyParams { buffer_id }),
            A::Undo => self.undo_redo::<InputUndo>(count),
            A::Redo => self.undo_redo::<InputRedo>(count),
            A::MoveLines(direction) => {
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let mut last = Err("move_lines: no iterations".to_string());
                        for _ in 0..count.max(1) {
                            last = handle
                                .rpc::<InputMoveLines>(InputMoveLinesParams {
                                    buffer_id,
                                    direction,
                                })
                                .await
                                .map_err(|e| e.to_string());
                            if last.is_err() {
                                break;
                            }
                        }
                        last
                    },
                    Message::EditDone,
                )
            }
            A::JoinLines => self.repeat_edit::<InputJoinLines>(count),
            A::Indent => self.repeat_edit::<InputIndent>(count),
            A::Dedent => self.repeat_edit::<InputDedent>(count),
            A::ToggleComment => self.edit::<InputToggleComment>(BufferOnlyParams { buffer_id }),
            A::OpenLineBelow => {
                // Park at the line's end, newline-and-indent, stay in Insert (TUI semantics).
                self.session.mode = Mode::Insert;
                let line = self.session.buffer.cursor.position.line;
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let target = LogicalPosition {
                            line,
                            col: u32::MAX,
                        };
                        handle
                            .rpc::<CursorSet>(CursorSetParams {
                                buffer_id,
                                position: target,
                                anchor: target,
                                granularity: Granularity::Char,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        handle
                            .rpc::<InputNewlineAndIndent>(BufferOnlyParams { buffer_id })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::EditDone,
                )
            }
            A::OpenLineAbove => {
                // Park at col 0, insert "\n" (pushes the line down), step back up (TUI semantics).
                self.session.mode = Mode::Insert;
                let line = self.session.buffer.cursor.position.line;
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let target = LogicalPosition { line, col: 0 };
                        handle
                            .rpc::<CursorSet>(CursorSetParams {
                                buffer_id,
                                position: target,
                                anchor: target,
                                granularity: Granularity::Char,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let r = handle
                            .rpc::<InputText>(InputTextParams {
                                buffer_id,
                                text: "\n".into(),
                                select_pasted: false,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let cursor = handle
                            .rpc::<CursorMove>(CursorMoveParams {
                                buffer_id,
                                motion: Motion::LogicalLine {
                                    direction: Direction::Backward,
                                    count: 1,
                                    preserve_col: false,
                                },
                                extend_selection: false,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(EditResult {
                            revision: r.revision,
                            cursor,
                        })
                    },
                    Message::EditDone,
                )
            }

            // ---- clipboard ----
            A::Copy => self.copy(CopyScope::Selection),
            A::CopyLine => self.copy(CopyScope::Line),
            A::Cut => self.cut(CopyScope::Selection),
            A::CutLine => self.cut(CopyScope::Line),
            A::Paste => self.read_clipboard(PasteKind::Before { count }),
            A::ReplaceClipboard => self.read_clipboard(PasteKind::Replace { count }),
            A::PasteAtCursor => self.read_clipboard(PasteKind::AtCursor),
            A::ReplaceLineClipboard => self.read_clipboard(PasteKind::Line),
            A::Change => {
                self.session.mode = Mode::Insert;
                self.edit::<InputDelete>(BufferOnlyParams { buffer_id })
            }
            A::ChangeLine => self.edit::<InputChangeLine>(BufferOnlyParams { buffer_id }),
            A::BeginSurround(target) => {
                self.session.pending = Pending::Surround(target);
                Task::none()
            }
            A::Unsurround(target) => self.edit::<InputUnsurround>(InputUnsurroundParams {
                buffer_id,
                target,
            }),

            // ---- search ----
            A::EnterSearch => self.enter_search(false),
            A::EnterSearchToCursor => self.enter_search(true),
            A::SearchCommit => {
                self.session.search.snapshot = None;
                if self.session.search.query.is_empty() {
                    self.session.search.active = false;
                    self.session.search.summary = None;
                } else {
                    self.session.search.active = true;
                    let q = self.session.search.query.clone();
                    self.push_history(q);
                }
                self.session.search.history_cursor = None;
                self.session.search.history_draft.clear();
                self.session.search.extend_to_cursor = false;
                self.session.mode = Mode::Normal;
                Task::none()
            }
            A::SearchAbort => self.abort_search(),
            A::SearchHistoryPrev => {
                self.history_up();
                self.incremental_search()
            }
            A::SearchHistoryNext => {
                self.history_down();
                self.incremental_search()
            }
            A::SearchCursorLeft => {
                if let Some((i, _)) = self.session.search.query[..self.session.search.cursor]
                    .char_indices()
                    .last()
                {
                    self.session.search.cursor = i;
                }
                Task::none()
            }
            A::SearchCursorRight => {
                if let Some(c) = self.session.search.query[self.session.search.cursor..].chars().next() {
                    self.session.search.cursor += c.len_utf8();
                }
                Task::none()
            }
            A::SearchBackspace => {
                let Some((i, _)) = self.session.search.query[..self.session.search.cursor]
                    .char_indices()
                    .last()
                else {
                    return Task::none();
                };
                self.session.search.query.remove(i);
                self.session.search.cursor = i;
                self.session.search.history_cursor = None;
                self.incremental_search()
            }
            A::SearchCycle(direction) => self.search_cycle(direction, count, extend),
            A::SearchFromSelection => {
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let copy = handle
                            .rpc::<BufferCopy>(BufferCopyParams {
                                buffer_id,
                                scope: CopyScope::Selection,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        if copy.text.is_empty() {
                            return Ok(None);
                        }
                        let query = regex_escape(&copy.text);
                        let r = handle
                            .rpc::<SearchSet>(SearchSetParams {
                                buffer_id,
                                query: query.clone(),
                                anchor: None,
                                extend: false,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(Some((query, r)))
                    },
                    Message::SearchFromSel,
                )
            }
            A::GrepNavigate(direction) => {
                // Step through cached grep hits server-side, then open + prime in one chain.
                let handle = self.session.handle.clone();
                let roots = self.session.project_paths.clone();
                self.task(
                    async move {
                        let target = handle
                            .rpc::<PickerGrepNavigate>(PickerGrepNavigateParams {
                                direction,
                                buffer_id,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let Some(t) = target else { return Ok(None) };
                        let Some((path_index, relative_path)) =
                            strip_longest_root(&t.path, &roots)
                        else {
                            return Err(format!("{} is outside the project's roots", t.path));
                        };
                        let _ = handle.rpc::<NavRecord>(NavRecordParams { buffer_id }).await;
                        let open = handle
                            .rpc::<BufferOpen>(BufferOpenParams {
                                path_index: Some(path_index),
                                relative_path: Some(relative_path),
                                jump_to: Some(t.position),
                                transient: Some(true),
                                ..Default::default()
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let _ = handle
                            .rpc::<SearchSet>(SearchSetParams {
                                buffer_id: open.buffer_id,
                                query: t.query.clone(),
                                anchor: None,
                                extend: false,
                            })
                            .await;
                        Ok(Some((t.query, open)))
                    },
                    Message::SwitchedPrimed,
                )
            }
            A::DropSearch => {
                if !(self.session.search.active || self.session.search.summary.is_some()) {
                    return Task::none();
                }
                self.session.search.active = false;
                self.session.search.summary = None;
                self.rpc::<SearchClear>(SearchClearParams { buffer_id }, |_| Message::Noop)
            }

            // ---- app ----
            // The server tears down all per-client state on disconnect, so quitting is just
            // closing the window.
            A::Quit => iced::exit(),
            A::Save => self.save_task(None, false),
            A::SaveAs => {
                // Prefill with the buffer's current project-relative path, like the web dialog.
                let (path_index, input) = self.session
                    .buffer
                    .path
                    .as_deref()
                    .and_then(|p| strip_longest_root(p, &self.session.project_paths))
                    .unwrap_or((0, String::new()));
                self.session.prompt = Some(Prompt::SaveAs {
                    path_index,
                    cursor: input.len(),
                    input,
                });
                Task::none()
            }
            A::Reload => {
                if self.session.buffer.path.is_none() {
                    return self.toast("scratch buffer has no path to reload", ToastKind::Warning);
                }
                self.reload_task(false)
            }
            A::NewScratch => {
                // Opening a fresh scratch is a buffer switch — record the origin so Alt-Left
                // returns.
                let handle = self.session.handle.clone();
                self.task(
                    async move {
                        let _ = handle.rpc::<NavRecord>(NavRecordParams { buffer_id }).await;
                        handle
                            .rpc::<BufferOpen>(BufferOpenParams::default())
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::Switched,
                )
            }
            A::CloseBuffer => {
                if self.session.buffer.revision != self.session.buffer.saved_revision {
                    self.session.prompt = Some(Prompt::Confirm {
                        message: format!("discard unsaved changes in {}", self.session.buffer.label),
                        action: ConfirmAction::CloseDiscard,
                    });
                    return Task::none();
                }
                self.close_buffer_task()
            }

            // ---- git ----
            A::ToggleDiffView => {
                let Some(viewport_id) = self.session.viewport_id else {
                    return Task::none();
                };
                let enabled = !self.session.diff_view;
                self.rpc::<GitSetDiffView>(
                    GitSetDiffViewParams {
                        viewport_id,
                        enabled,
                    },
                    move |result| Message::DiffViewSet { enabled, result },
                )
            }
            A::NextHunk | A::PrevHunk => {
                let direction = if matches!(action, A::NextHunk) {
                    HunkDirection::Next
                } else {
                    HunkDirection::Prev
                };
                self.rpc::<GitNavigateHunk>(
                    GitNavigateHunkParams {
                        buffer_id,
                        from_line: self.session.buffer.cursor.position.line,
                        direction,
                    },
                    Message::HunkNav,
                )
            }
            A::ToggleStageHunk | A::RevertHunk => {
                let hunk_action = if matches!(action, A::ToggleStageHunk) {
                    HunkAction::Toggle
                } else {
                    HunkAction::Revert
                };
                self.rpc::<GitApplyHunk>(
                    GitApplyHunkParams {
                        buffer_id,
                        action: hunk_action,
                    },
                    move |result| Message::HunkApplied {
                        action: hunk_action,
                        result,
                    },
                )
            }

            // ---- pickers ----
            A::OpenPicker(PickerKind::Explorer) => self.open_explorer(false),
            A::OpenPicker(kind) => self.open_picker(kind, None, None),
            A::OpenPickerInBufferDir(kind) => self.open_picker_in_buffer_dir(kind),
            A::OpenExplorerAtRoot => self.open_explorer(true),

            // ---- LSP ----
            A::GotoDefinition => {
                self.rpc::<LspGotoDefinition>(LspBufferParams { buffer_id }, Message::Definition)
            }
            A::Hover => self.rpc::<LspHover>(LspBufferParams { buffer_id }, Message::HoverInfo),
            A::Format => self.rpc::<LspFormat>(LspBufferParams { buffer_id }, Message::FormatDone),
            A::ShowDiagnostic => self.show_diagnostic(),
            A::ShowCommitInfo => {
                // Blame the cursor line on demand (no ambient blame cache yet), then resolve
                // the commit's full details.
                let handle = self.session.handle.clone();
                let line = self.session.buffer.cursor.position.line;
                self.task(
                    async move {
                        let blame = handle
                            .rpc::<GitBlameLine>(GitBlameLineParams { buffer_id, line })
                            .await
                            .map_err(|e| e.to_string())?;
                        let info = match blame.blame {
                            Some(b) if b.is_uncommitted => {
                                return Ok(CommitDetails::Note(
                                    "Uncommitted line — no commit details",
                                ))
                            }
                            Some(b) => b,
                            None => {
                                return Ok(CommitDetails::Note(
                                    "No commit details for this line",
                                ))
                            }
                        };
                        let r = handle
                            .rpc::<GitCommitInfo>(GitCommitInfoParams {
                                buffer_id,
                                commit: info.commit,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(match r.info {
                            Some(info) => CommitDetails::Info(Box::new(info)),
                            None => CommitDetails::Note("Commit not found"),
                        })
                    },
                    Message::CommitLookup,
                )
            }
            A::NextDiagnostic | A::PrevDiagnostic => {
                let direction = if matches!(action, A::NextDiagnostic) {
                    DiagnosticDirection::Next
                } else {
                    DiagnosticDirection::Prev
                };
                self.rpc::<LspNavigateDiagnostic>(
                    LspNavigateDiagnosticParams {
                        buffer_id,
                        from_line: self.session.buffer.cursor.position.line,
                        direction,
                    },
                    Message::DiagNav,
                )
            }
        }
    }

    /// Rebind the window to another open buffer: reset per-buffer state and subscribe a fresh
    /// viewport (the server drops the old one on subscribe — one logical viewport per client).
    fn switch_to(&mut self, open: BufferOpenResult) -> Task<Message> {
        self.session.mode = Mode::Normal;
        self.session.pending = Pending::None;
        self.session.count = None;
        self.session.diagnostics = DiagnosticCounts::default();
        self.session.lsp = None;
        self.session.externally_modified = false;
        self.session.externally_deleted = false;
        self.session.window = None;
        self.session.viewport_id = None;
        self.session.fetch_in_flight = false;
        self.session.refetch_queued = false;
        self.session.reveal_after_fetch = false;
        self.session.drag = None;
        self.session.hover = None;
        self.session.blame = None;
        self.session.blame_requested = None;
        self.session.prompt = None;
        // An externally-triggered switch (buffer/closed) can land mid-pick: drop the panel;
        // stale pushes are discarded and the server quiesces on the next view/hide.
        self.session.picker = None;
        self.session.scroll_px = 0.0;
        self.session.scroll_x_px = 0.0;
        self.session.scroll_anim = None;
        // Search state is per-(client, buffer) server-side; reset the prompt state but keep
        // the query history (it survives switches in the web client too).
        let history = std::mem::take(&mut self.session.search.history);
        self.session.search = SearchState {
            history,
            ..SearchState::default()
        };
        self.session.buffer = buffer_info(open, &self.session.project_paths);
        self.subscribe_task()
    }

    /// Subscribe a viewport for the current buffer at the current grid size. Initial scroll:
    /// a restored position when the server has one, otherwise centred on the cursor (so a
    /// `jump_to` open lands on-screen).
    fn subscribe_task(&mut self) -> Task<Message> {
        let Some((cols, rows)) = self.session.sent_grid else {
            return Task::none(); // no metrics yet; the first Layout event subscribes
        };
        let scroll = self.session.buffer.scroll.unwrap_or(ScrollPosition {
            logical_line: self.session.buffer.cursor.position.line.saturating_sub(rows / 2),
            sub_row: 0.0,
        });
        self.session.subscribe_scroll = scroll;
        self.rpc::<ViewportSubscribe>(
            ViewportSubscribeParams {
                buffer_id: self.session.buffer.buffer_id,
                cols,
                rows,
                overscan_rows: rows,
                scroll,
                wrap: self.session.wrap,
                continuation_marker_width: grid::CONTINUATION_MARKER_COLS,
                tab_width: TAB_WIDTH,
            },
            Message::Subscribed,
        )
    }

    // ---- save / reload / close (ask-then-confirm handshakes) --------------------------------

    /// `buffer/save`, mapping the server's refusal codes to a `[y/N]` confirmation that retries
    /// with `overwrite: true`. `target` is the save-as `(path_index, relative_path)`.
    fn save_task(&self, target: Option<(u32, String)>, overwrite: bool) -> Task<Message> {
        use aether_protocol::error::ErrorCode;
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                let (path_index, relative_path) = match &target {
                    Some((i, p)) => (Some(*i), Some(p.clone())),
                    None => (None, None),
                };
                match handle
                    .rpc::<BufferSave>(BufferSaveParams {
                        buffer_id,
                        path_index,
                        relative_path,
                        overwrite,
                    })
                    .await
                {
                    Ok(result) => Ok(SaveTry::Saved { result, target }),
                    Err(e) if e.code == ErrorCode::WOULD_OVERWRITE.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: match &target {
                                Some((_, p)) => format!("overwrite {p}"),
                                None => "overwrite".into(),
                            },
                            action: ConfirmAction::Save { target },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_MODIFIED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: "file changed on disk — overwrite".into(),
                            action: ConfirmAction::Save { target },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_DELETED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: "file removed on disk — recreate".into(),
                            action: ConfirmAction::Save { target },
                        })
                    }
                    Err(e) => Err(e.to_string()),
                }
            },
            Message::SaveTried,
        )
    }

    /// `buffer/reload`, mapping `WOULD_DISCARD_CHANGES` to a confirmation that retries with
    /// `force: true`.
    fn reload_task(&self, force: bool) -> Task<Message> {
        use aether_protocol::error::ErrorCode;
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                match handle
                    .rpc::<BufferReload>(BufferReloadParams { buffer_id, force })
                    .await
                {
                    Ok(r) => Ok(ReloadTry::Reloaded(r)),
                    Err(e) if e.code == ErrorCode::WOULD_DISCARD_CHANGES.code() => {
                        Ok(ReloadTry::NeedsConfirm)
                    }
                    Err(e) => Err(e.to_string()),
                }
            },
            Message::ReloadTried,
        )
    }

    /// Close, then attach to the server-indicated next MRU buffer (or a fresh scratch).
    fn close_buffer_task(&self) -> Task<Message> {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                let closed = handle
                    .rpc::<BufferClose>(BufferCloseParams { buffer_id })
                    .await
                    .map_err(|e| e.to_string())?;
                handle
                    .rpc::<BufferOpen>(BufferOpenParams {
                        buffer_id: closed.next_buffer_id,
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::Switched,
        )
    }

    /// Keys while a modal prompt is open. Confirm: `y`/Enter accepts, anything else declines
    /// (the `[y/N]` default). Save-as: a one-line path editor.
    fn on_prompt_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        match self.session.prompt.take().unwrap() {
            Prompt::Confirm { message: _, action } => {
                let accepts = !mods.ctrl
                    && !mods.alt
                    && (code == KeyCode::Char('y') || code == KeyCode::Enter);
                if accepts {
                    self.run_confirm(action)
                } else {
                    self.decline_confirm(action);
                    Task::none()
                }
            }
            Prompt::LspInfo(info) => {
                // `r` restarts; any other key closes the dialog.
                if code == KeyCode::Char('r') && !mods.ctrl && !mods.alt {
                    let restart = self.rpc::<LspRestartServer>(
                        LspRestartServerParams {
                            language: info.language.clone(),
                        },
                        |_| Message::Noop,
                    );
                    let note = self.toast(format!("restarting {}", info.name), ToastKind::Info);
                    return Task::batch([restart, note]);
                }
                Task::none()
            }
            Prompt::SaveAs {
                path_index,
                mut input,
                mut cursor,
            } => {
                match code {
                    KeyCode::Esc => return Task::none(), // prompt stays closed
                    // Tab cycles the target root in multi-root projects.
                    KeyCode::Tab => {
                        let n = self.session.project_paths.len().max(1) as u32;
                        self.session.prompt = Some(Prompt::SaveAs {
                            path_index: (path_index + 1) % n,
                            input,
                            cursor,
                        });
                        return Task::none();
                    }
                    KeyCode::Enter => {
                        let path = input.trim().to_string();
                        if path.is_empty() {
                            self.session.prompt = Some(Prompt::SaveAs {
                                path_index,
                                input,
                                cursor,
                            });
                            return Task::none();
                        }
                        // An absolute path re-resolves against the project roots.
                        let target = if path.starts_with('/') {
                            match strip_longest_root(&path, &self.session.project_paths) {
                                Some(t) => t,
                                None => {
                                    return self
                                        .error(format!("{path} is outside the project's roots"));
                                }
                            }
                        } else {
                            (path_index, path)
                        };
                        return self.save_task(Some(target), false);
                    }
                    KeyCode::Backspace => {
                        if let Some((i, _)) = input[..cursor].char_indices().last() {
                            input.remove(i);
                            cursor = i;
                        }
                    }
                    KeyCode::Left => {
                        if let Some((i, _)) = input[..cursor].char_indices().last() {
                            cursor = i;
                        }
                    }
                    KeyCode::Right => {
                        if let Some(c) = input[cursor..].chars().next() {
                            cursor += c.len_utf8();
                        }
                    }
                    _ => {
                        if !mods.ctrl && !mods.alt {
                            if let Some(t) = text {
                                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                                input.insert_str(cursor, &t);
                                cursor += t.len();
                            }
                        }
                    }
                }
                self.session.prompt = Some(Prompt::SaveAs {
                    path_index,
                    input,
                    cursor,
                });
                Task::none()
            }
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) -> Task<Message> {
        match action {
            ConfirmAction::Save { target } => self.save_task(target, true),
            ConfirmAction::ReloadDiscard => self.reload_task(true),
            ConfirmAction::CloseDiscard => self.close_buffer_task(),
        }
    }

    /// Declining a save-as overwrite returns to the path input (the TUI keeps the prompt open
    /// beneath the confirm); other declines just close the dialog.
    fn decline_confirm(&mut self, action: ConfirmAction) {
        if let ConfirmAction::Save {
            target: Some((path_index, input)),
        } = action
        {
            self.session.prompt = Some(Prompt::SaveAs {
                path_index,
                cursor: input.len(),
                input,
            });
        }
    }

    /// The prompt's Yes/Save button.
    fn accept_prompt(&mut self) -> Task<Message> {
        match self.session.prompt.take() {
            Some(Prompt::Confirm { action, .. }) => self.run_confirm(action),
            Some(p @ Prompt::SaveAs { .. }) => {
                // Submit via the same path as Enter.
                self.session.prompt = Some(p);
                self.on_prompt_key(KeyCode::Enter, Mods::default(), None)
            }
            Some(Prompt::LspInfo(_)) | None => Task::none(),
        }
    }

    fn decline_prompt(&mut self) {
        if let Some(Prompt::Confirm { action, .. }) = self.session.prompt.take() {
            self.decline_confirm(action);
        }
    }

    // ---- pickers ----------------------------------------------------------------------------

    /// Open a picker: subscribe a window and let `picker/update` pushes fill it. Grep resumes
    /// its prior query/hits (centred on the cursor's nearest hit); the rest reset.
    /// `directory_path` seeds the Explorer's listing (its `Space e` = the buffer's directory).
    /// `seed_filters` replaces the server's persisted set (Explorer→Grep/Files switches,
    /// `Space Alt-f/g`); the echo through `PickerViewed` rebuilds the chip row.
    fn open_picker(
        &mut self,
        kind: PickerKind,
        directory_path: Option<String>,
        seed_filters: Option<PickerFilters>,
    ) -> Task<Message> {
        let reset = !kind.preserves_state();
        self.session.picker = Some(PickerState::new(kind));
        let buffer_id = self.session.buffer.buffer_id;
        // Buffers / Projects: default the highlight to the first item that isn't the active
        // buffer/project, so Enter is a quick flip to the previous one (web/TUI behaviour).
        // Resolved by the first non-empty push.
        let skip = match kind {
            PickerKind::Buffers => Some(crate::picker::DefaultSkip::Buffer(buffer_id)),
            PickerKind::Projects => Some(crate::picker::DefaultSkip::Project(
                self.session.project.clone(),
            )),
            _ => None,
        };
        if let Some(p) = &mut self.session.picker {
            p.default_skip = skip;
        }
        // Explorer: anchor the highlight on the active buffer's filename, so the listing lands
        // on "where you are" (matched by name via the `effective_center_on` echo).
        let center_on = (kind == PickerKind::Explorer)
            .then(|| {
                let path = self.session.buffer.path.as_deref()?;
                let name = std::path::Path::new(path).file_name()?.to_str()?.to_string();
                Some(PickerItem::DirEntry {
                    name,
                    is_dir: false,
                    match_indices: Vec::new(),
                    git_status: None,
                })
            })
            .flatten();
        self.rpc::<PickerView>(
            PickerViewParams {
                kind,
                reset,
                offset: 0,
                limit: FETCH_LIMIT,
                center_on,
                center_on_cursor_grep_hit: (kind == PickerKind::Grep).then_some(buffer_id),
                directory_path,
                explorer_roots: false,
                buffer_id: matches!(kind, PickerKind::Diagnostics | PickerKind::References)
                    .then_some(buffer_id),
                filters: seed_filters,
            },
            move |result| Message::PickerViewed {
                initial: true,
                result,
            },
        )
    }

    /// `Space Alt-f` / `Space Alt-g`: open Files/Grep pre-scoped to the active buffer's
    /// directory — a normal dir filter chip, visible and removable. Falls back to an unscoped
    /// open for scratch buffers or files outside every root.
    fn open_picker_in_buffer_dir(&mut self, kind: PickerKind) -> Task<Message> {
        let seed = self.session
            .buffer
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).parent())
            .map(|p| p.display().to_string())
            .and_then(|dir| strip_longest_root(&dir, &self.session.project_paths))
            .map(|(path_index, relative_path)| PickerFilters {
                directories: vec![ScopedPath {
                    path_index,
                    relative_path,
                }],
                ..PickerFilters::default()
            });
        self.open_picker(kind, None, seed)
    }

    /// `Ctrl-g` / `Ctrl-f` in the Explorer: switch to the Grep / Files picker scoped to the
    /// directory being browsed ("grep here"), the explorer's filters translated along. In
    /// Roots mode no dir scope is seeded — the target covers the whole project.
    fn switch_explorer_picker(&mut self, target: PickerKind) -> Task<Message> {
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        if p.kind != PickerKind::Explorer {
            return Task::none();
        }
        let dir_scope = p
            .directory
            .as_deref()
            .and_then(|abs| strip_longest_root(abs, &self.session.project_paths))
            .map(|(path_index, relative_path)| ScopedPath {
                path_index,
                relative_path,
            });
        let seeded = seeded_filters_for_switch(&p.wire_filters(), dir_scope, target);
        let hide = self.close_picker();
        Task::batch([hide, self.open_picker(target, None, Some(seeded))])
    }

    /// `Space e` / `Space Alt-e`: Explorer at the buffer's directory, or at its project root.
    /// Scratch buffers fall through to the server default (last listing / first root).
    fn open_explorer(&mut self, at_root: bool) -> Task<Message> {
        let dir = self.session.buffer.path.as_deref().and_then(|path| {
            if at_root {
                let (i, _) = strip_longest_root(path, &self.session.project_paths)?;
                self.session.project_paths.get(i as usize).cloned()
            } else {
                std::path::Path::new(path)
                    .parent()
                    .map(|p| p.display().to_string())
            }
        });
        self.open_picker(PickerKind::Explorer, dir, None)
    }

    /// Explorer navigation: list a different directory (or the project roots). Clears the
    /// query — entering a directory starts a fresh listing — but the filter chips ride along
    /// (a `-hidden` toggled while browsing keeps applying in the next directory).
    /// `pre_select` lands the highlight on the named entry once the listing arrives — Alt-h
    /// pre-selects the directory being left, keeping the user's bearings.
    fn explorer_navigate(
        &mut self,
        directory_path: Option<String>,
        roots: bool,
        pre_select: Option<String>,
    ) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        p.generation += 1;
        p.query.clear();
        p.cursor = 0;
        p.selected = 0;
        p.offset = 0;
        p.scroll_y = 0.0;
        p.items.clear();
        let generation = p.generation;
        let filters = p.wire_filters();
        let center_on = pre_select.map(|name| PickerItem::DirEntry {
            name,
            is_dir: true,
            match_indices: Vec::new(),
            git_status: None,
        });
        let clear_query = self.rpc::<PickerQuery>(
            PickerQueryParams {
                kind: PickerKind::Explorer,
                query: String::new(),
                generation,
                // The query RPC replaces the persisted filters too — carry the chips so a
                // racing arrival order can't wipe them under the view below.
                filters: filters.clone(),
            },
            |_| Message::Noop,
        );
        let view = self.rpc::<PickerView>(
            PickerViewParams {
                kind: PickerKind::Explorer,
                reset: false,
                offset: 0,
                limit: FETCH_LIMIT,
                center_on,
                center_on_cursor_grep_hit: None,
                directory_path,
                explorer_roots: roots,
                buffer_id: None,
                filters: Some(filters),
            },
            |result| Message::PickerViewed {
                initial: false,
                result,
            },
        );
        Task::batch([clear_query, view])
    }

    /// Move the picker highlight, refetching when it leaves the fetched window and scrolling
    /// the native list the minimum to keep it visible.
    fn picker_move(&mut self, delta: i64) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        let refetch = p.move_selection(delta);
        match refetch {
            Some(offset) => self.picker_refetch(offset),
            None => self.picker_reveal_selected(),
        }
    }

    /// Scroll the results list so the highlighted row is inside the viewport (keyboard moves;
    /// native wheel scrolling never goes through here).
    fn picker_reveal_selected(&mut self) -> Task<Message> {
        self.picker_reveal_selected_with(Reveal::Minimal)
    }

    /// [`Self::picker_reveal_selected`], parameterised: `Top` aligns the row to the top of the
    /// pane unless it's already visible (grep file-jumps — landing on a new file reveals it
    /// from its first hit without yanking an in-view jump).
    fn picker_reveal_selected_with(&mut self, reveal: Reveal) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        reveal_picker_selection(p, reveal)
    }

    /// Re-subscribe the picker's window at a new offset (the highlight moved past it).
    fn picker_refetch(&mut self, offset: u32) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        p.offset = offset;
        p.items.clear();
        let kind = p.kind;
        self.rpc::<PickerView>(
            PickerViewParams {
                kind,
                reset: false,
                offset,
                limit: FETCH_LIMIT,
                center_on: None,
                center_on_cursor_grep_hit: None,
                directory_path: None,
                explorer_roots: false,
                buffer_id: None,
                filters: None,
            },
            move |result| Message::PickerViewed {
                initial: false,
                result,
            },
        )
    }

    /// A query edit: bump the generation (stale pushes get discarded), restart the window at
    /// the top, and tell the server.
    fn picker_query_changed(&mut self) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        p.generation += 1;
        p.selected = 0;
        p.offset = 0;
        p.scroll_y = 0.0;
        // A query change invalidates any pending pre-selection (centering / skip-the-active-
        // item default) — the user is steering somewhere new.
        p.pending_center = None;
        p.default_skip = None;
        p.reveal_on_update = None;
        let (kind, query, generation) = (p.kind, p.query.clone(), p.generation);
        let filters = p.wire_filters();
        let query_task = self.rpc::<PickerQuery>(
            PickerQueryParams {
                kind,
                query,
                generation,
                filters,
            },
            |_| Message::Noop,
        );
        Task::batch([query_task, self.picker_refetch(0)])
    }

    /// Push a filter (chip) change. For Grep/Files a filter change *is* a query change (same
    /// generation mechanics); for the Explorer the filters apply when the listing is built, so
    /// re-view the current directory with the replacement set. No-op for kinds that take no
    /// filters, and for the Explorer's Roots mode (nothing to filter there).
    fn apply_picker_filter_change(&mut self) -> Task<Message> {
        let Some(kind) = self.session.picker.as_ref().map(|p| p.kind) else {
            return Task::none();
        };
        match kind {
            PickerKind::Grep | PickerKind::Files => self.picker_query_changed(),
            PickerKind::Explorer => {
                let filters = {
                    let Some(p) = &mut self.session.picker else {
                        return Task::none();
                    };
                    if p.directory.is_none() {
                        return Task::none(); // Roots mode
                    }
                    p.selected = 0;
                    p.offset = 0;
                    p.scroll_y = 0.0;
                    p.items.clear();
                    p.wire_filters()
                };
                self.rpc::<PickerView>(
                    PickerViewParams {
                        kind: PickerKind::Explorer,
                        reset: false,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: None,
                        center_on_cursor_grep_hit: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        filters: Some(filters),
                    },
                    |result| Message::PickerViewed {
                        initial: false,
                        result,
                    },
                )
            }
            _ => Task::none(),
        }
    }

    /// Toggle/cycle the filter a chord (or Enter on a selected chip) names, then push the
    /// change. A chord that doesn't apply to this picker kind is a clean no-op.
    fn toggle_picker_filter(&mut self, id: ChipId) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        if !chips::filter_applies(p.kind, id) {
            return Task::none();
        }
        let explorer = p.kind == PickerKind::Explorer;
        if !chips::apply_chip_toggle(&mut p.chips, id, explorer) {
            return Task::none(); // valued chips (dir, glob) go through their editors
        }
        self.apply_picker_filter_change()
    }

    /// `Enter` on a selected chip: valued chips re-open their editor pre-filled; everything
    /// else toggles/cycles in place (a plain boolean's chip disappears).
    fn edit_selected_chip(&mut self, id: ChipId) -> Task<Message> {
        match id {
            ChipId::Glob(i) => self.open_glob_prompt(Some(i)),
            ChipId::Dir(i) => self.open_dir_prompt(Some(i)),
            _ => self.toggle_picker_filter(id),
        }
    }

    /// Open the glob editor line. `edit: Some(i)` pre-fills glob `i`; `None` adds a new chip
    /// on commit.
    fn open_glob_prompt(&mut self, edit: Option<usize>) -> Task<Message> {
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        if !chips::filter_applies(p.kind, ChipId::Glob(0)) {
            return Task::none();
        }
        // The editor owns the keys now; a lingering chip selection would go stale once the
        // commit reshapes the row.
        p.chip_selected = None;
        let prefill = edit
            .and_then(|i| p.glob_value(i))
            .map(str::to_string)
            .unwrap_or_default();
        p.chip_editor = Some(ChipEditor::glob(prefill, edit));
        Task::none()
    }

    /// Open the directory-scope editor line. `edit: Some(i)` re-opens scope `i` pre-filled
    /// (path focused); `None` adds a new chip on commit (multi-root projects focus the root
    /// segment first). Kicks off a `directory/list` so the path field's ghost suggestions are
    /// ready when focus lands there.
    fn open_dir_prompt(&mut self, edit: Option<usize>) -> Task<Message> {
        let project_paths = self.session.project_paths.clone();
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        if !chips::filter_applies(p.kind, ChipId::Dir(0)) {
            return Task::none();
        }
        p.chip_selected = None;
        let current = edit.and_then(|i| p.dir_value(i).cloned());
        let multi_root = project_paths.len() > 1;
        let root_index = current.as_ref().map(|d| d.path_index).unwrap_or(0);
        let field = if multi_root && current.is_none() {
            ChipEditorField::Root
        } else {
            ChipEditorField::Path
        };
        let mut ed = ChipEditor::dir(
            current.map(|d| d.relative_path).unwrap_or_default(),
            field,
            root_index,
            edit,
        );
        ed.sync_dir_listing(&project_paths);
        p.chip_editor = Some(ed);
        self.refresh_chip_editor_listing()
    }

    /// Fire `directory/list` for the dir-chip editor's current (root, dir-portion) pair. The
    /// requested path rides on the result message so a stale response (the editor moved on)
    /// can be discarded. No-op for glob editors and invalid roots.
    fn refresh_chip_editor_listing(&mut self) -> Task<Message> {
        let project_paths = self.session.project_paths.clone();
        let Some(path) = self.session
            .picker
            .as_ref()
            .and_then(|p| p.chip_editor.as_ref())
            .and_then(|ed| ed.dir_listing_path(&project_paths))
        else {
            return Task::none();
        };
        let abs = path.clone();
        self.rpc::<DirectoryList>(DirectoryListParams { path }, move |result| {
            Message::PickerChipListing {
                abs: abs.clone(),
                result,
            }
        })
    }

    /// Commit the chip editor line. A dir editor only commits a *valid* scope — a root that
    /// matches some label and a path that exists (or is empty); otherwise the editor stays
    /// open with the invalid segment rendered red.
    fn commit_chip_editor(&mut self) -> Task<Message> {
        let project_paths = self.session.project_paths.clone();
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        if let Some(ed) = p.chip_editor.as_ref() {
            if ed.is_dir() {
                let root_ok = project_paths.len() < 2 || {
                    let labels = crate::labels::root_labels(&project_paths);
                    !ed.root_invalid(&labels)
                };
                if !root_ok || !ed.path_valid() {
                    return Task::none();
                }
            }
        }
        let Some(ed) = p.chip_editor.take() else {
            return Task::none();
        };
        // A partially typed leaf commits as its highlighted completion — `committed_path` is
        // the typed text for glob editors and whenever there's nothing to complete.
        let text = ed.committed_path().trim().trim_matches('/').to_string();
        let changed = match ed.kind {
            chips::ChipEditorKind::Glob { edit } => {
                let normalized = chips::normalize_glob(&ed.input.text);
                chips::commit_glob_edit(&mut p.chips, normalized, edit)
            }
            chips::ChipEditorKind::Dir { edit } => {
                // An empty path is a whole-root scope in multi-root projects and clears the
                // chip in single-root ones (where "the whole root" means "no narrowing").
                let multi_root = project_paths.len() > 1;
                let value = if text.is_empty() && !multi_root {
                    None
                } else {
                    let labels = crate::labels::root_labels(&project_paths);
                    let path_index = if multi_root { ed.chosen_root(&labels) } else { 0 };
                    Some(ScopedPath {
                        path_index,
                        relative_path: text,
                    })
                };
                chips::commit_dir_edit(&mut p.chips, value, edit)
            }
        };
        if !changed {
            return Task::none();
        }
        self.apply_picker_filter_change()
    }

    /// Alt-l: descend into the highlighted explorer directory (Enter does too, via accept).
    fn explorer_enter_selected(&mut self) -> Task<Message> {
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        if let Some(PickerItem::DirEntry {
            name,
            is_dir: true,
            ..
        }) = p.selected_item()
        {
            let dir = match &p.directory {
                Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                None => return Task::none(),
            };
            return self.explorer_navigate(Some(dir), false, None);
        }
        Task::none()
    }

    /// Alt-h / Alt-Backspace: progressively unwind — clear the query, then pop the rightmost
    /// filter chip (one per press), then (explorer) one directory segment per press — landing
    /// the highlight on the directory just left — then roots mode in multi-root projects.
    fn picker_back(&mut self) -> Task<Message> {
        let project_paths = self.session.project_paths.clone();
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        if !p.query.is_empty() {
            p.query.clear();
            p.cursor = 0;
            return self.picker_query_changed();
        }
        if let Some(chip) = p.chip_row(&project_paths).last().map(|c| c.id) {
            chips::remove_chip(&mut p.chips, chip);
            p.chip_selected = None;
            return self.apply_picker_filter_change();
        }
        if p.kind != PickerKind::Explorer {
            return Task::none();
        }
        match p.directory_parent.clone() {
            Some(parent) => {
                // Pre-select the directory we're leaving in the parent's listing.
                let leaving = p.directory.as_deref().and_then(|d| {
                    std::path::Path::new(d)
                        .file_name()
                        .and_then(|os| os.to_str())
                        .map(str::to_string)
                });
                self.explorer_navigate(Some(parent), false, leaving)
            }
            None if p.directory.is_some() => {
                if project_paths.len() > 1 {
                    self.explorer_navigate(None, true, None)
                } else {
                    Task::none()
                }
            }
            None => Task::none(),
        }
    }

    /// Enter / row click: act on the highlighted item. Directories and roots navigate within
    /// the open explorer; everything else closes the panel and runs `picker/select`.
    fn picker_accept(&mut self) -> Task<Message> {
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        let Some(item) = p.selected_item().cloned() else {
            return Task::none();
        };
        match &item {
            PickerItem::DirEntry {
                name,
                is_dir: true,
                ..
            } => {
                let dir = match &p.directory {
                    Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                    None => return Task::none(),
                };
                return self.explorer_navigate(Some(dir), false, None);
            }
            PickerItem::Root { path_index, .. } => {
                let dir = self.session.project_paths.get(*path_index as usize).cloned();
                return self.explorer_navigate(dir, false, None);
            }
            PickerItem::LspServer {
                name,
                language,
                workspace_root,
                root_label,
                status,
                progress,
                ..
            } => {
                // Not a jump target: Enter drills into the detail dialog (restart lives there
                // and on Ctrl-r in the list).
                let info = LspServerStatus {
                    name: name.clone(),
                    language: language.clone(),
                    workspace_root: workspace_root.clone(),
                    status: status.clone(),
                    progress: progress.clone(),
                };
                let _ = root_label;
                let hide = self.close_picker();
                self.session.prompt = Some(Prompt::LspInfo(Box::new(info)));
                return hide;
            }
            _ => {}
        }
        let kind = p.kind;
        let prime = (kind == PickerKind::Grep).then(|| p.query.clone());
        let hide = self.close_picker();
        let select = self.rpc::<PickerSelect>(PickerSelectParams { kind, item }, move |result| {
            Message::PickerSelected {
                prime: prime.clone(),
                result,
            }
        });
        Task::batch([hide, select])
    }

    /// Drop the panel and unsubscribe (the server keeps walker/matcher state for resume).
    fn close_picker(&mut self) -> Task<Message> {
        let Some(p) = self.session.picker.take() else {
            return Task::none();
        };
        let kind = p.kind;
        self.rpc::<PickerHide>(PickerHideParams { kind }, |_| Message::Noop)
    }

    /// Keys while a picker is open: list navigation + query editing.
    fn on_picker_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        // The chip editor line (glob/dir, revealed below the input) owns the keys while open.
        if self.session
            .picker
            .as_ref()
            .is_some_and(|p| p.chip_editor.is_some())
        {
            return self.on_chip_editor_key(code, mods, text);
        }
        let project_paths = self.session.project_paths.clone();
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        let no_chord = !mods.ctrl && !mods.alt;
        // A selected chip captures the editing keys (Enter edits, Backspace/Delete removes,
        // Left/Right walk the row, Esc deselects, typing deselects back into the query).
        // Anything else falls through to the normal picker vocabulary below.
        if let Some(sel) = p.chip_selected {
            let row = p.chip_row(&project_paths);
            if row.is_empty() {
                p.chip_selected = None;
            } else {
                let sel = sel.min(row.len() - 1);
                match code {
                    KeyCode::Left if no_chord => {
                        p.chip_selected = Some(sel.saturating_sub(1));
                        return Task::none();
                    }
                    KeyCode::Right if no_chord => {
                        if sel + 1 >= row.len() {
                            p.chip_selected = None;
                        } else {
                            p.chip_selected = Some(sel + 1);
                        }
                        return Task::none();
                    }
                    KeyCode::Esc => {
                        p.chip_selected = None;
                        return Task::none();
                    }
                    KeyCode::Backspace | KeyCode::Delete if no_chord => {
                        chips::remove_chip(&mut p.chips, row[sel].id);
                        let remaining = row.len() - 1;
                        p.chip_selected =
                            (remaining > 0).then(|| sel.min(remaining - 1));
                        return self.apply_picker_filter_change();
                    }
                    KeyCode::Enter if no_chord => {
                        return self.edit_selected_chip(row[sel].id);
                    }
                    KeyCode::Char(_) if no_chord => {
                        // Typing returns to the query — fall through so the char lands.
                        p.chip_selected = None;
                    }
                    _ => {}
                }
            }
        }
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        match code {
            KeyCode::Esc => return self.close_picker(),
            KeyCode::Enter => return self.picker_accept(),
            // Alt-k/j move the highlight (Up/Down deliberately don't, matching the others).
            KeyCode::Char('k') if mods.alt && !mods.ctrl => return self.picker_move(-1),
            KeyCode::Char('j') if mods.alt && !mods.ctrl => return self.picker_move(1),
            // `Ctrl-g` / `Ctrl-f` in the Explorer: switch to Grep / Files scoped to the
            // browsed directory ("grep here").
            KeyCode::Char('g')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer =>
            {
                return self.switch_explorer_picker(PickerKind::Grep);
            }
            KeyCode::Char('f')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer =>
            {
                return self.switch_explorer_picker(PickerKind::Files);
            }
            // Alt-l/h are per-kind: Explorer descends / ascends; Grep jumps the selection to
            // the next / previous file's first hit; elsewhere Alt-h clears (via picker_back).
            KeyCode::Char('l')
                if mods.alt && !mods.ctrl && p.kind == PickerKind::Explorer =>
            {
                return self.explorer_enter_selected();
            }
            KeyCode::Char('l') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(Direction::Forward);
            }
            KeyCode::Char('h') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(Direction::Backward);
            }
            // Alt-h / Alt-Backspace unwind: clear the query first, then pop chips, then step
            // to the parent (one segment per press), then roots mode (multi-root only).
            KeyCode::Char('h') if mods.alt && !mods.ctrl => return self.picker_back(),
            KeyCode::Backspace if mods.alt && !mods.ctrl => return self.picker_back(),
            // Filter-chip chords (docs/picker-filters.md). Booleans toggle in place; valued
            // filters open the editor line. Gated per kind inside the helpers.
            KeyCode::Char('c') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Case);
            }
            KeyCode::Char('w') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Word);
            }
            KeyCode::Char('e') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Lit);
            }
            KeyCode::Char('i') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Ignored);
            }
            KeyCode::Char('.') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Hidden);
            }
            KeyCode::Char('m') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Changed);
            }
            KeyCode::Char('g') if mods.alt && !mods.ctrl => {
                return self.open_glob_prompt(None);
            }
            KeyCode::Char('d') if mods.alt && !mods.ctrl => {
                return self.open_dir_prompt(None);
            }
            KeyCode::PageUp => {
                return self.picker_move(-(crate::picker::VISIBLE_ROWS as i64 - 1));
            }
            KeyCode::PageDown => {
                return self.picker_move(crate::picker::VISIBLE_ROWS as i64 - 1);
            }
            // (Backspace below is pure query editing; explorer navigation lives on
            // Alt-h/Alt-Backspace.)
            // LspServers: Ctrl-r restarts the highlighted server in place.
            KeyCode::Char('r')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::LspServers =>
            {
                if let Some(PickerItem::LspServer { name, language, .. }) = p.selected_item() {
                    let (name, language) = (name.clone(), language.clone());
                    let restart = self.rpc::<LspRestartServer>(
                        LspRestartServerParams { language },
                        |_| Message::Noop,
                    );
                    let note = self.toast(format!("restarting {name}"), ToastKind::Info);
                    return Task::batch([restart, note]);
                }
                return Task::none();
            }
            // `Backspace` at the start of the query selects the rightmost chip (a second
            // press deletes it — two-stage, so holding backspace through a query can't
            // silently destroy a carefully typed glob).
            KeyCode::Backspace if no_chord => {
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.query.remove(i);
                    p.cursor = i;
                    return self.picker_query_changed();
                }
                let n = p.chip_row(&project_paths).len();
                if n > 0 {
                    p.chip_selected = Some(n - 1);
                }
                return Task::none();
            }
            // `Left` at the start of the query steps into the chip row (rightmost first) —
            // the browser tag-input gesture.
            KeyCode::Left if no_chord => {
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.cursor = i;
                } else {
                    let n = p.chip_row(&project_paths).len();
                    if n > 0 {
                        p.chip_selected = Some(n - 1);
                    }
                }
                return Task::none();
            }
            KeyCode::Right if no_chord => {
                if let Some(c) = p.query[p.cursor..].chars().next() {
                    p.cursor += c.len_utf8();
                }
                return Task::none();
            }
            _ => {}
        }
        if no_chord {
            if let Some(t) = text {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    let at = p.cursor;
                    p.query.insert_str(at, &t);
                    p.cursor = at + t.len();
                    return self.picker_query_changed();
                }
            }
        }
        Task::none()
    }

    /// Keys while the chip editor line is open. The dir editor reads as one `dir: root: path`
    /// field: Tab / Alt-l accept the focused segment's ghost (root — adopting it and moving
    /// into the path; path — absorbing the next directory segment), `:` on a completed root
    /// value moves into the path, Alt-j/k cycle the focused segment's matches, Alt-Backspace
    /// pops a path segment (then, at an empty path, clears the root selection), and plain
    /// Backspace at an empty path steps back into the root. Enter commits, Esc cancels.
    fn on_chip_editor_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Task<Message> {
        let project_paths = self.session.project_paths.clone();
        let labels = crate::labels::root_labels(&project_paths);
        let Some(p) = &mut self.session.picker else {
            return Task::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Task::none();
        };
        let is_dir = ed.is_dir();
        let multi_root_dir = is_dir && project_paths.len() > 1;
        let in_root = multi_root_dir && ed.field == ChipEditorField::Root;
        let no_chord = !mods.ctrl && !mods.alt;
        // Whether the path field's suggestion listing went stale and needs a directory/list.
        let mut refresh = false;
        match code {
            KeyCode::Enter if no_chord => return self.commit_chip_editor(),
            KeyCode::Esc => {
                p.chip_editor = None;
                return Task::none();
            }
            // Tab / Alt-l: accept the focused segment's suggestion. Root — adopt the ghost
            // completion and continue right into the path; path — absorb the ghost directory
            // segment (repeated presses walk down the tree).
            KeyCode::Tab if no_chord && is_dir => {
                if in_root {
                    refresh = ed.commit_root_field(&labels, &project_paths);
                } else {
                    refresh = ed.accept_path_suggestion(&project_paths);
                }
            }
            KeyCode::Char('l') if mods.alt && !mods.ctrl && is_dir => {
                if in_root {
                    refresh = ed.commit_root_field(&labels, &project_paths);
                } else {
                    refresh = ed.accept_path_suggestion(&project_paths);
                }
            }
            KeyCode::Char('h') if mods.alt && !mods.ctrl && multi_root_dir => {
                ed.field = ChipEditorField::Root;
            }
            // `:` on a completed root value confirms it and moves into the path — it's the
            // separator you'd type next. On an incomplete value it's swallowed (`:` can never
            // extend a root-label prefix match).
            KeyCode::Char(':') if !mods.ctrl && !mods.alt && in_root => {
                if ed.root_complete(&labels) {
                    refresh = ed.commit_root_field(&labels, &project_paths);
                }
            }
            // Alt-Backspace: in the dir editor's path it deletes the rightmost segment,
            // fish-style; at an empty path it clears the root selection (the next rung of the
            // progressive unwind). In the root and glob fields it clears the field outright.
            KeyCode::Backspace if mods.alt && !mods.ctrl => {
                if is_dir && ed.field == ChipEditorField::Path {
                    if ed.input.text.is_empty() {
                        if multi_root_dir {
                            ed.field = ChipEditorField::Root;
                            ed.root_filter.clear();
                            ed.root_selected = 0;
                        }
                    } else {
                        refresh = ed.pop_path_segment(&project_paths);
                    }
                } else if in_root {
                    ed.root_filter.clear();
                    ed.root_selected = 0;
                } else {
                    ed.input.clear();
                }
            }
            // Backspace at an empty path steps back into the root field — the same leftward
            // gesture the chip row uses from the query.
            KeyCode::Backspace
                if no_chord
                    && multi_root_dir
                    && ed.field == ChipEditorField::Path
                    && ed.input.text.is_empty() =>
            {
                ed.field = ChipEditorField::Root;
            }
            // Cycle the focused segment's matches: root typeahead candidates (wrapping), or
            // the path field's directory suggestions (clamped). Glob: no-op — reserved for
            // input history, matching the search bar.
            KeyCode::Char(c @ ('j' | 'k')) if mods.alt && !mods.ctrl => {
                let down = c == 'j';
                if in_root {
                    let n = chips::root_candidates(&labels, &ed.root_filter.text).len();
                    if n > 0 {
                        let sel = ed.root_selected.min(n - 1);
                        ed.root_selected = if down { (sel + 1) % n } else { (sel + n - 1) % n };
                        // The chosen root moved — the path now resolves under it.
                        refresh = ed.sync_dir_listing(&project_paths);
                    }
                } else if is_dir {
                    ed.cycle_path_suggestion(down);
                }
            }
            KeyCode::Backspace if no_chord => {
                if in_root {
                    if ed.root_filter.backspace() {
                        // The match set changed under the highlight — snap back to the best
                        // match; the chosen root may have moved under existing path text.
                        ed.root_selected = 0;
                        refresh = ed.sync_dir_listing(&project_paths);
                    }
                } else if ed.input.backspace() && is_dir {
                    refresh = ed.path_edited(&project_paths);
                }
            }
            KeyCode::Left if no_chord => {
                if in_root {
                    ed.root_filter.move_left();
                } else {
                    ed.input.move_left();
                }
            }
            KeyCode::Right if no_chord => {
                if in_root {
                    ed.root_filter.move_right();
                } else {
                    ed.input.move_right();
                }
            }
            _ => {
                if no_chord {
                    if let Some(t) = text {
                        let t: String = t.chars().filter(|c| !c.is_control()).collect();
                        if !t.is_empty() {
                            if in_root {
                                ed.root_filter.insert_str(&t);
                                ed.root_selected = 0;
                                refresh = ed.sync_dir_listing(&project_paths);
                            } else {
                                ed.input.insert_str(&t);
                                if is_dir {
                                    refresh = ed.path_edited(&project_paths);
                                }
                            }
                        }
                    }
                }
            }
        }
        if refresh {
            return self.refresh_chip_editor_listing();
        }
        Task::none()
    }

    /// Jump the grep picker's selection to the first hit of the next / previous file. The
    /// server finds the boundary across the *whole* result list (so it works past the
    /// over-fetch window); the result lands as [`Message::GrepFileJumped`].
    fn grep_jump_file(&mut self, direction: Direction) -> Task<Message> {
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        if p.kind != PickerKind::Grep || p.items.is_empty() {
            return Task::none();
        }
        self.rpc::<PickerGrepFileJump>(
            PickerGrepFileJumpParams {
                from_index: p.selected,
                direction,
            },
            Message::GrepFileJumped,
        )
    }

    /// `Space j` — show the diagnostic(s) at the cursor in the hover box. Prefers diagnostics
    /// under the cursor column (zero-width points widened to one cell), falling back to all on
    /// the line. Reads the cached window render — no round-trip.
    fn show_diagnostic(&mut self) -> Task<Message> {
        let cursor = self.session.buffer.cursor.position;
        let diags: Vec<(DiagnosticSeverity, String)> = self.session
            .window
            .as_ref()
            .and_then(|w| w.lines.iter().find(|l| l.logical_line == cursor.line))
            .map(|line| {
                let under: Vec<_> = line
                    .diagnostics
                    .iter()
                    .filter(|d| cursor.col >= d.start && cursor.col < d.end.max(d.start + 1))
                    .map(|d| (d.severity, d.message.clone()))
                    .collect();
                if under.is_empty() {
                    line.diagnostics
                        .iter()
                        .map(|d| (d.severity, d.message.clone()))
                        .collect()
                } else {
                    under
                }
            })
            .unwrap_or_default();
        if diags.is_empty() {
            self.session.hover = None;
            return self.toast("No diagnostics on this line", ToastKind::Info);
        }
        self.session.hover = Some(HoverContent::Blocks(
            diags
                .into_iter()
                .map(|(severity, msg)| HoverBlock {
                    text: format!("{}: {msg}", severity_label(severity)),
                    severity: Some(severity),
                })
                .collect(),
        ));
        Task::none()
    }

    // ---- search ---------------------------------------------------------------------------

    /// `/` or `?`: open the search prompt. Snapshots cursor/scroll/query for Esc-restore and
    /// clears the server-side search so stale highlights disappear immediately.
    fn enter_search(&mut self, extend_to_cursor: bool) -> Task<Message> {
        self.session.search.snapshot = Some(SearchSnapshot {
            cursor: self.session.buffer.cursor,
            scroll_px: self.session.scroll_px,
            query: std::mem::take(&mut self.session.search.query),
            active: self.session.search.active,
        });
        self.session.search.active = false;
        self.session.search.summary = None;
        self.session.search.history_cursor = None;
        self.session.search.history_draft.clear();
        self.session.search.extend_to_cursor = extend_to_cursor;
        self.session.search.cursor = 0;
        self.session.mode = Mode::Search;
        let buffer_id = self.session.buffer.buffer_id;
        self.rpc::<SearchClear>(SearchClearParams { buffer_id }, |_| Message::Noop)
    }

    /// One incremental step: hand the server the latest query; it jumps the cursor to the
    /// first match at-or-after the prompt's entry point. An emptied query clears instead.
    fn incremental_search(&mut self) -> Task<Message> {
        let buffer_id = self.session.buffer.buffer_id;
        if self.session.search.query.is_empty() {
            self.session.search.summary = None;
            let clear =
                self.rpc::<SearchClear>(SearchClearParams { buffer_id }, |_| Message::Noop);
            return Task::batch([clear, self.revert_to_snapshot_cursor()]);
        }
        self.rpc::<SearchSet>(
            SearchSetParams {
                buffer_id,
                query: self.session.search.query.clone(),
                anchor: self.session
                    .search
                    .snapshot
                    .as_ref()
                    .map(|s| min_pos(s.cursor.position, s.cursor.anchor)),
                extend: self.session.search.extend_to_cursor,
            },
            Message::SearchApplied,
        )
    }

    /// Move the cursor back to where the prompt opened (no-op outside incremental search or
    /// when it hasn't moved).
    fn revert_to_snapshot_cursor(&mut self) -> Task<Message> {
        let Some(snap) = self.session.search.snapshot.as_ref() else {
            return Task::none();
        };
        if self.session.buffer.cursor.position == snap.cursor.position
            && self.session.buffer.cursor.anchor == snap.cursor.anchor
        {
            return Task::none();
        }
        let (position, anchor) = (snap.cursor.position, snap.cursor.anchor);
        self.rpc::<CursorSet>(
            CursorSetParams {
                buffer_id: self.session.buffer.buffer_id,
                position,
                anchor,
                granularity: Granularity::Char,
            },
            Message::CursorMsg,
        )
    }

    /// Esc in the prompt: restore the pre-prompt search (query + server state), cursor, and
    /// scroll.
    fn abort_search(&mut self) -> Task<Message> {
        self.session.mode = Mode::Normal;
        self.session.search.extend_to_cursor = false;
        self.session.search.history_cursor = None;
        self.session.search.history_draft.clear();
        let Some(snap) = self.session.search.snapshot.take() else {
            return Task::none();
        };
        let buffer_id = self.session.buffer.buffer_id;
        let restore_search = if snap.active && !snap.query.is_empty() {
            self.rpc::<SearchSet>(
                SearchSetParams {
                    buffer_id,
                    query: snap.query.clone(),
                    anchor: None,
                    extend: false,
                },
                Message::SearchRestored,
            )
        } else {
            self.session.search.summary = None;
            self.rpc::<SearchClear>(SearchClearParams { buffer_id }, |_| Message::Noop)
        };
        self.session.search.cursor = snap.query.len();
        self.session.search.query = snap.query;
        self.session.search.active = snap.active;
        let (position, anchor) = (snap.cursor.position, snap.cursor.anchor);
        let restore_cursor = self.rpc::<CursorSet>(
            CursorSetParams {
                buffer_id,
                position,
                anchor,
                granularity: Granularity::Char,
            },
            Message::CursorMsg,
        );
        self.scroll_to_px(snap.scroll_px, false);
        Task::batch([restore_search, restore_cursor])
    }

    /// `n`/`Alt-n`: step match-to-match; with no active search, revive the most recent history
    /// entry first. Steps run sequentially in one task.
    fn search_cycle(&mut self, direction: Direction, count: u32, extend: bool) -> Task<Message> {
        let revive = if self.session.search.active {
            None
        } else {
            match self.session.search.history.last().cloned() {
                Some(q) => {
                    self.session.search.cursor = q.len();
                    self.session.search.query = q.clone();
                    self.session.search.active = true;
                    Some(q)
                }
                None => return Task::none(),
            }
        };
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                if let Some(query) = revive {
                    let r = handle
                        .rpc::<SearchSet>(SearchSetParams {
                            buffer_id,
                            query,
                            anchor: None,
                            extend: false,
                        })
                        .await
                        .map_err(|e| e.to_string())?;
                    if r.summary.total == 0 {
                        return Ok(SearchNavResult {
                            cursor: r.cursor,
                            summary: r.summary,
                        });
                    }
                }
                let mut last: Result<SearchNavResult, String> =
                    Err("search_cycle: no iterations".into());
                for _ in 0..count.max(1) {
                    let params = SearchNavParams { buffer_id, extend };
                    last = match direction {
                        Direction::Forward => handle.rpc::<SearchNext>(params).await,
                        Direction::Backward => handle.rpc::<SearchPrev>(params).await,
                    }
                    .map_err(|e| e.to_string());
                    if last.is_err() {
                        break;
                    }
                }
                last
            },
            Message::SearchNav,
        )
    }

    /// Printable input in the prompt (control keys took the Search-table path).
    fn on_search_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        if let Some(b) = keymap::lookup(KeyContext::Search, code, mods) {
            return self.run_action(b.action, 1, false);
        }
        if mods.ctrl || mods.alt {
            return Task::none();
        }
        let Some(t) = text else {
            return Task::none();
        };
        let t: String = t.chars().filter(|c| !c.is_control()).collect();
        if t.is_empty() {
            return Task::none();
        }
        let at = self.session.search.cursor;
        self.session.search.query.insert_str(at, &t);
        self.session.search.cursor = at + t.len();
        self.session.search.history_cursor = None;
        self.incremental_search()
    }

    fn push_history(&mut self, query: String) {
        const SEARCH_HISTORY_MAX: usize = 100;
        if query.is_empty() || self.session.search.history.last() == Some(&query) {
            return;
        }
        self.session.search.history.push(query);
        let overflow = self.session.search.history.len().saturating_sub(SEARCH_HISTORY_MAX);
        if overflow > 0 {
            self.session.search.history.drain(..overflow);
        }
    }

    fn history_up(&mut self) {
        if self.session.search.history.is_empty() {
            return;
        }
        let idx = match self.session.search.history_cursor {
            None => {
                self.session.search.history_draft = self.session.search.query.clone();
                self.session.search.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.session.search.history_cursor = Some(idx);
        self.session.search.query = self.session.search.history[idx].clone();
        self.session.search.cursor = self.session.search.query.len();
    }

    fn history_down(&mut self) {
        match self.session.search.history_cursor {
            None => {} // already past the newest entry
            Some(i) if i + 1 < self.session.search.history.len() => {
                self.session.search.history_cursor = Some(i + 1);
                self.session.search.query = self.session.search.history[i + 1].clone();
                self.session.search.cursor = self.session.search.query.len();
            }
            Some(_) => {
                self.session.search.history_cursor = None;
                self.session.search.query = std::mem::take(&mut self.session.search.history_draft);
                self.session.search.cursor = self.session.search.query.len();
            }
        }
    }

    /// Open a resolved source location (goto-definition): record the jump origin, open the
    /// target as a transient preview with the cursor on the definition.
    fn open_location(&mut self, location: LspLocation) -> Task<Message> {
        self.open_path(location.path, Some(location.position))
    }

    /// Open a file by absolute path as a transient preview — result-style navigation (picker
    /// selections, goto-definition). Records the jump origin onto the nav history first.
    /// `prime_search` (grep flows) also sets the opened buffer's search to that query so
    /// `n`/`Alt-n` step matches.
    fn open_path_primed(
        &mut self,
        path: String,
        jump_to: Option<LogicalPosition>,
        prime_search: Option<String>,
    ) -> Task<Message> {
        let Some((path_index, relative_path)) = strip_longest_root(&path, &self.session.project_paths)
        else {
            return self.error(format!("{path} is outside the project's roots"));
        };
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let open_chain = move || async move {
            let _ = handle.rpc::<NavRecord>(NavRecordParams { buffer_id }).await;
            let open = handle
                .rpc::<BufferOpen>(BufferOpenParams {
                    path_index: Some(path_index),
                    relative_path: Some(relative_path),
                    jump_to,
                    transient: Some(true),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())?;
            if let Some(query) = &prime_search {
                let _ = handle
                    .rpc::<SearchSet>(SearchSetParams {
                        buffer_id: open.buffer_id,
                        query: query.clone(),
                        anchor: None,
                        extend: false,
                    })
                    .await;
            }
            Ok((prime_search, open))
        };
        self.task(open_chain(), |r: Result<(Option<String>, BufferOpenResult), String>| {
            match r {
                Ok((Some(query), open)) => Message::SwitchedPrimed(Ok(Some((query, open)))),
                Ok((None, open)) => Message::Switched(Ok(open)),
                Err(e) => Message::Switched(Err(e)),
            }
        })
    }

    fn open_path(&mut self, path: String, jump_to: Option<LogicalPosition>) -> Task<Message> {
        self.open_path_primed(path, jump_to, None)
    }

    // ---- RPC helpers ------------------------------------------------------------------------

    /// One reconnect attempt, after `attempt`'s backoff: re-run discovery
    /// (a restarted daemon gets a fresh port), dial, re-activate the project, and reopen the
    /// buffer — by path when it has one (transient flag preserved, cursor as the jump target),
    /// by id otherwise (recovers a scratch's content when the daemon stayed up), falling back
    /// to a fresh transient scratch. Dial failures retry via [`ReconnectError::NotUp`];
    /// post-dial failures are terminal ([`ReconnectError::Fatal`]).
    fn try_reconnect(&self, attempt: u32) -> Task<Message> {
        let s = &self.session;
        if !matches!(s.conn, ConnState::Reconnecting { .. }) {
            return Task::none();
        }
        let version = self.client_version.clone();
        let project = s.project.clone();
        let path = s.buffer.path.clone();
        let buffer_id = s.buffer.buffer_id;
        let transient = s.buffer.transient;
        let cursor = s.buffer.cursor.position;
        self.task(
            async move {
                tokio::time::sleep(reconnect_backoff(attempt)).await;
                let info = crate::discovery::read().map_err(|_| ReconnectError::NotUp)?;
                let server_url = format!("ws://127.0.0.1:{}", info.port);
                let (handle, rx) = crate::connection::connect(&server_url, &version)
                    .await
                    .map_err(|_| ReconnectError::NotUp)?;
                let activated = handle
                    .rpc::<ProjectActivate>(ProjectActivateParams { name: project })
                    .await
                    .map_err(|e| ReconnectError::Fatal(e.to_string()))?;
                let params = match &path {
                    Some(p) => strip_longest_root(p, &activated.project.paths).map(
                        |(path_index, relative_path)| BufferOpenParams {
                            path_index: Some(path_index),
                            relative_path: Some(relative_path),
                            // The old session's transient stayed a preview; reopen it as one
                            // rather than silently promoting it.
                            transient: transient.then_some(true),
                            jump_to: Some(cursor),
                            ..Default::default()
                        },
                    ),
                    // A scratch has no path; reopening by id recovers its content when the
                    // daemon stayed up across the drop.
                    None => Some(BufferOpenParams {
                        buffer_id: Some(buffer_id),
                        ..Default::default()
                    }),
                };
                let mut open = None;
                if let Some(params) = params {
                    open = handle.rpc::<BufferOpen>(params).await.ok();
                }
                let open = match open {
                    Some(o) => o,
                    // The buffer is gone (daemon restarted; a dead scratch, or the file moved)
                    // — fall back to a fresh transient scratch placeholder.
                    None => handle
                        .rpc::<BufferOpen>(BufferOpenParams {
                            transient: Some(true),
                            ..Default::default()
                        })
                        .await
                        .map_err(|e| ReconnectError::Fatal(e.to_string()))?,
                };
                Ok(Box::new(Reestablished {
                    handle,
                    notifications: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
                    project: activated.project,
                    open,
                    server_url,
                    server_started_at: info.started_at_unix_ms,
                }))
            },
            Message::Reconnected,
        )
    }

    /// Swap a fresh connection into the session and re-establish its view: new pump,
    /// resubscribe at the saved cursor, restore the selection and any committed search. The
    /// old client_id's undo stack and nav history are gone (server-side, per-client) — the
    /// same trade the web client makes.
    fn adopt_reconnect(&mut self, r: Reestablished) -> Task<Message> {
        let had_unsaved = matches!(
            self.session.conn,
            ConnState::Reconnecting {
                had_unsaved: true,
                ..
            }
        );
        let restarted = r.server_started_at != self.server_started_at;
        tracing::info!(restarted, url = %r.server_url, "reconnected");
        self.server_started_at = r.server_started_at;
        let s = &mut self.session;
        let old_cursor = s.buffer.cursor;
        s.handle = r.handle;
        s.notifications = r.notifications.clone();
        s.project = r.project.name;
        s.project_paths = r.project.paths;
        let same_file = r.open.path == s.buffer.path;
        s.buffer = buffer_info(r.open, &s.project_paths);
        s.conn = ConnState::Connected;
        // Server-side per-client state died with the old connection; drop the client overlays
        // that fronted it. The frozen window stays rendered until the resubscribe replaces it.
        s.viewport_id = None;
        s.sent_grid = None;
        s.fetch_in_flight = false;
        s.refetch_queued = false;
        s.scroll_anim = None;
        s.blame = None;
        s.blame_requested = None;
        s.hover = None;
        s.prompt = None;
        s.picker = None;
        let buffer_id = s.buffer.buffer_id;
        let pump = pump(r.notifications);
        // Restore a selection (jump_to only carried the cursor): same buffer only, and a
        // failure (the file shrank on disk) keeps the server's default rather than erroring.
        let restore_sel = if same_file && old_cursor.anchor != old_cursor.position {
            self.rpc::<CursorSet>(
                CursorSetParams {
                    buffer_id,
                    position: old_cursor.position,
                    anchor: old_cursor.anchor,
                    granularity: Granularity::Char,
                },
                |r| match r {
                    Ok(c) => Message::CursorMsg(Ok(c)),
                    Err(_) => Message::Noop,
                },
            )
        } else {
            Task::none()
        };
        // Re-prime a committed search so highlights and `n` survive the drop.
        let search = &self.session.search;
        let restore_search = if same_file && search.active && !search.query.is_empty() {
            self.rpc::<SearchSet>(
                SearchSetParams {
                    buffer_id,
                    query: search.query.clone(),
                    anchor: None,
                    extend: false,
                },
                Message::SearchRestored,
            )
        } else {
            Task::none()
        };
        // Resubscribe at the current grid (the first Layout event covers the no-metrics case).
        let grid = self.current_grid();
        let subscribe = if grid.is_some() {
            self.session.sent_grid = grid;
            self.subscribe_task()
        } else {
            Task::none()
        };
        let note = if restarted && had_unsaved {
            self.toast(
                "reconnected — the server restarted, unsaved changes were lost",
                ToastKind::Warning,
            )
        } else {
            self.toast("reconnected", ToastKind::Success)
        };
        Task::batch([pump, subscribe, restore_sel, restore_search, note])
    }

    /// The viewport grid for the current cell metrics + editor area, as sent to the server.
    fn current_grid(&self) -> Option<(u32, u32)> {
        let cell = self.cell?;
        let cols = ((self.view_size.width / cell.width) as u32).saturating_sub(GUTTER_COLS);
        let rows = (((self.view_size.height - PAD) / cell.height).max(1.0)) as u32;
        (cols > 0 && rows > 0).then_some((cols, rows))
    }

    /// Run a future, mapping its output to a message.
    fn task<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = T> + Send + 'static,
        f: impl Fn(T) -> Message + Send + 'static,
    ) -> Task<Message> {
        Task::perform(fut, f)
    }

    fn read_clipboard(&self, kind: PasteKind) -> Task<Message> {
        iced::clipboard::read().map(move |t| Message::ClipboardRead(kind, t))
    }

    fn rpc<M>(
        &self,
        params: M::Params,
        f: impl Fn(Result<M::Result, String>) -> Message + Send + 'static,
    ) -> Task<Message>
    where
        M: RpcMethod + 'static,
        M::Params: Send,
        M::Result: Send,
    {
        let handle = self.session.handle.clone();
        self.task(
            async move { handle.rpc::<M>(params).await.map_err(|e| e.to_string()) },
            f,
        )
    }

    fn move_motion(&self, motion: Motion, extend: bool) -> Task<Message> {
        self.rpc::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.session.buffer.buffer_id,
                motion,
                extend_selection: extend,
            },
            Message::CursorMsg,
        )
    }

    fn edit<M>(&self, params: M::Params) -> Task<Message>
    where
        M: RpcMethod<Result = EditResult> + 'static,
        M::Params: Send,
    {
        self.rpc::<M>(params, Message::EditDone)
    }

    /// Run an edit `count` times sequentially (the TUI's `for _ in 0..count` loops).
    fn repeat_edit<M>(&self, count: u32) -> Task<Message>
    where
        M: RpcMethod<Params = BufferOnlyParams, Result = EditResult> + 'static,
    {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                let mut last = Err("no iterations".to_string());
                for _ in 0..count.max(1) {
                    last = handle
                        .rpc::<M>(BufferOnlyParams { buffer_id })
                        .await
                        .map_err(|e| e.to_string());
                    if last.is_err() {
                        break;
                    }
                }
                last
            },
            Message::EditDone,
        )
    }

    /// Tree expand/contract: repeat until the cursor stops changing (root / empty history).
    fn repeat_cursor<M>(&self, count: u32) -> Task<Message>
    where
        M: RpcMethod<Params = CursorBufferOnlyParams, Result = CursorState> + 'static,
    {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let mut prev = self.session.buffer.cursor;
        self.task(
            async move {
                for _ in 0..count.max(1) {
                    match handle.rpc::<M>(CursorBufferOnlyParams { buffer_id }).await {
                        Ok(new) if new == prev => break,
                        Ok(new) => prev = new,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(prev)
            },
            Message::CursorMsg,
        )
    }

    /// Cursor-motion undo/redo: repeat until the history runs dry.
    fn motion_history<M>(&self, count: u32) -> Task<Message>
    where
        M: RpcMethod<
                Params = CursorUndoParams,
                Result = aether_protocol::cursor::CursorUndoResult,
            > + 'static,
    {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let mut cursor = self.session.buffer.cursor;
        self.task(
            async move {
                for _ in 0..count.max(1) {
                    match handle.rpc::<M>(CursorUndoParams { buffer_id }).await {
                        Ok(r) => {
                            if r.applied {
                                cursor = r.cursor;
                            } else {
                                break;
                            }
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(cursor)
            },
            Message::CursorMsg,
        )
    }

    /// Buffer undo/redo: repeat until the stack runs dry.
    fn undo_redo<M>(&self, count: u32) -> Task<Message>
    where
        M: RpcMethod<Params = BufferOnlyParams, Result = UndoResult> + 'static,
    {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        self.task(
            async move {
                let mut last = Err("no iterations".to_string());
                for _ in 0..count.max(1) {
                    match handle.rpc::<M>(BufferOnlyParams { buffer_id }).await {
                        Ok(r) => {
                            let applied = r.applied;
                            last = Ok(r);
                            if !applied {
                                break;
                            }
                        }
                        Err(e) => {
                            last = Err(e.to_string());
                            break;
                        }
                    }
                }
                last
            },
            Message::UndoRedoDone,
        )
    }

    /// `i`/`a`/`Alt-i`/`Alt-a` — the TUI's `enter_insert_at` RPC chains.
    fn enter_insert_at(&self, where_: InsertWhere) -> Task<Message> {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let cursor = self.session.buffer.cursor;
        let set = move |handle: Handle, target: LogicalPosition| async move {
            handle
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id,
                    position: target,
                    anchor: target,
                    granularity: Granularity::Char,
                })
                .await
                .map_err(|e| e.to_string())
        };
        self.task(
            async move {
                match where_ {
                    InsertWhere::SelectionStart => {
                        set(handle, min_pos(cursor.position, cursor.anchor)).await
                    }
                    InsertWhere::SelectionEnd => {
                        // Set to the selection's max, then step one char forward server-side
                        // (handles multi-byte chars / end-of-line).
                        let max = max_pos(cursor.position, cursor.anchor);
                        set(handle.clone(), max).await?;
                        handle
                            .rpc::<CursorMove>(CursorMoveParams {
                                buffer_id,
                                motion: Motion::Char {
                                    direction: Direction::Forward,
                                    count: 1,
                                },
                                extend_selection: false,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    }
                    InsertWhere::FirstLineStart => {
                        let line = cursor.position.line.min(cursor.anchor.line);
                        set(handle.clone(), LogicalPosition { line, col: 0 }).await?;
                        handle
                            .rpc::<CursorMove>(CursorMoveParams {
                                buffer_id,
                                motion: Motion::LineFirstNonblank,
                                extend_selection: false,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    }
                    InsertWhere::LastLineEnd => {
                        let line = cursor.position.line.max(cursor.anchor.line);
                        set(
                            handle,
                            LogicalPosition {
                                line,
                                col: u32::MAX,
                            },
                        )
                        .await
                    }
                }
            },
            Message::CursorMsg,
        )
    }

    fn copy(&self, scope: CopyScope) -> Task<Message> {
        self.rpc::<BufferCopy>(
            BufferCopyParams {
                buffer_id: self.session.buffer.buffer_id,
                scope,
            },
            Message::CopyDone,
        )
    }

    fn cut(&self, scope: CopyScope) -> Task<Message> {
        self.rpc::<BufferCut>(
            BufferCopyParams {
                buffer_id: self.session.buffer.buffer_id,
                scope,
            },
            Message::CutDone,
        )
    }

    /// Apply clipboard text per the paste flavour (the TUI's paste_* helpers).
    fn paste(&mut self, kind: PasteKind, text: String) -> Task<Message> {
        let handle = self.session.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        match kind {
            PasteKind::Before { count } => {
                let text = text.repeat(count.max(1) as usize);
                let start = min_pos(self.session.buffer.cursor.position, self.session.buffer.cursor.anchor);
                self.task(
                    async move {
                        handle
                            .rpc::<CursorSet>(CursorSetParams {
                                buffer_id,
                                position: start,
                                anchor: start,
                                granularity: Granularity::Char,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        handle
                            .rpc::<InputText>(InputTextParams {
                                buffer_id,
                                text,
                                select_pasted: true,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::EditDone,
                )
            }
            PasteKind::Replace { count } => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: text.repeat(count.max(1) as usize),
                select_pasted: true,
            }),
            PasteKind::AtCursor => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text,
                select_pasted: false,
            }),
            PasteKind::Line => self.edit::<InputReplaceLine>(InputReplaceLineParams {
                buffer_id,
                text,
            }),
        }
    }

    // ---- scroll / view sync -----------------------------------------------------------------

    fn visible_rows(&self) -> u32 {
        match self.cell {
            Some(cell) => (((self.view_size.height - PAD) / cell.height) as u32).max(1),
            None => 1,
        }
    }

    fn scroll_by(&mut self, delta_px: f32) {
        // Direct (wheel/trackpad) input overrides any animated glide in flight.
        self.session.scroll_anim = None;
        self.session.scroll_px += delta_px;
        self.clamp_scroll();
    }

    /// Horizontal scroll (no-op under soft wrap, where content always fits).
    fn scroll_x_by(&mut self, delta_px: f32) {
        if self.session.wrap != WrapMode::None || delta_px == 0.0 {
            return;
        }
        self.session.scroll_x_px = (self.session.scroll_x_px + delta_px).clamp(0.0, self.max_scroll_x_px());
    }

    fn max_scroll_x_px(&self) -> f32 {
        match (&self.session.window, self.cell) {
            (Some(w), Some(cell)) => {
                let content_w = self.view_size.width - (GUTTER_COLS as f32 + 1.0) * cell.width;
                (w.max_line_width as f32 * cell.width - content_w).max(0.0)
            }
            _ => 0.0,
        }
    }

    fn max_scroll_px(&self) -> f32 {
        match (&self.session.window, self.cell) {
            (Some(w), Some(cell)) => {
                (PAD * 2.0 + w.total_visual_rows as f32 * cell.height - self.view_size.height)
                    .max(0.0)
            }
            _ => 0.0,
        }
    }

    fn clamp_scroll(&mut self) {
        self.session.scroll_px = self.session.scroll_px.clamp(0.0, self.max_scroll_px());
    }

    /// Scroll to `target` px — animated when the move is short enough to look good (the web
    /// client's `scrollTopTo`): smooth within ~1.5 viewports, snap beyond (a long glide would
    /// sail over not-yet-loaded rows and storm the server with window fetches).
    fn scroll_to_px(&mut self, target: f32, smooth: bool) {
        let target = target.clamp(0.0, self.max_scroll_px());
        let delta = (target - self.session.scroll_px).abs();
        let max_smooth = self
            .cell
            .map(|c| self.visible_rows() as f32 * c.height * 1.5)
            .unwrap_or(0.0);
        if smooth && delta > 0.0 && delta <= max_smooth {
            self.session.scroll_anim = Some(ScrollAnim {
                from: self.session.scroll_px,
                to: target,
                started: std::time::Instant::now(),
            });
        } else {
            self.session.scroll_anim = None;
            self.session.scroll_px = target;
        }
    }

    /// Where the view is headed: the animation target while a glide is in flight, the current
    /// offset otherwise — keypress-repeat scrolling accumulates from here.
    fn scroll_target(&self) -> f32 {
        self.session.scroll_anim
            .as_ref()
            .map(|a| a.to)
            .unwrap_or(self.session.scroll_px)
    }

    /// Fetch a new window when the view nears the loaded range's edge (web's `onScroll`).
    fn maybe_fetch(&mut self) -> Task<Message> {
        let (Some(window), Some(cell), Some(viewport_id)) =
            (&self.session.window, self.cell, self.session.viewport_id)
        else {
            return Task::none();
        };
        let top_row = (((self.session.scroll_px - PAD) / cell.height).floor()).max(0.0) as u32;
        let loaded_start = window.first_visual_row;
        let loaded_end = loaded_start + loaded_rows(window);
        let margin = self.visible_rows();
        let visible = self.visible_rows();
        let need_above = loaded_start > 0 && top_row < loaded_start.saturating_add(margin);
        let need_below = loaded_end < window.total_visual_rows
            && top_row + visible > loaded_end.saturating_sub(margin);
        if !(need_above || need_below) {
            return Task::none();
        }
        if self.session.fetch_in_flight {
            self.session.refetch_queued = true;
            return Task::none();
        }
        self.session.fetch_in_flight = true;
        self.rpc::<ViewportScrollToRow>(
            ViewportScrollToRowParams {
                viewport_id,
                top_visual_row: top_row,
            },
            Message::WindowUpdate,
        )
    }

    /// After a cursor move: fetch around the cursor when it left the loaded window, otherwise
    /// scroll the minimum to reveal it (web's `ensureCursorVisible` + `revealCursor`).
    fn ensure_cursor_visible(&mut self) -> Task<Message> {
        let blame = self.maybe_blame();
        let reveal = self.ensure_cursor_visible_inner();
        Task::batch([blame, reveal])
    }

    /// Keep the cursor-line blame fresh: re-request when the cursor changed lines or the
    /// buffer changed underneath it. Scratch buffers (no path) show none.
    fn maybe_blame(&mut self) -> Task<Message> {
        let line = self.session.buffer.cursor.position.line;
        let key = (line, self.session.buffer.revision);
        if self.session.buffer.path.is_none() {
            self.session.blame = None;
            return Task::none();
        }
        if self.session.blame_requested == Some(key) {
            return Task::none();
        }
        self.session.blame_requested = Some(key);
        if self.session.blame.as_ref().is_some_and(|(l, _)| *l != line) {
            self.session.blame = None; // stale line's text shouldn't linger while the request flies
        }
        let buffer_id = self.session.buffer.buffer_id;
        self.rpc::<GitBlameLine>(GitBlameLineParams { buffer_id, line }, move |result| {
            Message::BlameLine {
                buffer_id,
                line,
                result,
            }
        })
    }

    fn ensure_cursor_visible_inner(&mut self) -> Task<Message> {
        let Some(window) = &self.session.window else {
            return Task::none();
        };
        let line = self.session.buffer.cursor.position.line;
        if line < window.first_logical_line || line >= window.last_logical_line_exclusive {
            let Some(viewport_id) = self.session.viewport_id else {
                return Task::none();
            };
            self.session.reveal_after_fetch = true;
            self.session.fetch_in_flight = true;
            return self.rpc::<ViewportScroll>(
                ViewportScrollParams {
                    viewport_id,
                    scroll: ScrollPosition {
                        logical_line: line,
                        sub_row: 0.0,
                    },
                },
                Message::WindowUpdate,
            );
        }
        self.reveal_cursor();
        self.maybe_fetch()
    }

    fn reveal_cursor(&mut self) {
        let (Some(cell), Some(window)) = (self.cell, &self.session.window) else {
            return;
        };
        let Some((row, dcol, _)) =
            grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH)
        else {
            return;
        };
        let h = cell.height;
        let top = PAD + row as f32 * h;
        // Overscroll by half a row so the cursor lands just inside the edge.
        let margin = h / 2.0;
        let view_h = self.view_size.height;
        if top - margin < self.session.scroll_px {
            self.scroll_to_px((top - margin).max(0.0), true);
        } else if top + h + margin > self.session.scroll_px + view_h {
            self.scroll_to_px(top + h + margin - view_h, true);
        }
        // Horizontal (no-wrap): keep the cursor's column clear of the gutter and right edge.
        if self.session.wrap == WrapMode::None {
            let cx = dcol as f32 * cell.width; // content-space x
            let content_w = self.view_size.width - (GUTTER_COLS as f32 + 1.0) * cell.width;
            if cx < self.session.scroll_x_px {
                self.session.scroll_x_px = cx;
            } else if cx + cell.width > self.session.scroll_x_px + content_w {
                self.session.scroll_x_px = cx + cell.width - content_w;
            }
            self.session.scroll_x_px = self.session.scroll_x_px.clamp(0.0, self.max_scroll_x_px());
        }
    }

    fn center_cursor(&mut self) {
        let (Some(cell), Some(window)) = (self.cell, &self.session.window) else {
            return;
        };
        let Some((row, _, _)) =
            grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH)
        else {
            return;
        };
        self.scroll_to_px(
            PAD + row as f32 * cell.height - self.view_size.height / 2.0,
            true,
        );
    }

    fn apply_window(&mut self, window: Window) {
        self.session.window = Some(window);
        self.clamp_scroll();
    }

    // ---- notifications ------------------------------------------------------------------------

    fn on_notification(
        &mut self,
        n: aether_protocol::envelope::Notification,
    ) -> Task<Message> {
        match n.method.as_str() {
            ViewportLinesChanged::NAME => {
                let Ok(p) = serde_json::from_value::<ViewportLinesChangedParams>(n.params) else {
                    return Task::none();
                };
                if Some(p.viewport_id) != self.session.viewport_id {
                    return Task::none();
                }
                // The notification carries the freshly rendered window for the loaded range —
                // apply it directly, keep the revision fresh (edits that only arrive this way,
                // e.g. another client's), and keep the cursor in view under the new geometry.
                self.session.buffer.revision = p.revision;
                self.apply_window(Window {
                    first_logical_line: p.range.start_logical_line,
                    last_logical_line_exclusive: p.range.end_logical_line_exclusive,
                    line_count: p.line_count,
                    max_scroll_logical_line: p.max_scroll_logical_line,
                    total_visual_rows: p.total_visual_rows,
                    first_visual_row: p.first_visual_row,
                    max_line_width: p.max_line_width,
                    git_status: p.git_status,
                    lines: p.replacement_lines,
                });
                self.reveal_cursor();
                Task::none()
            }
            BufferState::NAME => {
                let Ok(p) = serde_json::from_value::<BufferStateParams>(n.params) else {
                    return Task::none();
                };
                if p.buffer_id != self.session.buffer.buffer_id {
                    return Task::none();
                }
                self.session.buffer.saved_revision = p.saved_revision;
                self.session.buffer.transient = p.transient;
                let was_external = self.session.externally_modified || self.session.externally_deleted;
                self.session.externally_modified = p.externally_modified;
                self.session.externally_deleted = p.externally_deleted;
                if !was_external && p.externally_deleted {
                    self.toast(
                        "file removed on disk — save to recreate, or close",
                        ToastKind::Warning,
                    )
                } else if !was_external && p.externally_modified {
                    self.toast(
                        "file changed on disk — save to overwrite, or reload",
                        ToastKind::Warning,
                    )
                } else {
                    Task::none()
                }
            }
            LspDiagnosticsChanged::NAME => {
                if let Ok(p) = serde_json::from_value::<LspDiagnosticsChangedParams>(n.params) {
                    if p.buffer_id == self.session.buffer.buffer_id {
                        self.session.diagnostics = p.counts;
                    }
                }
                Task::none()
            }
            PickerUpdate::NAME => {
                if let Ok(u) = serde_json::from_value::<PickerUpdateParams>(n.params) {
                    if let Some(p) = &mut self.session.picker {
                        if p.apply_update(u) && p.pending_center.is_none() {
                            if let Some(reveal) = p.reveal_on_update.take() {
                                return self.picker_reveal_selected_with(reveal);
                            }
                        }
                    }
                }
                Task::none()
            }
            SearchStateChanged::NAME => {
                // Matches recomputed (buffer edit) or the cursor crossed a match boundary.
                if let Ok(s) = serde_json::from_value::<SearchSummary>(n.params) {
                    if s.buffer_id == self.session.buffer.buffer_id
                        && (self.session.search.active || self.session.mode == Mode::Search)
                    {
                        self.session.search.summary = Some(s);
                    }
                }
                Task::none()
            }
            LspStatusChanged::NAME => {
                if let Ok(s) = serde_json::from_value::<LspServerStatus>(n.params) {
                    let matches = self.session.buffer.lsp_server.as_ref().is_some_and(|r| {
                        r.language == s.language && r.workspace_root == s.workspace_root
                    });
                    if matches {
                        self.session.lsp = Some(s);
                    }
                }
                Task::none()
            }
            BufferClosed::NAME => {
                // Another client (or a path/project deletion) closed a buffer; if it's ours,
                // switch to the server-indicated next buffer (or a fresh scratch).
                let Ok(p) = serde_json::from_value::<BufferClosedParams>(n.params) else {
                    return Task::none();
                };
                if p.buffer_id != self.session.buffer.buffer_id {
                    return Task::none();
                }
                let note = self.toast("buffer closed by another client", ToastKind::Warning);
                let handle = self.session.handle.clone();
                let switch = self.task(
                    async move {
                        handle
                            .rpc::<BufferOpen>(BufferOpenParams {
                                buffer_id: p.next_buffer_id,
                                ..Default::default()
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::Switched,
                );
                Task::batch([note, switch])
            }
            _ => Task::none(),
        }
    }

    // ---- view ----------------------------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        if let Some(boot) = &self.boot {
            return self.boot_view(boot);
        }
        let editor = editor::editor(
            editor::Content {
                window: self.session.window.as_ref(),
                cursor: self.session.buffer.cursor,
                insert_mode: self.session.mode == Mode::Insert,
                awaiting_key: !matches!(self.session.pending, Pending::None) || self.session.count.is_some(),
                diff_view: self.session.diff_view,
                scroll_px: self.session.scroll_px,
                scroll_x_px: self.session.scroll_x_px,
                blame: self.session
                    .blame
                    .as_ref()
                    .map(|(line, text)| (*line, text.as_str())),
                tab_width: TAB_WIDTH,
            },
            Message::Editor,
        );
        let mut base = column![];
        base = base.push(Element::from(editor));
        base = base.push(self.status_bar());
        let mut layers: Vec<Element<'_, Message>> = vec![base.into()];
        if self.session.mode == Mode::Search {
            layers.push(self.search_bar());
        }
        if self.session.hover.is_some() {
            layers.push(self.hover_overlay());
        }
        if let Some(p) = &self.session.picker {
            layers.push(
                Element::from(crate::picker::overlay(p, &self.session.project_paths)).map(
                    |m| match m {
                        PickerMsg::Click(abs) => Message::PickerClicked(abs),
                        PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
                        PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
                        PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
                        PickerMsg::ChipClicked(i) => Message::PickerChipClicked(i),
                    },
                ),
            );
        }
        if self.session.prompt.is_some() {
            layers.push(self.prompt_overlay());
        }
        if !self.toasts.is_empty() {
            layers.push(self.toast_overlay());
        }
        // Last so its appearance never shifts an earlier layer's tree position (the picker
        // can be open when the connection drops).
        if self.session.conn != ConnState::Connected {
            layers.push(self.conn_banner());
        }
        // Always a stack — conditionally unwrapping the single-layer case would change the
        // tree shape when an overlay opens, resetting widget state (e.g. a scrollable's
        // offset) keyed by tree position.
        iced::widget::stack(layers).into()
    }

    /// Floating connection banner (the web's `#conn-banner`): a top-centred pill while the
    /// connection isn't healthy — yellow while the retry loop dials, red once
    /// re-establishing failed terminally.
    fn conn_banner(&self) -> Element<'_, Message> {
        let (label, bg, fg) = match self.session.conn {
            ConnState::Failed => ("Disconnected", theme::NORD11, theme::NORD6),
            _ => ("Reconnecting…", theme::NORD13, theme::NORD0),
        };
        let pill = container(text(label).size(12).font(SANS).color(fg))
            .padding([6, 14])
            .style(move |_| container::Style {
                background: Some(bg.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..iced::Border::default()
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba8(0, 0, 0, 0.35),
                    offset: iced::Vector::new(0.0, 4.0),
                    blur_radius: 16.0,
                },
                ..container::Style::default()
            });
        container(pill)
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .padding(iced::Padding {
                top: 12.0,
                ..iced::Padding::ZERO
            })
            .into()
    }

    /// The no-args start screen: just the Projects picker over the editor background.
    fn boot_view<'a>(&'a self, boot: &'a Boot) -> Element<'a, Message> {
        let backdrop = container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(theme::NORD0.into()),
                ..container::Style::default()
            });
        let picker = Element::from(crate::picker::overlay(&boot.picker, &[])).map(|m| match m {
            PickerMsg::Click(abs) => Message::PickerClicked(abs),
            PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
            PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
            PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
            PickerMsg::ChipClicked(i) => Message::PickerChipClicked(i),
        });
        let mut layers: Vec<Element<'_, Message>> = vec![backdrop.into(), picker];
        if !self.toasts.is_empty() {
            layers.push(self.toast_overlay());
        }
        iced::widget::stack(layers).into()
    }

    /// The floating search prompt, bottom-left above the status bar — mirrors the web client's
    /// `#searchbar` (query + beam cursor, match count on the right).
    fn search_bar(&self) -> Element<'_, Message> {
        let q = &self.session.search.query;
        let mut inner = row![].spacing(0).align_y(iced::Alignment::Center);
        if q.is_empty() {
            inner = inner.push(
                text("search").size(13).font(SANS).color(theme::NORD3),
            );
        } else {
            let pre = &q[..self.session.search.cursor];
            let post = &q[self.session.search.cursor..];
            if !pre.is_empty() {
                inner = inner.push(text(pre.to_string()).size(13).font(SANS).color(theme::NORD6));
            }
            inner = inner.push(
                container(iced::widget::Space::new().width(2).height(15)).style(|_| {
                    container::Style {
                        background: Some(theme::NORD8.into()),
                        ..container::Style::default()
                    }
                }),
            );
            if !post.is_empty() {
                inner =
                    inner.push(text(post.to_string()).size(13).font(SANS).color(theme::NORD6));
            }
        }
        let mut bar = row![inner, iced::widget::Space::new().width(Length::Fill)]
            .spacing(6)
            .width(Length::Fill)
            .align_y(iced::Alignment::Center);
        if let Some(count) = self.search_count_label() {
            bar = bar.push(text(count).size(13).font(SANS).color(theme::NORD4));
        }
        let prompt = container(bar)
            .width(420)
            .padding([5, 10])
            .style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                border: iced::Border {
                    color: theme::NORD3,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba8(0, 0, 0, 0.22),
                    offset: iced::Vector::new(0.0, 3.0),
                    blur_radius: 12.0,
                },
                ..container::Style::default()
            });
        container(prompt)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Left)
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: 32.0,
                left: 12.0,
            })
            .into()
    }

    /// The modal dialog, centred — web `modal.ts` styling (nord1 panel, message + buttons or
    /// the save-as path input). Buttons need `Clone` messages, so the content is built in
    /// [`PromptMsg`] space and mapped.
    fn prompt_overlay(&self) -> Element<'_, Message> {
        let prompt = self.session.prompt.as_ref().unwrap();
        let btn = |label: &str, primary: bool, msg: PromptMsg| {
            iced::widget::button(
                text(label.to_string())
                    .size(13)
                    .font(SANS)
                    .color(theme::NORD6),
            )
            .padding([5, 14])
            .style(move |_, _| iced::widget::button::Style {
                background: Some(if primary {
                    theme::NORD9.into()
                } else {
                    theme::NORD3.into()
                }),
                text_color: theme::NORD6,
                border: iced::Border {
                    radius: 4.0.into(),
                    ..iced::Border::default()
                },
                ..iced::widget::button::Style::default()
            })
            .on_press(msg)
        };
        let body: Element<'_, PromptMsg> = match prompt {
            Prompt::LspInfo(info) => {
                let busy = matches!(info.status, LspStatus::Ready) && !info.progress.is_empty();
                let dot = if busy {
                    theme::NORD13
                } else {
                    theme::lsp_status_color(&info.status)
                };
                let kv = |k: &str, v: String| {
                    row![
                        container(text(k.to_string()).size(13).font(SANS).color(theme::NORD3_BRIGHT))
                            .width(90),
                        text(v).size(13).font(SANS).color(theme::NORD6),
                    ]
                    .spacing(8)
                };
                let status_label = match &info.status {
                    LspStatus::Ready if busy => "busy".to_string(),
                    LspStatus::Ready => "ready".to_string(),
                    LspStatus::Starting => "starting".to_string(),
                    LspStatus::Initializing => "initializing".to_string(),
                    LspStatus::Restarting => "restarting".to_string(),
                    LspStatus::Crashed { code, message } => match code {
                        Some(c) => format!("crashed ({c}): {message}"),
                        None => format!("crashed: {message}"),
                    },
                    LspStatus::Stopped => "stopped".to_string(),
                };
                let mut col = column![
                    row![
                        text("● ").size(14).color(dot),
                        text(info.name.clone()).size(13).font(SANS_BOLD_UI).color(theme::NORD6),
                    ]
                    .align_y(iced::Alignment::Center),
                    kv("Language", info.language.clone()),
                    kv("Workspace", info.workspace_root.clone()),
                    kv("Status", status_label),
                ]
                .spacing(8);
                for p in &info.progress {
                    let mut line = p.title.clone();
                    if let Some(m) = &p.message {
                        line.push_str(&format!(" — {m}"));
                    }
                    if let Some(pct) = p.percentage {
                        line.push_str(&format!(" ({pct}%)"));
                    }
                    col = col.push(kv("Working", line));
                }
                col = col.push(
                    text("r — restart · Esc — close")
                        .size(12)
                        .font(SANS)
                        .color(theme::NORD3_BRIGHT),
                );
                col.spacing(10).into()
            }
            Prompt::Confirm { message, .. } => column![
                text(format!("{message}?")).size(13).font(SANS).color(theme::NORD6),
                row![
                    iced::widget::Space::new().width(Length::Fill),
                    btn("No", false, PromptMsg::Cancel),
                    btn("Yes", true, PromptMsg::Accept),
                ]
                .spacing(8),
            ]
            .spacing(14)
            .into(),
            Prompt::SaveAs {
                path_index, input, cursor, ..
            } => {
                let root = self.session
                    .project_paths
                    .get(*path_index as usize)
                    .map(|p| {
                        format!(
                            "{}/",
                            std::path::Path::new(p)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| p.clone())
                        )
                    })
                    .unwrap_or_default();
                let mut field = row![
                    text(root).size(13).font(SANS).color(theme::NORD3_BRIGHT),
                ]
                .align_y(iced::Alignment::Center);
                let pre = &input[..*cursor];
                let post = &input[*cursor..];
                if !pre.is_empty() {
                    field = field
                        .push(text(pre.to_string()).size(13).font(SANS).color(theme::NORD6));
                }
                field = field.push(
                    container(iced::widget::Space::new().width(2).height(15)).style(|_| {
                        container::Style {
                            background: Some(theme::NORD8.into()),
                            ..container::Style::default()
                        }
                    }),
                );
                if !post.is_empty() {
                    field = field
                        .push(text(post.to_string()).size(13).font(SANS).color(theme::NORD6));
                }
                column![
                    text("Save as").size(13).font(SANS).color(theme::NORD6),
                    container(field).padding([5, 8]).width(Length::Fill).style(|_| {
                        container::Style {
                            background: Some(theme::NORD0.into()),
                            border: iced::Border {
                                color: theme::NORD3,
                                width: 1.0,
                                radius: 4.0.into(),
                            },
                            ..container::Style::default()
                        }
                    }),
                    row![
                        iced::widget::Space::new().width(Length::Fill),
                        btn("Cancel", false, PromptMsg::Cancel),
                        btn("Save", true, PromptMsg::Accept),
                    ]
                    .spacing(8),
                ]
                .spacing(14)
                .into()
            }
        };
        let boxed = container(body)
            .width(420)
            .padding(16)
            .style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                border: iced::Border {
                    color: theme::NORD3,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba8(0, 0, 0, 0.45),
                    offset: iced::Vector::new(0.0, 12.0),
                    blur_radius: 40.0,
                },
                ..container::Style::default()
            });
        Element::from(
            container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Top)
                .padding(iced::Padding {
                    top: 120.0,
                    ..iced::Padding::ZERO
                })
                .style(|_| container::Style {
                    background: Some(iced::Color::from_rgba8(20, 24, 30, 0.5).into()),
                    ..container::Style::default()
                }),
        )
        .map(|m| match m {
            PromptMsg::Accept => Message::PromptAccept,
            PromptMsg::Cancel => Message::PromptCancel,
        })
    }

    /// The hover popover, anchored at the cursor cell: below it when there's room, above
    /// otherwise (estimated from the content's line count), clamped into the view.
    fn hover_overlay(&self) -> Element<'_, Message> {
        let content = self.session.hover.as_ref().unwrap();
        let mut est_lines = 0usize;
        let body: Element<'_, Message> = match content {
            HoverContent::Blocks(blocks) => {
                let mut col = column![].spacing(6);
                for b in blocks {
                    est_lines += b.text.lines().map(|l| 1 + l.len() / 90).sum::<usize>();
                    let color = b
                        .severity
                        .map(theme::diagnostic_color)
                        .unwrap_or(theme::NORD4);
                    col = col.push(text(b.text.clone()).size(13).color(color));
                }
                col.into()
            }
            HoverContent::Markdown { items, est_lines: n } => {
                est_lines = *n;
                // Links in hover docs aren't followable yet — clicks are swallowed.
                let mut settings = iced::widget::markdown::Settings::with_text_size(
                    13,
                    iced::widget::markdown::Style::from_palette(iced::Theme::Nord.palette()),
                );
                // Hover docs are a popover, not a document — headings step down gently
                // instead of doubling.
                settings.h1_size = iced::Pixels(16.0);
                settings.h2_size = iced::Pixels(15.0);
                settings.h3_size = iced::Pixels(14.0);
                settings.h4_size = iced::Pixels(13.0);
                settings.h5_size = iced::Pixels(13.0);
                settings.h6_size = iced::Pixels(13.0);
                settings.code_size = iced::Pixels(12.0);
                Element::from(iced::widget::markdown::view(items, settings))
                    .map(|_uri| Message::Noop)
            }
        };
        // Long content scrolls within the popover rather than growing past the view. The
        // padding lives inside the scrollable so its scrollbar sits against the popover edge.
        let boxed = container(
            iced::widget::scrollable(container(body).padding([8, 10])).direction(
                iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::new()
                        .width(5)
                        .margin(0)
                        .scroller_width(5),
                ),
            ),
        )
        .max_width(640)
        .max_height(380)
            .style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                border: iced::Border {
                    color: theme::NORD3,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            });

        // Anchor at the cursor cell. Below the line when the (estimated) height fits;
        // otherwise bottom-ALIGNED in a container ending just above the line, so the popover
        // sits flush against it regardless of how far off the height estimate was. The
        // top-left corner is the fallback when the cursor isn't in the loaded window.
        let mut anchor = None;
        if let (Some(cell), Some(window)) = (self.cell, &self.session.window) {
            if let Some((row, dcol, _)) =
                grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH)
            {
                let row_top = PAD + row as f32 * cell.height - self.session.scroll_px;
                let x = ((GUTTER_COLS + dcol) as f32 * cell.width)
                    .min((self.view_size.width - 360.0).max(8.0))
                    .max(4.0);
                let est_h = est_lines as f32 * 19.0 + 20.0;
                let below = row_top + cell.height + est_h + 8.0 <= self.view_size.height;
                anchor = Some((x, row_top, below, cell.height));
            }
        }
        match anchor {
            Some((x, row_top, true, cell_h)) => container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(iced::Padding {
                    top: (row_top + cell_h + 2.0).max(4.0),
                    right: 12.0,
                    bottom: 0.0,
                    left: x,
                })
                .into(),
            Some((x, row_top, false, _)) => container(
                // A box ending just above the cursor line; the popover hugs its bottom.
                container(boxed)
                    .width(Length::Fill)
                    .height((row_top - 2.0).max(40.0))
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(iced::Padding {
                        right: 12.0,
                        left: x,
                        ..iced::Padding::ZERO
                    }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(iced::alignment::Vertical::Top)
            .into(),
            None => container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(12)
                .into(),
        }
    }

    /// Prompt count label: "3/47", "3/10000+", bare total when the cursor isn't on a match,
    /// "no matches" — `None` while the query is empty.
    fn search_count_label(&self) -> Option<String> {
        if self.session.search.query.is_empty() {
            return None;
        }
        let summary = self.session.search.summary.as_ref()?;
        if summary.total == 0 {
            return Some("no matches".into());
        }
        let total = format_total(summary);
        Some(if summary.current_index == 0 {
            total
        } else {
            format!("{}/{total}", summary.current_index)
        })
    }

    /// Buffer-state accent colour, in the web client's precedence order: deleted-on-disk →
    /// changed-on-disk → unsaved edits → `None` when clean.
    fn buffer_state_color(&self) -> Option<iced::Color> {
        session_state_color(&self.session)
    }

    /// Cursor `line:col`, or the selection span in Normal mode (1-based) — the web client's
    /// `positionLabel`.
    fn position_label(&self) -> String {
        let p = self.session.buffer.cursor.position;
        let a = self.session.buffer.cursor.anchor;
        if self.session.mode == Mode::Insert || p == a {
            return format!("{}:{}", p.line + 1, p.col + 1);
        }
        let lo = min_pos(p, a);
        let hi = max_pos(p, a);
        if lo.line == hi.line {
            format!("{}:{}-{}", lo.line + 1, lo.col + 1, hi.col + 1)
        } else {
            format!(
                "{}:{}-{}:{}",
                lo.line + 1,
                lo.col + 1,
                hi.line + 1,
                hi.col + 1
            )
        }
    }

    /// The status bar mirrors the web client's: persistent state only (messages are toasts, the
    /// mode lives in the cursor shape). Left: state dot, `[project] file` (italic when
    /// transient), git cluster. Right: grep position, diagnostic counts, cursor position, LSP
    /// health dot.
    fn status_bar(&self) -> Element<'_, Message> {
        let t = |s: String, color: iced::Color| text(s).size(13).font(SANS).color(color);

        let mut left = row![];
        if let Some(color) = self.buffer_state_color() {
            left = left.push(t("● ".into(), color));
        }
        left = left.push(t(format!("[{}] ", self.session.project), theme::NORD4));
        // Segment-elide long labels to roughly half the bar so the filename survives (the
        // web's `truncatePath`; chars approximate px since the bar is sans).
        let budget = ((self.view_size.width * 0.5 / 6.5) as usize).max(12);
        let name = text(truncate_path_label(&self.session.buffer.label, budget))
            .size(13)
            .color(theme::NORD4)
            .font(
            // A transient (preview) buffer slants the file label, like the other clients.
                if self.session.buffer.transient { SANS_ITALIC } else { SANS },
            );
        left = left.push(name);
        // Git cluster: `⎇  branch  +u(s) ~u(s) -u(s)` — per-class counts combine unstaged with
        // the staged count in parens, each omitted when zero.
        if let Some(gs) = self.session.window.as_ref().and_then(|w| w.git_status.as_ref()) {
            if let Some(branch) = &gs.branch {
                left = left.push(t(format!("   ⎇  {branch}"), theme::NORD9));
            }
            let u = &gs.unstaged;
            let s = &gs.staged;
            for (sigil, color, un, st) in [
                ("+", theme::GIT_ADDED, u.added, s.added),
                ("~", theme::GIT_MODIFIED, u.modified, s.modified),
                ("-", theme::GIT_DELETED, u.deleted, s.deleted),
            ] {
                if un == 0 && st == 0 {
                    continue;
                }
                let mut tok = String::from(sigil);
                if un > 0 {
                    tok.push_str(&un.to_string());
                }
                if st > 0 {
                    tok.push_str(&format!("({st})"));
                }
                left = left.push(t(format!("  {tok}"), color));
            }
        }

        let mut right = row![].spacing(10);
        // Committed-search counter, only while the cursor sits on a match (web convention).
        if self.session.search.active {
            if let Some(s) = self.session.search.summary.as_ref() {
                if s.current_index > 0 && s.total > 0 {
                    right = right
                        .push(t(format!("{}/{}", s.current_index, format_total(s)), theme::NORD4));
                }
            }
        }
        if let Some(grep) = self.session.buffer.cursor.grep_position {
            right = right.push(t(format!("grep {}/{}", grep.current, grep.total), theme::NORD4));
        }
        // Diagnostic counts, as a tight cluster left of the position. Text glyphs stand in for
        // the web client's SVG icons (same forms as the TUI).
        if !self.session.diagnostics.is_empty() {
            let mut diag = row![].spacing(8);
            for (n, glyph, color) in [
                (self.session.diagnostics.errors, "✗", theme::NORD11),
                (self.session.diagnostics.warnings, "⚠", theme::NORD13),
                (self.session.diagnostics.infos, "ℹ", theme::NORD8),
                (self.session.diagnostics.hints, "·", theme::NORD8),
            ] {
                if n > 0 {
                    diag = diag.push(t(format!("{glyph} {n}"), color));
                }
            }
            right = right.push(diag);
        }
        right = right.push(t(self.position_label(), theme::NORD4));
        // LSP health dot: state-coloured; a ready server with in-flight progress shows busy.
        if let Some(lsp) = &self.session.lsp {
            let color = if matches!(lsp.status, LspStatus::Ready) && !lsp.progress.is_empty() {
                theme::NORD13
            } else {
                theme::lsp_status_color(&lsp.status)
            };
            right = right.push(t("•".into(), color));
        }

        container(
            row![
                left,
                iced::widget::Space::new().width(Length::Fill),
                right,
            ]
            .width(Length::Fill),
        )
        .padding([2, 8])
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(theme::NORD1.into()),
            text_color: Some(theme::NORD4),
            ..container::Style::default()
        })
        .into()
    }

    /// Bottom-right toast stack, above the status bar — layout and accent colours mirror the
    /// web client's `#toasts` (a `▌` glyph stands in for its 3px left border).
    fn toast_overlay(&self) -> Element<'_, Message> {
        let mut stack_col = column![].spacing(8).align_x(iced::Alignment::End);
        for toast in &self.toasts {
            let accent = match toast.kind {
                ToastKind::Info => theme::NORD8,
                ToastKind::Error => theme::NORD11,
                ToastKind::Warning => theme::NORD13,
                ToastKind::Success => theme::NORD14,
            };
            stack_col = stack_col.push(
                container(
                    row![
                        text("▌").size(13).color(accent),
                        text(toast.message.clone()).size(13).font(SANS).color(theme::NORD4),
                    ]
                    .spacing(6),
                )
                .padding([6, 12])
                .style(|_| container::Style {
                    background: Some(theme::NORD1.into()),
                    border: iced::Border {
                        color: theme::NORD3,
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    shadow: iced::Shadow {
                        color: iced::Color::from_rgba8(0, 0, 0, 0.4),
                        offset: iced::Vector::new(0.0, 4.0),
                        blur_radius: 16.0,
                    },
                    ..container::Style::default()
                }),
            );
        }
        container(stack_col)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Right)
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(iced::Padding {
                top: 0.0,
                right: 12.0,
                bottom: 36.0,
                left: 0.0,
            })
            .into()
    }
}

/// System sans-serif for GUI chrome (status bar, toasts) — the buffer keeps the app-default
/// monospace; mirrors the web client's `#status` font split.
const SANS: iced::Font = iced::Font {
    family: iced::font::Family::SansSerif,
    ..iced::Font::DEFAULT
};
const SANS_ITALIC: iced::Font = iced::Font {
    style: iced::font::Style::Italic,
    ..SANS
};
const SANS_BOLD_UI: iced::Font = iced::Font {
    weight: iced::font::Weight::Bold,
    ..SANS
};

fn pump(notifications: NotifRx) -> Task<Message> {
    Task::perform(
        async move { notifications.lock().await.recv().await },
        Message::Notified,
    )
}



fn loaded_rows(window: &Window) -> u32 {
    window.lines.iter().map(grid::line_rows).sum()
}

/// Build the client-side buffer record from a `buffer/open` result.
pub fn buffer_info(open: BufferOpenResult, roots: &[String]) -> BufferInfo {
    let label = match (&open.path, open.scratch_number) {
        (Some(path), _) => strip_longest_root(path, roots)
            .map(|(_, rel)| rel)
            .unwrap_or_else(|| path.clone()),
        (None, Some(n)) => format!("(scratch {n})"),
        (None, None) => "(scratch)".into(),
    };
    BufferInfo {
        buffer_id: open.buffer_id,
        label,
        path: open.path,
        language: open.language,
        revision: open.revision,
        saved_revision: open.saved_revision,
        cursor: open.cursor,
        scroll: open.scroll,
        transient: open.transient,
        lsp_server: open.lsp_server,
    }
}

/// Scroll the picker's results list so the highlighted row is in view. `Minimal` moves the
/// least distance; `Top` aligns the row to the top unless it's already fully visible.
fn reveal_picker_selection(p: &mut PickerState, reveal: Reveal) -> Task<Message> {
    let Some(sd) = p.selected_display_row() else {
        return Task::none();
    };
    let top = sd as f32 * crate::picker::ROW_H;
    let bottom = top + crate::picker::ROW_H;
    let h = p.list_height();
    let visible = top >= p.scroll_y && bottom <= p.scroll_y + h;
    let y = match reveal {
        Reveal::Top if !visible => top,
        Reveal::Top => return Task::none(),
        Reveal::Minimal if top < p.scroll_y => top,
        Reveal::Minimal if bottom > p.scroll_y + h => bottom - h,
        Reveal::Minimal => return Task::none(),
    };
    p.scroll_y = y;
    iced::widget::operation::scroll_to(
        crate::picker::list_id(),
        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
    )
}

/// Translate the Explorer's filter set for a Grep/Files switch. The dir scope is the browsed
/// directory; changed-only copies as-is. For Grep the ignored/hidden visibility *inverts*:
/// the explorer's listing shows ignored/hidden entries unless hidden (`hide_*`), grep's walk
/// excludes them unless included (`include_*`) — flipping the polarity means the search sees
/// exactly what the listing showed. Files takes only dir + changed-only.
fn seeded_filters_for_switch(
    explorer: &PickerFilters,
    dir_scope: Option<ScopedPath>,
    target: PickerKind,
) -> PickerFilters {
    let mut seeded = PickerFilters::default();
    if let Some(d) = dir_scope {
        seeded.directories.push(d);
    }
    seeded.changed_only = explorer.changed_only;
    if target == PickerKind::Grep {
        seeded.include_ignored = !explorer.hide_ignored;
        seeded.include_hidden = !explorer.hide_hidden;
    }
    seeded
}

/// Find the project root that contains `abs` (longest match wins, for nested roots) and return
/// `(path_index, relative_path)`.
pub fn strip_longest_root(abs: &str, roots: &[String]) -> Option<(u32, String)> {
    let abs_path = std::path::Path::new(abs);
    roots
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            abs_path
                .strip_prefix(root)
                .ok()
                .map(|rel| (i as u32, root.len(), rel.to_string_lossy().into_owned()))
        })
        .max_by_key(|(_, len, _)| *len)
        .map(|(i, _, rel)| (i, rel))
}

/// Segment-elide a path to `budget` chars, dropping leading directories first so the filename
/// survives; a still-too-long filename tail-truncates.
pub(crate) fn truncate_path_label(label: &str, budget: usize) -> String {
    if label.chars().count() <= budget {
        return label.to_string();
    }
    let mut parts: Vec<&str> = label.split('/').collect();
    while parts.len() > 1 {
        parts.remove(0);
        let cand = format!("…/{}", parts.join("/"));
        if cand.chars().count() <= budget {
            return cand;
        }
    }
    let last = parts.last().copied().unwrap_or(label);
    let tail: String = {
        let chars: Vec<char> = last.chars().collect();
        let keep = budget.saturating_sub(1).min(chars.len());
        chars[chars.len() - keep..].iter().collect()
    };
    format!("…{tail}")
}

/// `3w ago`-style age from a unix timestamp (seconds).
fn time_ago(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let s = (now - ts).max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        86_400..=604_799 => format!("{}d ago", s / 86_400),
        604_800..=2_591_999 => format!("{}w ago", s / 604_800),
        2_592_000..=31_535_999 => format!("{}mo ago", s / 2_592_000),
        _ => format!("{}y ago", s / 31_536_000),
    }
}

/// Buffer-state dot colour for the session, shown in the status bar.
fn session_state_color(s: &Session) -> Option<iced::Color> {
    if s.externally_deleted {
        return Some(theme::NORD11);
    }
    if s.externally_modified {
        return Some(theme::NORD12);
    }
    if s.buffer.revision != s.buffer.saved_revision {
        return Some(theme::NORD9);
    }
    None
}

fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "Error",
        DiagnosticSeverity::Warning => "Warning",
        DiagnosticSeverity::Information => "Info",
        DiagnosticSeverity::Hint => "Hint",
    }
}

/// `"47"` or `"10000+"` when the server hit its match cap.
fn format_total(s: &SearchSummary) -> String {
    if s.truncated {
        format!("{}+", s.total)
    } else {
        s.total.to_string()
    }
}

/// Escape regex metacharacters so a literal string can be the search term — mirrors the TUI's
/// `regex_escape`.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn min_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) <= (b.line, b.col) {
        a
    } else {
        b
    }
}

fn max_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) >= (b.line, b.col) {
        a
    } else {
        b
    }
}

fn nord_theme(_app: &App) -> iced::Theme {
    iced::Theme::Nord
}

/// Run the iced application. Called by `main` once the connection and buffer are bootstrapped.
pub fn run(bootstrap: Bootstrap) -> iced::Result {
    iced::application(
        move || App::new(bootstrap.clone()),
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    // Everything we draw sets explicit Nord colours, but theme-inheriting surfaces (markdown
    // hover body text, scrollbars) must not default to the Light theme.
    .theme(nord_theme)
    // The buffer's font + size (chrome sets explicit fonts/sizes): web's 14px monospace.
    .settings(iced::Settings {
        default_font: iced::Font::MONOSPACE,
        default_text_size: iced::Pixels(14.0),
        antialiasing: true,
        ..iced::Settings::default()
    })
    .window_size(Size::new(1100.0, 750.0))
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the TUI's seeded_filters_for_switch tests: the explorer's visibility filters
    // invert for Grep (its walk excludes what the listing shows), and Files takes only
    // dir + changed-only.
    #[test]
    fn explorer_switch_translates_filters() {
        let scope = ScopedPath {
            path_index: 0,
            relative_path: "src".into(),
        };
        let defaults = PickerFilters::default();
        let seeded =
            seeded_filters_for_switch(&defaults, Some(scope.clone()), PickerKind::Grep);
        assert!(seeded.include_ignored && seeded.include_hidden);
        assert_eq!(seeded.directories, vec![scope.clone()]);

        let hiding = PickerFilters {
            hide_ignored: true,
            changed_only: true,
            ..PickerFilters::default()
        };
        let seeded = seeded_filters_for_switch(&hiding, Some(scope.clone()), PickerKind::Grep);
        assert!(!seeded.include_ignored && seeded.include_hidden && seeded.changed_only);

        let seeded = seeded_filters_for_switch(&hiding, Some(scope), PickerKind::Files);
        assert!(!seeded.include_ignored && !seeded.include_hidden && seeded.changed_only);

        // Roots mode: no dir scope — the target covers the whole project.
        let seeded = seeded_filters_for_switch(&defaults, None, PickerKind::Grep);
        assert!(seeded.directories.is_empty());
    }
}
