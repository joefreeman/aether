//! Application state and message loop.
//!
//! Mirrors the TUI's `app.rs` in miniature, restructured for iced's architecture: key events
//! resolve through `keymap` to `Action`s, actions become RPC `Task`s, and responses /
//! server notifications come back as `Message`s that update state. The scroll model is the web
//! client's: a pixel offset into the full document height, with window fetches when the view
//! nears the loaded range's edge.

use crate::connection::Handle;
use crate::connection::NotifRx;
pub use crate::core::effect::{Effect, Effects, RevealStyle, ShellAction, ToastKind};
use crate::core::markdown::{Block as MdBlock, Inline as MdInline};
pub use crate::core::session::*;
use crate::core::update::Event as CoreEvent;
use crate::editor::{self, ClickKind, EditorEvent, GUTTER_COLS, PAD};
use crate::grid;
use crate::keymap::{
    hover_action, HoverAction, KeyCode, Mods, ScrollDir, ScrollUnit, ViewportPlace,
    CURSOR_REST_FRACTION,
};
use crate::picker::{PickerMsg, PickerState, Reveal, FETCH_LIMIT};
use crate::theme;
use aether_protocol::buffer::{BufferOpen, BufferOpenParams, BufferOpenResult};
use aether_protocol::cursor::Granularity;
use aether_protocol::envelope::{NotificationMethod, RpcMethod};
use aether_protocol::git::{GitBlameLine, GitBlameLineParams};
use aether_protocol::lsp::LspStatus;
use aether_protocol::picker::{
    PickerItem, PickerKind, PickerQuery, PickerQueryParams, PickerUpdate, PickerUpdateParams,
    PickerView, PickerViewParams,
};
use aether_protocol::workspace::{
    WorkspaceActivate, WorkspaceActivateParams, WorkspaceCreate, WorkspaceCreateParams, WorkspaceInfo,
    WorkspaceOpenPath, WorkspaceOpenPathParams,
};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{
    ScrollPosition, ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams,
    ViewportScrollToRow, ViewportScrollToRowParams, ViewportSetWrap, ViewportSetWrapParams,
    ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult, ViewportWindowResult,
    Window, WrapMode,
};
use iced::widget::{column, container, row, text};
use iced::{keyboard, Element, Event, Length, Size, Subscription, Task};

const TAB_WIDTH: u32 = 4;

/// What `main` resolves before iced starts. With a workspace on the CLI, a live connection and
/// an opened buffer ([`SessionBootstrap`]); without one, just the connection — the app opens
/// the workspace picker and builds the session over it when the user picks ([`ChooseBootstrap`]).
#[derive(Clone)]
pub enum Bootstrap {
    /// No connection yet: the app launches immediately into an immersive "Connecting…" backdrop
    /// and dials the daemon from within (a client can start before the server). Carries the CLI
    /// args the connect task needs to bootstrap once the socket lands.
    Connecting(ConnectingBootstrap),
    Session(Box<SessionBootstrap>),
    Choose(ChooseBootstrap),
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Carries non-Debug transport handles; the variant name is all a log needs.
        let name = match self {
            Bootstrap::Connecting(_) => "Connecting",
            Bootstrap::Session(_) => "Session",
            Bootstrap::Choose(_) => "Choose",
        };
        f.debug_tuple(name).finish()
    }
}

/// The CLI args a boot-connect task needs: which workspace/file to open once connected, and the
/// client version for the handshake. No live connection — that's what the task establishes.
#[derive(Clone)]
pub struct ConnectingBootstrap {
    pub workspace: Option<String>,
    pub file: Option<String>,
    pub client_version: String,
    /// The (profile-resolved) WebSocket address every dial and reconnect targets.
    pub server_url: String,
}

/// The live connection and opened buffer for the window's session.
#[derive(Clone)]
pub struct SessionBootstrap {
    pub handle: Handle,
    pub notifications: NotifRx,
    pub client_version: String,
    pub server_url: String,
    /// The daemon's start stamp, learned from the `workspace/activate` result — reconnects compare
    /// it to tell "same daemon, connection blipped" from "daemon restarted" (where unsaved buffer
    /// state died with it).
    pub server_started_at: u64,
    pub workspace: String,
    pub workspace_paths: Vec<String>,
    pub buffer: BufferInfo,
    /// Set when the CLI path was a directory: the absolute dir to open the file explorer at,
    /// over the transient scratch in `buffer`. `None` for the file / no-path cases.
    pub explorer_dir: Option<String>,
    /// The session was launched directly onto a file outside any workspace (ephemeral context) —
    /// closing it should quit rather than drop to the chooser (see `Session::launched_with_file`).
    pub launched_with_file: bool,
}

/// A bare connection for the no-args start: the workspace picker browses on it, and the picked
/// workspace's session is built over it.
#[derive(Clone)]
pub struct ChooseBootstrap {
    pub handle: Handle,
    pub notifications: NotifRx,
    pub client_version: String,
    pub server_url: String,
    pub server_started_at: u64,
}

/// Everything a successful reconnect hands back to rebuild the session.
pub struct Reestablished {
    pub handle: Handle,
    pub notifications: NotifRx,
    /// The restored workspace + landing buffer, or `None` when the workspace is gone — renamed or
    /// removed by another client while we were disconnected. The socket is fine, so the shell
    /// recovers into the boot chooser rather than failing.
    pub restore: Option<(WorkspaceInfo, BufferOpenResult)>,
    pub server_url: String,
    pub server_started_at: u64,
}

impl std::fmt::Debug for Reestablished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reestablished").finish_non_exhaustive()
    }
}

/// Why a reconnect attempt didn't produce a session.
#[derive(Debug)]
pub enum ReconnectError {
    /// No daemon reachable (dial failed) — retry, silently.
    NotUp,
    /// A server answered but re-establishing failed — terminal.
    Fatal(String),
}

/// Pre-session state: the workspace chooser shown on a no-args start. Owns the connection the
/// session will be built over; all input routes through `update_boot` while this is set.
struct Boot {
    handle: Handle,
    notifications: NotifRx,
    picker: PickerState,
    /// Byte caret into `picker.query`. The boot chooser predates the session, so it drives its own
    /// keycode editing (fake-caret rendering) rather than the core's value-synced query — hence the
    /// caret lives here, not on the (now caret-free) `PickerState`.
    query_cursor: usize,
    /// A workspace was picked and its activation is in flight — input is parked meanwhile.
    opening: bool,
    /// The connection died; a retry loop is dialling. Input is parked until it lands.
    down: bool,
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

/// The prompt buttons' message space (buttons need `Clone`, the app `Message` isn't).
#[derive(Debug, Clone, Copy)]
enum PromptMsg {
    Accept,
    Cancel,
}

/// The workspace-settings overlay's clickable-affordance message space (buttons need `Clone`, the
/// app `Message` isn't). Mirrors [`PickerMsg`]: the overlay renders in this space, then `.map`s to
/// `Message::Core`. Today only the per-root delete button.
#[derive(Debug, Clone, Copy)]
enum WorkspaceSettingsMsg {
    /// The delete button on root row `index` (0-based) was clicked.
    RemoveRoot(usize),
}

/// Which overlay text field an [`Message::OverlayInput`] targets. Each maps to a core `*_set_*`
/// method; the shell renders that field as a controlled `text_input` whose `on_input` carries one
/// of these (web parity — the browser client syncs native `<input>` values the same way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayField {
    /// The picker query input.
    PickerQuery,
    /// The search-bar query input.
    Search,
    /// The save-as prompt's path input.
    SaveAs,
    /// The save-as prompt's root-filter input (multi-root workspaces).
    SaveAsRoot,
    /// The open-from-path prompt's single path input.
    OpenPath,
    /// The workspace-settings name field.
    WorkspaceName,
    /// The workspace-settings add-root input.
    WorkspaceAddRoot,
    /// The chip editor's root-filter input (multi-root dir editor).
    ChipRoot,
    /// The chip editor's path/glob input.
    ChipPath,
}

impl OverlayField {
    /// The stable widget id for this field's `text_input`, for `.id()` + `operation::focus`.
    fn id(self) -> iced::advanced::widget::Id {
        iced::advanced::widget::Id::new(match self {
            OverlayField::PickerQuery => "overlay-picker-query",
            OverlayField::Search => "overlay-search",
            OverlayField::SaveAs => "overlay-saveas",
            OverlayField::SaveAsRoot => "overlay-saveas-root",
            OverlayField::OpenPath => "overlay-openpath",
            OverlayField::WorkspaceName => "overlay-workspace-name",
            OverlayField::WorkspaceAddRoot => "overlay-workspace-addroot",
            OverlayField::ChipRoot => "overlay-chip-root",
            OverlayField::ChipPath => "overlay-chip-path",
        })
    }
}

/// The hover popover's body: plain severity-coloured blocks (diagnostics, commit details) or
/// rendered markdown (LSP hover). The *content* comes from the core ([`HoverText`]); the
/// parsed widget items are this shell's cache of it.
enum HoverContent {
    Blocks(Vec<HoverBlock>),
    Markdown {
        /// The shared hover AST (parsed in the core), rendered by `md_doc`.
        blocks: Vec<MdBlock>,
        /// Estimated wrapped-row count, for the place-above-or-below decision.
        est_lines: usize,
    },
}

impl HoverContent {
    /// The whole popover as plain text, for "copy popover content" (`Ctrl-y`) — iced's `rich_text`
    /// can't be drag-selected, so this is the copy affordance. Diagnostic/commit blocks join by
    /// blank lines; Markdown flattens via the shared AST serializer.
    fn to_plain_text(&self) -> String {
        match self {
            HoverContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            HoverContent::Markdown { blocks, .. } => crate::core::markdown::to_plain(blocks),
        }
    }
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

#[derive(Debug)]
struct Toast {
    id: u64,
    message: String,
    kind: ToastKind,
    /// Replacement key (see [`aether_client::effect::Effect::Toast`]). A new grouped toast evicts
    /// any existing toast sharing this key instead of stacking. `None` toasts always stack.
    group: Option<String>,
}

#[derive(Debug)]
pub enum Message {
    /// The boot chooser's pick resolved: the activated workspace, the buffer to land on, and the
    /// server instance's start stamp (for restart detection once the session exists).
    SessionReady(Result<Box<(WorkspaceInfo, BufferOpenResult, u64)>, String>),
    /// The boot-connect dial resolved (from the `Connecting` launch state): either a connected
    /// `Session`/`Choose` bootstrap to install, or a failure to retry.
    Booted(Result<Bootstrap, String>),
    Editor(EditorEvent),
    Key {
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    },
    /// A controlled overlay `text_input` produced new text — sync the full value into the core
    /// via the matching `*_set_*` method (web parity). Carries the field and its new value.
    OverlayInput(OverlayField, String),
    ToastExpired(u64),
    /// Fire-and-forget RPC completed (e.g. `search/clear`); result ignored.
    /// An RPC outcome for a core-issued `Effect::Request` (the token routes it back to
    /// the parked mapping in the session).
    RpcResult(u64, Result<serde_json::Value, crate::connection::RpcError>),
    /// A Markdown link in the hover popover was clicked — open it in the OS handler.
    OpenLink(String),
    Noop,
    /// Frame tick while a smooth scroll is in flight.
    AnimTick(std::time::Instant),
    Subscribed(Result<ViewportSubscribeResult, String>),
    WindowUpdate(Result<ViewportWindowResult, String>),

    /// A core event (docs/client-core.md): forwarded to `Session::on_event`, whose effects
    /// the shell executes. Grows a subsystem at a time as update logic migrates into core.
    Core(CoreEvent),
    /// The picker's results list scrolled natively (absolute y in px).
    PickerScrolled(f32),
    /// Pointer entered (`Some(abs)`) or left (`None`-if-still-current, see mapping) a row.
    PickerHovered(Option<u32>),
    PickerUnhovered(u32),
    Notified(Option<aether_protocol::envelope::Notification>),
    /// A reconnect attempt resolved (the backoff sleep rides inside the attempt task).
    Reconnected(Result<Box<Reestablished>, ReconnectError>),
    /// The boot chooser's reconnect attempt resolved.
    BootReconnected(Result<BootConn, String>),
}

pub struct App {
    /// The workspace chooser (no-args start). While set, `session` is an inert placeholder and
    /// all messages route through `update_boot`; picking a workspace builds the real session
    /// over the boot connection and clears this.
    boot: Option<Boot>,
    /// Set while the app is in the boot-connecting state (`ConnState::Connecting`): the CLI args
    /// the dial task needs, retained so a failed attempt can retry. Cleared the moment a
    /// connection lands and the real session/chooser is installed. While `Some`, input is parked
    /// and the immersive "Connecting…" backdrop shows.
    boot_args: Option<ConnectingBootstrap>,
    /// The window's one editing context (one connection — the server's client).
    session: Session,
    /// The session's transport — shell-owned (native sockets don't exist on every shell;
    /// the core receives the handle per call rather than storing it).
    handle: Handle,
    notifications: NotifRx,
    client_version: String,
    /// The (profile-resolved) WebSocket address every boot dial and reconnect dials.
    server_url: String,
    /// The connected daemon instance's start stamp (see [`TabBootstrap::server_started_at`]).
    server_started_at: u64,
    cell: Option<Size>,
    view_size: Size,
    // Per-session presentation state (geometry + parsed artifacts) — deliberately NOT on
    // `core` Session (docs/client-core.md: semantics in the core, geometry in the shell).
    scroll_px: f32,
    /// Horizontal scroll in px (`wrap: none` only; soft wrap always fits the viewport).
    scroll_x_px: f32,
    scroll_anim: Option<ScrollAnim>,
    /// The search prompt's Esc-restore scroll position (`SaveScrollAnchor` effect).
    scroll_anchor: Option<f32>,
    // Viewport/fetch geometry — shell-owned (the core reasons about `window`/`viewport_id`, never
    // these). Grid last sent, the scroll a subscribe asked for, and the fetch-coordination flags.
    sent_grid: Option<(u32, u32)>,
    subscribe_scroll: ScrollPosition,
    fetch_in_flight: bool,
    refetch_queued: bool,
    /// Set when a cursor move scrolled out of the loaded window: once the fetch lands, reveal the
    /// cursor with this style (`Follow` = minimal, `Jump` = rest near the top).
    reveal_after_fetch: Option<RevealStyle>,
    /// Like `reveal_after_fetch`, but places the cursor at a fixed fraction down once its
    /// (out-of-window) line lands — for `;` / `Alt-;` when the line was scrolled out of the window.
    place_after_fetch: Option<ViewportPlace>,
    /// The picker results list's scroll offset in px (boot chooser or session picker —
    /// never both). The core tracks rows, not pixels; resets arrive as
    /// `Effect::PickerScrollReset`.
    picker_scroll_y: f32,
    /// The picker search throbber's rotation (radians), advanced from frame ticks while a search is
    /// in progress, with the time of the last tick so the step is frame-rate independent.
    spinner_phase: f32,
    last_anim_tick: Option<std::time::Instant>,
    /// The hover popover (hover info / diagnostics-at-cursor / commit details), anchored at
    /// the cursor; holds *parsed* iced markdown. Dismissed by any key, click, or scroll.
    hover: Option<HoverContent>,
    /// The keyboard-shortcuts help dialog (`Space ?`), or `None` when closed. A shell-local overlay
    /// (the core only triggers it via `Effect::ShellAction(OpenHelp)`); content comes from the
    /// core keymap (`keymap::help_entries`). Holds the selected tab index; scroll lives in the
    /// scrollable widget, keyed by `help_scroll_id`.
    help: Option<usize>,
    /// Last horizontal anchor (px) computed for the hover popover, cached so it's retained when the
    /// cursor scrolls out of the loaded window (otherwise its column is unknown and the popover
    /// would jump to the left edge). Interior-mutable: refreshed from the render path (`&self`).
    hover_anchor_x: std::cell::Cell<f32>,
    /// Popover orientation (`Some(below)`), decided the first frame a hover is shown and retained
    /// so it doesn't flip sides as the buffer scrolls (it slides with the line and clamps to an
    /// edge instead). Reset to `None` when a new hover opens. Interior-mutable (render path).
    hover_below: std::cell::Cell<Option<bool>>,

    // Transient messages are toasts; the status bar shows persistent state only (web client
    // convention).
    toasts: Vec<Toast>,
    next_toast: u64,
    /// The overlay `text_input` that currently *should* hold focus (mirrors the web's
    /// `focusTarget`). Recomputed after every update; when it changes, the shell issues an
    /// `operation::focus` so typing lands in the right field the moment an overlay opens (and
    /// moves between the workspace-settings name/add inputs as the core's selection changes).
    focused_field: Option<OverlayField>,
}

impl App {
    pub fn new(b: Bootstrap) -> (Self, Task<Message>) {
        let shell = |boot: Option<Boot>,
                     session: Session,
                     handle: Handle,
                     notifications: NotifRx,
                     client_version: String,
                     server_url: String,
                     server_started_at: u64| App {
            boot,
            boot_args: None,
            session,
            handle,
            notifications,
            client_version,
            server_url,
            server_started_at,
            cell: None,
            view_size: Size::ZERO,
            scroll_px: 0.0,
            scroll_x_px: 0.0,
            scroll_anim: None,
            scroll_anchor: None,
            sent_grid: None,
            subscribe_scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            fetch_in_flight: false,
            refetch_queued: false,
            reveal_after_fetch: None,
            place_after_fetch: None,
            picker_scroll_y: 0.0,
            spinner_phase: 0.0,
            last_anim_tick: None,
            hover: None,
            help: None,
            hover_anchor_x: std::cell::Cell::new(4.0),
            hover_below: std::cell::Cell::new(None),
            toasts: Vec::new(),
            next_toast: 0,
            focused_field: None,
        };
        match b {
            // Launch immediately, connectionless: a placeholder session flagged `Connecting`
            // (the view renders an empty backdrop + the "Connecting…" banner) plus dummy transport
            // that's never used while input is parked. The returned task dials and bootstraps; its
            // `Booted` result installs the real session/chooser. No pump yet — the real
            // notification stream arrives with the connection.
            Bootstrap::Connecting(args) => {
                let mut session = Session::placeholder();
                session.conn = ConnState::Connecting;
                let mut app = shell(
                    None,
                    session,
                    crate::connection::dummy_handle(),
                    crate::connection::dummy_notifications(),
                    args.client_version.clone(),
                    args.server_url.clone(),
                    0,
                );
                app.boot_args = Some(args.clone());
                (app, spawn_connect(args))
            }
            Bootstrap::Session(b) => {
                let pump = pump(b.notifications.clone());
                let mut session = Session::new(b.workspace, b.workspace_paths, b.buffer);
                session.launched_with_file = b.launched_with_file;
                // Fetch persisted app settings (e.g. the soft-wrap default) as the session comes up.
                let startup = session.startup();
                let mut app = shell(
                    None,
                    session,
                    b.handle,
                    b.notifications,
                    b.client_version,
                    b.server_url,
                    b.server_started_at,
                );
                let startup_task = app.run_core(startup);
                (app, Task::batch([pump, startup_task]))
            }
            Bootstrap::Choose(b) => {
                // Open the Workspaces picker on the boot connection; the session is built over
                // that same connection when the user picks a workspace (`SessionReady`). Until
                // then `session` is an inert placeholder — `update_boot` owns every message.
                let pump = pump(b.notifications.clone());
                let handle = b.handle.clone();
                let view = Task::perform(
                    async move {
                        handle
                            .rpc::<PickerView>(PickerViewParams {
                                from_selection: false,
                                kind: PickerKind::Workspaces,
                                reset: true,
                                offset: 0,
                                limit: FETCH_LIMIT,
                                center_on: None,
                                center_on_cursor: None,
                                directory_path: None,
                                explorer_roots: false,
                                buffer_id: None,
                                filters: None,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    |result| {
                        Message::Core(CoreEvent::PickerViewed {
                            initial: true,
                            result,
                        })
                    },
                );
                let boot = Boot {
                    handle: b.handle.clone(),
                    notifications: b.notifications.clone(),
                    picker: PickerState::new(PickerKind::Workspaces),
                    query_cursor: 0,
                    opening: false,
                    down: false,
                };
                (
                    shell(
                        Some(boot),
                        Session::placeholder(),
                        b.handle,
                        b.notifications,
                        b.client_version,
                        b.server_url,
                        b.server_started_at,
                    ),
                    Task::batch([pump, view]),
                )
            }
        }
    }

    /// `[workspace] file` — mirrors the web client's page title and the TUI's terminal title.
    pub fn title(&self) -> String {
        crate::labels::window_title(&self.session.workspace, &self.session.buffer.label)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let keys = iced::event::listen_with(|event, status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                modifiers,
                text,
                ..
            }) => {
                // Overlay `text_input`s capture editing keys (typing, Backspace/Delete, arrows,
                // Home/End, clipboard) and report them `Captured`; those must NOT also reach the
                // core's key handler or it would double-handle them. So forward a key to `on_key`
                // only when no focused widget consumed it (`Ignored`) — global bindings, plus the
                // non-editing keys (Enter, Tab, Up/Down, Alt/Ctrl chords) that `text_input` leaves
                // alone. One exception: `Escape`. A focused `text_input` *captures* Escape (it
                // unfocuses itself, publishing nothing), which would otherwise swallow every
                // overlay's Esc-to-close. Forward it regardless so the core still gets it; the
                // input vanishes with the overlay anyway.
                let mods = crate::input::mods(modifiers);
                // macOS composes Option(Alt)-chords into glyphs (Option-f → `ƒ`); resolve Alt
                // bindings against the unmodified base key so they still match. See
                // `input::keycode_for_binding`.
                let code = crate::input::keycode_for_binding(&key, &modified_key, mods.alt)?;
                // Forward to the core when no focused widget consumed the key (`Ignored`), PLUS two
                // forced exceptions a focused `text_input` would otherwise swallow:
                //   - `Escape` (the input captures it to unfocus itself), and
                //   - any `Alt`-chord — `Alt-j/k/l` is the app's universal navigation idiom (move
                //     between picker results / settings fields); `text_input` reports it Captured,
                //     so force it through. (The `alt_filter::alt_passthrough` wrapper around each
                //     overlay input also drops the `Alt` press before the input can insert it as
                //     text, which some platforms' winit delivers — so the field stays clean.)
                let forward =
                    status == iced::event::Status::Ignored || code == KeyCode::Esc || mods.alt;
                forward.then(|| Message::Key {
                    code,
                    mods,
                    text: text.map(|t| t.to_string()),
                })
            }
            _ => None,
        });
        // Frame ticks drive the scroll easing and the picker's search throbber; subscribe to them
        // only while one of those is actually animating — and never while disconnected, where a
        // picker throbber stuck mid-search (the server stopped answering) would otherwise pin the
        // 60fps redraw loop for the whole reconnect window.
        let animating = self.scroll_anim.is_some() || self.picker_ticking();
        if self.boot.is_none() && animating && self.session.conn == ConnState::Connected {
            Subscription::batch([keys, iced::window::frames().map(Message::AnimTick)])
        } else {
            keys
        }
    }

    /// Whether a picker search is still streaming (drives the throbber animation).
    fn picker_ticking(&self) -> bool {
        self.session.picker.as_ref().is_some_and(|p| p.ticking)
    }

    // ---- update ---------------------------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        // Boot-connecting (no socket yet): input is parked; only the dial result moves us on.
        // Then the workspace chooser (if any) owns every message until `SessionReady` hands off.
        let task = if self.boot_args.is_some() {
            self.update_connecting(message)
        } else if self.boot.is_some() {
            self.update_boot(message)
        } else {
            self.update_inner(message)
        };
        // After every update, snap focus to the overlay field that should own the keyboard (web
        // parity: `ensureFocus`). Only fires a focus operation when the target *changes*, so it
        // doesn't fight the user (e.g. re-grab focus every keystroke).
        Task::batch([task, self.sync_focus()])
    }

    /// The overlay `text_input` that should hold focus right now, given session state. Mirrors the
    /// web's `focusTarget`. The boot chooser drives the workspace picker through `update_boot` with
    /// its own [`Boot::picker`] (no `text_input` — its query stays on the fake-caret path), so it
    /// has no focus target here. `None` means "no overlay field" (the editor owns the keyboard).
    fn desired_focus(&self) -> Option<OverlayField> {
        if self.boot.is_some() {
            return None;
        }
        // A confirm / LSP-info prompt has no text field; only the save-as prompt does. Its two
        // segments (root filter / path) are controlled `text_input`s with ghost overlays behind
        // them, exactly like the chip editor — focus the active one so its caret shows and plain
        // typing flows through `on_input`. The root segment only exists in multi-root workspaces.
        match &self.session.prompt {
            Some(Prompt::SaveAs(ed)) => {
                let multi_root = self.session.workspace_paths.len() > 1;
                return Some(
                    if multi_root && ed.field == crate::chips::ChipEditorField::Root {
                        OverlayField::SaveAsRoot
                    } else {
                        OverlayField::SaveAs
                    },
                );
            }
            Some(Prompt::OpenPath(_)) => return Some(OverlayField::OpenPath),
            Some(_) => return None,
            None => {}
        }
        if let Some(p) = &self.session.picker {
            // The chip editor (glob/dir filter line) is a controlled `text_input` per segment,
            // with a ghost-suggestion overlay behind it (web parity). Focus the *active*
            // segment's input so the caret shows and plain typing flows through `on_input` →
            // the core's `chip_editor_set_*`; Tab/Enter/Esc/arrows stay uncaptured and Alt is
            // dropped by `alt_passthrough`, so the bespoke chip-editor key routing still reaches
            // the core.
            if let Some(ed) = &p.chip_editor {
                return Some(if ed.field == crate::chips::ChipEditorField::Root {
                    OverlayField::ChipRoot
                } else {
                    OverlayField::ChipPath
                });
            }
            // No focus target when a filter chip is selected — chip navigation
            // (Left/Right/Backspace/Enter/Esc) must reach the core, but a focused `text_input`
            // would capture the editing keys among them. Defocusing lets every key bubble (web
            // parity: "chip selected → forward all").
            return p
                .chip_selected
                .is_none()
                .then_some(OverlayField::PickerQuery);
        }
        if let Some(s) = &self.session.workspace_settings {
            // Name field (selection 0) or add-root input (last row) — a highlighted root row in
            // between has no text field, so nothing to focus there.
            if s.on_name() {
                return Some(OverlayField::WorkspaceName);
            }
            if s.on_input() {
                return Some(OverlayField::WorkspaceAddRoot);
            }
            return None;
        }
        if self.session.mode == Mode::Search {
            // No focus target when an option chip is selected — its row keys (Left/Right/
            // Backspace/Enter/Esc) must reach the core, but a focused `text_input` would capture
            // the editing keys among them. Defocusing lets every key bubble (picker parity).
            return self
                .session
                .search
                .chip_selected
                .is_none()
                .then_some(OverlayField::Search);
        }
        None
    }

    // (see `core_key_message` free fn below for the chip-boundary key forwarder)

    /// Move keyboard focus to [`Self::desired_focus`] when it changed since the last update.
    fn sync_focus(&mut self) -> Task<Message> {
        let want = self.desired_focus();
        if want == self.focused_field {
            return Task::none();
        }
        self.focused_field = want;
        match want {
            Some(field) => iced::widget::operation::focus(field.id()),
            // The focus left every overlay field — e.g. a filter chip just got selected, so the
            // query input must stop owning the keyboard. `focus(None)` is not a thing; actively
            // *unfocus* the previously-focused widget, otherwise it keeps focus (and its caret).
            // (We only reach here when `want` changed, so something was focused before.)
            None => iced::advanced::widget::operate(
                iced::advanced::widget::operation::focusable::unfocus(),
            ),
        }
    }

    /// Message handling while the workspace chooser is up: a reduced picker vocabulary (type to
    /// filter, Alt-j/k, Enter/click to pick, Esc quits), plus the `SessionReady` hand-off that
    /// builds the session over the boot connection.
    fn update_boot(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Key { code, mods, text } => self.on_boot_key(code, mods, text),
            Message::Core(CoreEvent::PickerViewed { initial, result }) => {
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
                    Err(e) => self.error(format!("Workspace list failed: {e}")),
                }
            }
            Message::Notified(Some(n)) => {
                if let Some(boot) = &mut self.boot {
                    if n.method == PickerUpdate::NAME {
                        if let Ok(u) = serde_json::from_value::<PickerUpdateParams>(n.params) {
                            if boot.picker.apply_update(u) {
                                tracing::debug!(
                                    workspaces = boot.picker.total_matches,
                                    "workspace chooser updated"
                                );
                            }
                        }
                    }
                    return pump(boot.notifications.clone());
                }
                Task::none()
            }
            // The boot connection died under the chooser — dial the fixed address again until a
            // daemon is back (a restarted daemon rebinds the same port).
            Message::Notified(None) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                if boot.down {
                    return Task::none();
                }
                boot.down = true;
                let note = self.toast(
                    "Server disconnected — reconnecting…",
                    ToastKind::Warning,
                    Some("connection".into()),
                );
                Task::batch([note, self.boot_reconnect()])
            }
            Message::BootReconnected(Ok(c)) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                boot.handle = c.handle.clone();
                boot.notifications = c.notifications.clone();
                boot.picker = PickerState::new(PickerKind::Workspaces);
                self.picker_scroll_y = 0.0;
                boot.opening = false;
                boot.down = false;
                self.server_started_at = c.server_started_at;
                let handle = c.handle;
                let view = Task::perform(
                    async move {
                        handle
                            .rpc::<PickerView>(PickerViewParams {
                                from_selection: false,
                                kind: PickerKind::Workspaces,
                                reset: true,
                                offset: 0,
                                limit: FETCH_LIMIT,
                                center_on: None,
                                center_on_cursor: None,
                                directory_path: None,
                                explorer_roots: false,
                                buffer_id: None,
                                filters: None,
                            })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    |result| {
                        Message::Core(CoreEvent::PickerViewed {
                            initial: true,
                            result,
                        })
                    },
                );
                let note = self.toast("Reconnected", ToastKind::Success, Some("connection".into()));
                Task::batch([pump(c.notifications), view, note])
            }
            Message::BootReconnected(Err(_)) => self.boot_reconnect(),
            Message::SessionReady(Ok(r)) => {
                // The pick resolved: the boot connection becomes the session's. The running
                // pump carries on — same notification channel, now read by the main handler.
                let Some(boot) = self.boot.take() else {
                    return Task::none();
                };
                let (workspace, open, server_started_at) = *r;
                tracing::info!(workspace = %workspace.name, "session established");
                // First activation on the boot connection establishes the restart-detection
                // baseline (it was 0/unknown while only the chooser was up).
                self.server_started_at = server_started_at;
                // A rootless workspace (just created from the chooser) has nowhere to open files —
                // land in settings on the add-root input so the user can give it a root.
                let rootless = workspace.paths.is_empty();
                let buffer = buffer_info(open, &workspace.paths);
                self.handle = boot.handle;
                self.notifications = boot.notifications;
                self.session = Session::new(workspace.name, workspace.paths, buffer);
                if rootless {
                    self.session.open_workspace_settings();
                    if let Some(s) = self.session.workspace_settings.as_mut() {
                        s.selected = s.input_index();
                    }
                }
                // The editor's first Layout event subscribes the viewport (cell metrics are
                // only published once it renders). Fetch the persisted app settings now.
                let startup = self.session.startup();
                self.run_core(startup)
            }
            Message::SessionReady(Err(e)) => {
                if let Some(boot) = &mut self.boot {
                    boot.opening = false;
                }
                self.error(format!("Open failed: {e}"))
            }
            Message::Core(CoreEvent::PickerClicked(abs)) => {
                if let Some(boot) = &mut self.boot {
                    boot.picker.selected = abs;
                }
                self.boot_accept()
            }
            Message::PickerScrolled(y) => {
                let Some(boot) = &mut self.boot else {
                    return Task::none();
                };
                self.picker_scroll_y = y;
                match boot
                    .picker
                    .scrolled_refetch(crate::picker::first_visible_row(y))
                {
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

    /// Keys while the workspace chooser is up.
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
                if let Some((i, _)) = p.query[..boot.query_cursor].char_indices().last() {
                    p.query.remove(i);
                    boot.query_cursor = i;
                    return self.boot_query_changed();
                }
                return Task::none();
            }
            KeyCode::Left if no_chord => {
                if let Some((i, _)) = boot.picker.query[..boot.query_cursor].char_indices().last() {
                    boot.query_cursor = i;
                }
                return Task::none();
            }
            KeyCode::Right if no_chord => {
                if let Some(c) = boot.picker.query[boot.query_cursor..].chars().next() {
                    boot.query_cursor += c.len_utf8();
                }
                return Task::none();
            }
            _ => {}
        }
        if no_chord {
            if let Some(t) = text {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    let at = boot.query_cursor;
                    boot.picker.query.insert_str(at, &t);
                    boot.query_cursor = at + t.len();
                    return self.boot_query_changed();
                }
            }
        }
        Task::none()
    }

    /// Enter / click in the chooser: activate the picked workspace over the boot connection
    /// and open its last buffer (or a fresh transient scratch) — the bootstrap convention.
    fn boot_accept(&mut self) -> Task<Message> {
        let Some(boot) = &self.boot else {
            return Task::none();
        };
        let handle = boot.handle.clone();
        // The synthetic "+ Create workspace …" row: create the workspace named by the query, then land
        // in it (the session picker reaches this via the core; the boot chooser predates a session,
        // so it drives the RPCs itself).
        if boot.picker.selected_is_create() {
            let name = boot.picker.query.trim().to_string();
            if name.is_empty() || name.contains('/') || name.contains('\\') {
                return self.error("Workspace name can't be empty or contain path separators".into());
            }
            if let Some(b) = &mut self.boot {
                b.opening = true;
            }
            return Task::perform(
                async move {
                    let created = handle
                        .rpc::<WorkspaceCreate>(WorkspaceCreateParams { name })
                        .await
                        .map_err(|e| e.to_string())?;
                    // A fresh workspace has no roots, so `workspace/create` returns no landing buffer —
                    // open a scratch so the session lands in *some* editor.
                    let open = match created.opened {
                        Some(open) => open,
                        None => handle
                            .rpc::<BufferOpen>(BufferOpenParams::default())
                            .await
                            .map_err(|e| e.to_string())?,
                    };
                    Ok(Box::new((created.workspace, open, created.server_started_at)))
                },
                Message::SessionReady,
            );
        }
        let Some(PickerItem::Workspace { name, .. }) = boot.picker.selected_item() else {
            return Task::none();
        };
        let name = name.clone();
        if let Some(b) = &mut self.boot {
            b.opening = true;
        }
        Task::perform(
            async move {
                // One composite: activate + land on the workspace's MRU buffer (or a fresh
                // transient scratch on first visit).
                let activated = handle
                    .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
                        name,
                        open_last: true,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let open = activated
                    .opened
                    .ok_or_else(|| "workspace/activate returned no landing buffer".to_string())?;
                Ok(Box::new((
                    activated.workspace,
                    open,
                    activated.server_started_at,
                )))
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
        let (query, generation) = (p.query.clone(), p.generation);
        self.picker_scroll_y = 0.0;
        let q = self.boot_rpc::<PickerQuery>(
            PickerQueryParams {
                kind: PickerKind::Workspaces,
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
                from_selection: false,
                kind: PickerKind::Workspaces,
                reset: false,
                offset,
                limit: FETCH_LIMIT,
                center_on: None,
                center_on_cursor: None,
                directory_path: None,
                explorer_roots: false,
                buffer_id: None,
                filters: None,
            },
            |result| {
                Message::Core(CoreEvent::PickerViewed {
                    initial: false,
                    result,
                })
            },
        )
    }

    fn boot_move(&mut self, delta: i64) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        match boot.picker.move_selection(delta) {
            Some(offset) => self.boot_refetch(offset),
            None => {
                reveal_picker_selection(&boot.picker, &mut self.picker_scroll_y, Reveal::Minimal)
            }
        }
    }

    /// One paced boot-reconnect attempt: sleep, dial the fixed address. Failures loop back
    /// through [`Message::BootReconnected`] — indefinitely, like the session's retry. No workspace
    /// is active on the boot connection, so there's no instance stamp to learn yet (0); the first
    /// `workspace/activate` establishes the baseline (nothing is open to lose in the meantime).
    fn boot_reconnect(&self) -> Task<Message> {
        let version = self.client_version.clone();
        let server_url = self.server_url.clone();
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let (handle, rx) = crate::connection::connect(&server_url, &version)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(BootConn {
                    handle,
                    notifications: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
                    server_started_at: 0,
                })
            },
            Message::BootReconnected,
        )
    }

    /// Recovery when a reconnect succeeds but the old workspace is gone (renamed/removed while away):
    /// re-enter the boot chooser over the fresh connection and fetch the workspace list, mirroring a
    /// no-args start. Picking a workspace (the renamed one shows under its new name) builds the
    /// session the usual way.
    /// Open the Workspaces chooser over a fresh connection: install the boot state and start the
    /// pump + the chooser's first `picker/view`. Shared by the no-args boot (`Booted` → `Choose`),
    /// the boot-connection reconnect, and [`Self::reconnect_to_chooser`] (workspace-gone recovery).
    fn enter_boot_chooser(&mut self, handle: Handle, notifications: NotifRx) -> Task<Message> {
        let view = self.raise_boot_chooser(handle, notifications.clone());
        Task::batch([pump(notifications), view])
    }

    /// Install the boot-chooser state and fire its first `picker/view`, WITHOUT starting a
    /// notification pump — for when a pump is already running on this connection. Used by
    /// [`Self::enter_boot_chooser`] (which adds the pump for a fresh connection) and directly by
    /// the [`Effect::ToChooser`] handler, which drops to the chooser mid-session (the live pump
    /// keeps delivering, now routed through `update_boot`).
    fn raise_boot_chooser(&mut self, handle: Handle, notifications: NotifRx) -> Task<Message> {
        self.boot = Some(Boot {
            handle: handle.clone(),
            notifications,
            picker: PickerState::new(PickerKind::Workspaces),
            query_cursor: 0,
            opening: false,
            down: false,
        });
        Task::perform(
            async move {
                handle
                    .rpc::<PickerView>(PickerViewParams {
                        from_selection: false,
                        kind: PickerKind::Workspaces,
                        reset: true,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: None,
                        center_on_cursor: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        filters: None,
                    })
                    .await
                    .map_err(|e| e.to_string())
            },
            |result| {
                Message::Core(CoreEvent::PickerViewed {
                    initial: true,
                    result,
                })
            },
        )
    }

    fn reconnect_to_chooser(&mut self, handle: Handle, notifications: NotifRx) -> Task<Message> {
        let chooser = self.enter_boot_chooser(handle, notifications);
        let toast = self.toast(
            "Workspace no longer exists — pick another",
            ToastKind::Warning,
            None,
        );
        Task::batch([chooser, toast])
    }

    /// Boot-connecting state (`ConnState::Connecting`): the editor chrome is live but there's no
    /// socket yet. The dial's `Booted` result installs the real session (workspace on the CLI) or
    /// chooser (no workspace), or retries after a short delay (the daemon may still be starting).
    /// Everything else flows to the normal handler so client-side keys behave as in a reconnect —
    /// the core drops any RPC while not `Connected`, so the dummy transport is never exercised.
    fn update_connecting(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Booted(Ok(Bootstrap::Session(b))) => {
                self.boot_args = None;
                self.server_started_at = b.server_started_at;
                self.handle = b.handle;
                self.notifications = b.notifications.clone();
                self.session = Session::new(b.workspace, b.workspace_paths, b.buffer);
                self.session.launched_with_file = b.launched_with_file;
                // The connecting editor already laid out (recording cell metrics) without
                // subscribing, so its Layout may not fire again — subscribe explicitly now that
                // we're Connected. `subscribe_task` is a no-op if no metrics arrived yet, and the
                // first real Layout then handles it.
                self.sent_grid = self.current_grid();
                // A directory CLI arg opens the file explorer over the scratch buffer.
                let startup = match b.explorer_dir {
                    Some(dir) => {
                        self.session
                            .open_picker(PickerKind::Explorer, Some(dir), None, false)
                    }
                    None => Effects::none(),
                };
                // Fetch the persisted app settings (e.g. the soft-wrap default) on this connection.
                let startup = startup.and(self.session.startup());
                Task::batch([
                    pump(b.notifications),
                    self.subscribe_task(),
                    self.run_core(startup),
                ])
            }
            Message::Booted(Ok(Bootstrap::Choose(b))) => {
                self.boot_args = None;
                self.server_started_at = b.server_started_at;
                self.handle = b.handle.clone();
                self.notifications = b.notifications.clone();
                self.enter_boot_chooser(b.handle, b.notifications)
            }
            // The dial only ever yields Session/Choose; Connecting can't come back.
            Message::Booted(Ok(Bootstrap::Connecting(_))) => Task::none(),
            Message::Booted(Err(e)) => {
                tracing::debug!(error = %e, "boot connect failed; retrying");
                match &self.boot_args {
                    Some(args) => spawn_connect_delayed(args.clone()),
                    None => Task::none(),
                }
            }
            // Client-side input runs against the placeholder session (RPCs dropped while
            // Connecting), giving the reconnect-style "some keys work" feel.
            other => self.update_inner(other),
        }
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            // Boot-only message that slipped past a finished boot — nothing to do.
            Message::SessionReady(_) => Task::none(),
            Message::Editor(ev) => self.on_editor_event(ev),
            Message::Key { code, mods, text } => self.on_key(code, mods, text),

            Message::Subscribed(Ok(res)) => {
                tracing::debug!(
                    viewport_id = res.viewport_id,
                    lines = res.window.lines.len(),
                    total_visual_rows = res.window.total_visual_rows,
                    "viewport subscribed"
                );
                // Position the view at the scroll the subscribe asked for (restored or
                // cursor-centred), now the window geometry is known, then make sure the cursor
                // is on-screen (it may sit below a restored scroll after a `jump_to` open).
                let scroll = self.subscribe_scroll;
                self.session.adopt_subscribe(res);
                if let (Some(cell), Some(w)) = (self.cell, self.session.window.as_ref()) {
                    if let Some(rel) = grid::rows_before_line(w, scroll.logical_line) {
                        let row = w.first_visual_row + rel;
                        self.scroll_px = (row as f32 + scroll.sub_row) * cell.height;
                    }
                }
                self.clamp_scroll();
                self.reveal_cursor();
                // Diff view rides the subscribe params, so there's nothing to re-apply here.
                Task::none()
            }
            Message::Subscribed(Err(e)) => self.error(format!("Subscribe failed: {e}")),

            Message::WindowUpdate(Ok(res)) => {
                self.fetch_in_flight = false;
                self.session.adopt_window(res);
                // A wrap toggle left a content anchor pending: restore the view to it (same content
                // on screen across the reflow), suppressing the reveal/center this fetch would do.
                let anchored = if let Some(px) = self.resolve_anchor_px() {
                    self.scroll_px = px;
                    true
                } else {
                    false
                };
                self.clamp_scroll();
                let mut task = Task::none();
                if anchored {
                    self.reveal_after_fetch = None;
                    self.place_after_fetch = None;
                } else {
                    if let Some(style) = self.reveal_after_fetch.take() {
                        self.reveal_cursor_styled(style);
                    }
                    if let Some(place) = self.place_after_fetch.take() {
                        self.place_cursor_in_window(place);
                    }
                }
                if self.refetch_queued {
                    self.refetch_queued = false;
                    task = self.maybe_fetch();
                }
                task
            }
            Message::WindowUpdate(Err(e)) => {
                self.fetch_in_flight = false;
                self.refetch_queued = false;
                self.error(format!("Viewport update failed: {e}"))
            }

            Message::Core(ev) => {
                let fx = self.session.on_event(ev);
                self.run_core(fx)
            }

            // A controlled overlay `text_input` produced new text — sync the whole value into the
            // core via the matching `*_set_*` method and run its effects (web parity). The core
            // owns cursor/validity/suggestion state; the widget owns text editing.
            Message::OverlayInput(field, value) => {
                let fx = self.overlay_set(field, value);
                self.run_core(fx)
            }

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
                self.picker_scroll_y = y;
                match p.scrolled_refetch(crate::picker::first_visible_row(y)) {
                    Some(offset) => {
                        let fx = self.session.picker_refetch(offset);
                        self.run_core(fx)
                    }
                    None => Task::none(),
                }
            }

            Message::ToastExpired(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            Message::RpcResult(token, result) => {
                let fx = self.session.on_rpc_result(token, result);
                self.run_core(fx)
            }
            Message::OpenLink(url) => {
                open_link(&url);
                Task::none()
            }
            Message::Noop => Task::none(),

            Message::AnimTick(now) => {
                // Advance the picker throbber by elapsed time (clamped so a gap between animation
                // bursts doesn't jump it); ~1 rotation/sec. Processing the tick re-renders the view.
                if self.picker_ticking() {
                    let dt = self
                        .last_anim_tick
                        .map_or(0.0, |t| (now - t).as_secs_f32().min(0.1));
                    self.spinner_phase =
                        (self.spinner_phase + dt * std::f32::consts::TAU) % std::f32::consts::TAU;
                }
                self.last_anim_tick = Some(now);
                // Scroll easing (independent of the throbber).
                let Some(anim) = &self.scroll_anim else {
                    return Task::none();
                };
                let t = ((now - anim.started).as_secs_f32() * 1000.0 / SCROLL_ANIM_MS).min(1.0);
                let eased = 1.0 - (1.0 - t).powi(3); // cubic ease-out
                self.scroll_px = anim.from + (anim.to - anim.from) * eased;
                if t >= 1.0 {
                    self.scroll_anim = None;
                }
                self.clamp_scroll();
                self.maybe_fetch()
            }

            Message::Notified(Some(n)) => {
                let fx = self.session.on_event(CoreEvent::ServerPush(n));
                Task::batch([self.run_core(fx), pump(self.notifications.clone())])
            }
            Message::Notified(None) => {
                let fx = self.session.on_event(CoreEvent::ConnectionLost);
                self.run_core(fx)
            }

            // The transport swap is the shell's half of a reconnect (the new socket and
            // daemon identity live here); the session adoption is the core's.
            Message::Reconnected(Ok(r)) => {
                let restarted = r.server_started_at != self.server_started_at;
                tracing::info!(restarted, url = %r.server_url, "transport re-established");
                self.server_started_at = r.server_started_at;
                self.handle = r.handle.clone();
                self.notifications = r.notifications.clone();
                match r.restore {
                    Some((workspace, open)) => {
                        let fx = self.session.on_event(CoreEvent::Reestablished {
                            workspace,
                            open,
                            restarted,
                        });
                        Task::batch([pump(r.notifications), self.run_core(fx)])
                    }
                    // The workspace is gone — re-enter the boot chooser over the fresh connection.
                    None => self.reconnect_to_chooser(r.handle, r.notifications),
                }
            }
            Message::Reconnected(Err(ReconnectError::NotUp)) => {
                let fx = self.session.on_event(CoreEvent::ReconnectRetry);
                self.run_core(fx)
            }
            Message::Reconnected(Err(ReconnectError::Fatal(e))) => {
                let fx = self.session.on_event(CoreEvent::ReconnectFatal(e));
                self.run_core(fx)
            }
            // Boot-only messages that slipped past a finished boot — nothing to do. `Booted` is
            // handled in `update_connecting`; once a session exists it's a stale dial result.
            Message::BootReconnected(_) | Message::Booted(_) => Task::none(),
        }
    }

    fn toast(
        &mut self,
        message: impl Into<String>,
        kind: ToastKind,
        group: Option<String>,
    ) -> Task<Message> {
        let message = message.into();
        match &group {
            // A grouped toast replaces any existing toast with the same key, so an evolving status
            // (LSP restart → ready, the diff toggle) updates one toast in place.
            Some(g) => self.toasts.retain(|t| t.group.as_deref() != Some(g.as_str())),
            // Ungrouped: drop a repeat of the last message (incremental search re-reports "Invalid
            // regex" on every keystroke).
            None => {
                if self.toasts.last().is_some_and(|t| t.message == message) {
                    return Task::none();
                }
            }
        }
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast {
            id,
            message,
            kind,
            group,
        });
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(3600)).await;
                id
            },
            Message::ToastExpired,
        )
    }

    fn error(&mut self, message: String) -> Task<Message> {
        self.toast(message, ToastKind::Error, None)
    }

    /// Execute a batch of core effects: futures spawn onto iced's executor with their events
    /// routed back through the bridge; presentation effects run against shell state.
    fn run_core(&mut self, fx: Effects) -> Task<Message> {
        let mut tasks = Vec::new();
        for e in fx.0 {
            match e {
                Effect::Toast {
                    message,
                    kind,
                    group,
                } => tasks.push(self.toast(message, kind, group)),
                Effect::WriteClipboard(text) => tasks.push(iced::clipboard::write(text)),
                Effect::RevealCursor(style) => tasks.push(self.ensure_cursor_visible(style)),
                Effect::Resubscribe => {
                    self.scroll_px = 0.0;
                    self.scroll_x_px = 0.0;
                    self.scroll_anim = None;
                    self.hover = None;
                    // Reconnects zero the grid (new viewport identity); re-derive it from
                    // the current metrics so subscribe_task has something to send.
                    if self.sent_grid.is_none() {
                        self.sent_grid = self.current_grid();
                    }
                    tasks.push(self.subscribe_task());
                }
                Effect::SaveScrollAnchor => self.scroll_anchor = Some(self.scroll_px),
                Effect::SaveContentAnchor => {
                    if let Some(cell) = self.cell {
                        let top_row = (self.scroll_px / cell.height).round().max(0.0) as u32;
                        self.session
                            .capture_scroll_anchor(top_row, self.visible_rows());
                    }
                }
                Effect::ShowHover(content) => {
                    self.hover_below.set(None); // re-pick orientation for this fresh hover
                    self.hover = Some(match content {
                        crate::core::session::HoverText::Blocks(blocks) => {
                            HoverContent::Blocks(blocks)
                        }
                        crate::core::session::HoverText::Markdown(blocks) => {
                            let est_lines = md_estimate(&blocks).max(1);
                            HoverContent::Markdown { blocks, est_lines }
                        }
                    });
                }
                Effect::DismissHover => self.hover = None,
                Effect::WindowAdopted => {
                    // Diff toggle re-layout: restore the view to the pending content anchor (same
                    // content on screen) if there is one; otherwise clamp + reveal as before.
                    if let Some(px) = self.resolve_anchor_px() {
                        self.scroll_px = px;
                        self.clamp_scroll();
                    } else {
                        self.clamp_scroll();
                        self.reveal_cursor();
                    }
                }
                Effect::Request {
                    token,
                    method,
                    params,
                } => {
                    // Enqueue NOW (Handle::call sends synchronously) so requests hit the
                    // wire in effect-emission order; only the response ride is async.
                    let fut = self.handle.call(method, params);
                    tasks.push(Task::perform(fut, move |r| Message::RpcResult(token, r)));
                }
                Effect::RevealPickerSelection(reveal) => {
                    tasks.push(self.picker_reveal_selected_with(reveal));
                    // The reveal's `scroll_to` drops the query input's focus, and `sync_focus`
                    // won't restore it (the desired field is unchanged, so its change-guard skips).
                    // Re-assert it so the cursor stays — e.g. opening the Explorer in a subdirectory
                    // centres on the active file, which reveals; the workspace-root case finds no match
                    // and never reveals, which is why only the subdir case lost its cursor. Reveals
                    // fire on open-centring and nav, never on typing, so this can't fight an edit.
                    if let Some(field) = self.desired_focus() {
                        tasks.push(iced::widget::operation::focus(field.id()));
                    }
                }
                Effect::PickerScrollReset => {
                    self.picker_scroll_y = 0.0;
                    tasks.push(iced::widget::operation::scroll_to(
                        crate::picker::list_id(),
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    ));
                }
                Effect::Reconnect { attempt } => tasks.push(self.try_reconnect(attempt)),
                Effect::Exit => tasks.push(iced::exit()),
                Effect::ToChooser => {
                    // Drop to the workspace chooser over the live connection (no new pump — the
                    // current one keeps delivering, now routed through `update_boot`).
                    let (handle, notifications) =
                        (self.handle.clone(), self.notifications.clone());
                    tasks.push(self.raise_boot_chooser(handle, notifications));
                }
                Effect::ReadClipboard(kind) => tasks.push(self.read_clipboard(kind)),
                Effect::ShellAction(action) => tasks.push(self.run_shell_action(action)),
                Effect::RestoreScrollAnchor => {
                    if let Some(px) = self.scroll_anchor.take() {
                        self.scroll_to_px(px, false);
                    }
                }
            }
        }
        Task::batch(tasks)
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
                match self.session.viewport_id {
                    None => {
                        if self.sent_grid.is_some() {
                            return Task::none(); // subscribe in flight
                        }
                        self.sent_grid = Some((cols, rows));
                        self.subscribe_task()
                    }
                    Some(viewport_id) if self.sent_grid != Some((cols, rows)) => {
                        self.sent_grid = Some((cols, rows));
                        self.rpc::<ViewportResize>(
                            ViewportResizeParams {
                                viewport_id,
                                cols,
                                rows,
                            },
                            Message::WindowUpdate,
                        )
                    }
                    Some(_) => Task::none(),
                }
            }
            EditorEvent::Wheel {
                delta_px,
                delta_x_px,
            } => {
                // The hover popover stays open while wheel-scrolling the buffer behind it —
                // `hover_overlay` re-anchors it to its line (clamped to the window) each frame.
                // With a picker open, its scrollable owns wheel input over the list; wheel
                // over the backdrop shouldn't scroll the editor behind it either.
                if self.session.picker.is_some() {
                    return Task::none();
                }
                self.scroll_by(delta_px);
                self.scroll_x_by(delta_x_px);
                self.maybe_fetch()
            }
            EditorEvent::ScrollTo { offset_px } => {
                self.hover = None;
                // Dragging the thumb snaps directly to the offset (no easing) and may pull in a
                // not-yet-loaded window.
                self.scroll_to_px(offset_px, false);
                self.maybe_fetch()
            }
            EditorEvent::Pressed {
                row,
                dcol,
                kind,
                shift,
            } => {
                self.hover = None;
                // A click outside the dialog/picker cancels it (the web's backdrop-click
                // behaviour); the click doesn't also move the cursor.
                if self.session.prompt.is_some() {
                    self.session.decline_prompt();
                    return Task::none();
                }
                if self.session.picker.is_some() {
                    let fx = self.session.close_picker();
                    return self.run_core(fx);
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
                // Selection semantics (drag anchor, click-streak granularity, and the
                // selection-in-Insert → Normal switch) live in the core, shared by every shell.
                let fx = self.session.pointer_press(pos, granularity, shift);
                self.run_core(fx)
            }
            EditorEvent::Dragged { row, dcol } => {
                let Some(window) = &self.session.window else {
                    return Task::none();
                };
                let Some(pos) = grid::hit_test(window, row, dcol, TAB_WIDTH) else {
                    return Task::none();
                };
                let fx = self.session.pointer_drag(pos);
                self.run_core(fx)
            }
            EditorEvent::Released => {
                self.session.pointer_release();
                Task::none()
            }
        }
    }

    // ---- keyboard --------------------------------------------------------------------------

    /// Key events: the shell's edge — dismiss the hover popover (its parse cache lives
    /// here), then hand the key to the core with the viewport height it may need.
    fn on_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        // The help dialog owns the keyboard while open: tab nav, scrolling, close — every key is
        // consumed so nothing leaks to the editor behind it.
        if self.help.is_some() {
            return self.on_help_key(code, mods);
        }
        // While a hover popover is open, scroll keys pan it (and keep it open); any other key
        // dismisses it — Esc is then consumed, everything else still acts.
        if self.hover.is_some() {
            // The popover reuses the editor's own Copy / Scroll bindings (`keymap::hover_action`), so
            // its chords never drift from the keymap. Copy / scroll keep it open; any other key
            // dismisses it (Esc is then consumed).
            match hover_action(code, mods) {
                // rich_text can't be drag-selected, so copy-all is the affordance; toast mirrors the
                // normal copy.
                Some(HoverAction::Copy) => {
                    let text = self.hover.as_ref().unwrap().to_plain_text();
                    let note = self.toast("Copied popover", ToastKind::Success, None);
                    return Task::batch([iced::clipboard::write(text), note]);
                }
                Some(HoverAction::Scroll { dir, unit }) => {
                    return iced::widget::operation::scroll_by(
                        hover_scroll_id(),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: 0.0,
                            y: hover_scroll_px(dir, unit, self.cell),
                        },
                    );
                }
                None => {}
            }
            self.hover = None;
            if code == KeyCode::Esc {
                return Task::none();
            }
        }
        // Snapshot the chip editor's active-field text before the core sees the key. The chip
        // inputs are controlled `text_input`s, so when the core rewrites the text in response to a
        // key (Tab-complete, suggestion cycle, switching root↔path) iced leaves the widget's own
        // caret where it was — mid-string. Detect that out-of-band change and jump the caret to the
        // end. Scoped to the key path: plain typing flows through `OverlayInput`, so this never
        // fights a click-to-position-then-type.
        let chip_before = self.chip_field_snapshot();
        let chips_before = self.picker_chip_count();
        // The picker query is a controlled `text_input` too: a command key can rewrite it
        // out-of-band (Tab-complete extends it, Alt-Backspace clears it), and iced would leave the
        // caret mid-string. Snapshot it for the same caret-to-end treatment. (Plain typing flows
        // through `OverlayInput`, not here, so this never fights click-to-position-then-type.)
        let query_before = self.session.picker.as_ref().map(|p| p.query.clone());
        let visible_rows = self.visible_rows();
        // Report the on-screen line range so sneak scopes labels to what's visible (the core owns no
        // pixel scroll). `scroll_px / cell.height` is the absolute top visual row.
        if let Some(cell) = self.cell {
            let top_row = (self.scroll_px / cell.height).round().max(0.0) as u32;
            self.session.set_visible_lines(top_row, visible_rows);
        }
        let fx = self.session.on_key(code, mods, text, visible_rows);
        let mut task = self.run_core(fx);
        let chip_after = self.chip_field_snapshot();
        if let Some((field, _)) = &chip_after {
            // The active field or its text changed out-of-band (the core rewrote it) — snap the
            // controlled `text_input`'s caret to the end of the new value.
            if chip_after != chip_before {
                task = Task::batch([
                    task,
                    iced::widget::operation::move_cursor_to_end(field.id()),
                ]);
            }
        }
        // Same for the query input: only when the picker stayed open and its query changed under a
        // command key (not on open/close, where focus is handled elsewhere).
        let query_after = self.session.picker.as_ref().map(|p| p.query.clone());
        if query_before.is_some() && query_after.is_some() && query_after != query_before {
            task = Task::batch([
                task,
                iced::widget::operation::move_cursor_to_end(crate::picker::query_input_id()),
            ]);
        }
        // A filter chip was added or removed (an `Alt`-chord toggle, or deleting the last chip):
        // the chip-row children change under the overlay, and iced drops the focused `text_input`'s
        // focus when its siblings shift in the tree diff. `desired_focus` is unchanged (still the
        // query), so `sync_focus` won't restore it — re-assert it here so the input stays the
        // keyboard owner instead of leaking keys to the core's character path. (`focus()` snaps the
        // caret to the end, which is harmless for a chip toggle — not an in-query caret action.)
        if self.picker_chip_count() != chips_before {
            if let Some(field) = self.desired_focus() {
                task = Task::batch([task, iced::widget::operation::focus(field.id())]);
            }
        }
        task
    }

    /// The number of filter chips on the open picker, or `None` when no picker is open. A change in
    /// this count means the chip row restructured — see the focus re-assertion in `on_key`.
    fn picker_chip_count(&self) -> Option<usize> {
        self.session.picker.as_ref().map(|p| p.chips.len())
    }

    /// The active chip-editor / save-as field (the one with a focused `text_input`) and its current
    /// text, or `None` when neither is open. Used to spot core-driven text changes that need the
    /// `text_input` caret moved to the end (see `on_key`). The save-as prompt's root/path segments
    /// are the same controlled-input-over-ghost shape as the chip editor, so they get the same
    /// caret-to-end treatment when the core rewrites them (Tab-complete, cycle, root↔path switch).
    fn chip_field_snapshot(&self) -> Option<(OverlayField, String)> {
        if let Some(Prompt::SaveAs(ed)) = &self.session.prompt {
            let multi_root = self.session.workspace_paths.len() > 1;
            return Some(
                if multi_root && ed.field == crate::chips::ChipEditorField::Root {
                    (OverlayField::SaveAsRoot, ed.root_filter.text.clone())
                } else {
                    (OverlayField::SaveAs, ed.input.text.clone())
                },
            );
        }
        let ed = self.session.picker.as_ref()?.chip_editor.as_ref()?;
        Some(if ed.field == crate::chips::ChipEditorField::Root {
            (OverlayField::ChipRoot, ed.root_filter.text.clone())
        } else {
            (OverlayField::ChipPath, ed.input.text.clone())
        })
    }

    /// Write an overlay field's text into the core — the sink for the controlled `text_input`s'
    /// `on_input`.
    fn overlay_set(&mut self, field: OverlayField, value: String) -> Effects {
        match field {
            OverlayField::PickerQuery => self.session.picker_set_query(value),
            OverlayField::Search => self.session.search_set_query(value),
            OverlayField::SaveAs => self.session.save_as_set_input(value),
            OverlayField::SaveAsRoot => self.session.save_as_set_root_filter(value),
            OverlayField::OpenPath => self.session.open_path_set_input(value),
            OverlayField::WorkspaceName => self.session.workspace_settings_set_name(value),
            OverlayField::WorkspaceAddRoot => self.session.workspace_settings_set_add(value),
            OverlayField::ChipRoot => self.session.chip_editor_set_root_filter(value),
            OverlayField::ChipPath => self.session.chip_editor_set_input(value),
        }
    }

    /// Keyboard handling while the help dialog is open (mirrors the TUI / web help): Esc / `q` / `?`
    /// close it; `h`/`l`, arrows, Tab, or `1`-`4` switch tabs (resetting scroll); `j`/`k`, arrows,
    /// PageUp/Down and Space scroll the body. Every key is consumed.
    fn on_help_key(&mut self, code: KeyCode, mods: Mods) -> Task<Message> {
        let n = HELP_TABS.len();
        let tab = self.help.unwrap_or(0);
        let scroll_to_top = || {
            iced::widget::operation::scroll_to(
                help_scroll_id(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
            )
        };
        let scroll_by = |dy: f32| {
            iced::widget::operation::scroll_by(
                help_scroll_id(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: dy },
            )
        };
        const LINE: f32 = 40.0;
        const PAGE: f32 = 320.0;
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                self.help = None;
                Task::none()
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.help = Some((tab + n - 1) % n);
                scroll_to_top()
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.help = Some((tab + 1) % n);
                scroll_to_top()
            }
            KeyCode::Tab => {
                self.help = Some(if mods.shift {
                    (tab + n - 1) % n
                } else {
                    (tab + 1) % n
                });
                scroll_to_top()
            }
            KeyCode::Char(c @ '1'..='4') => {
                let idx = (c as usize - '1' as usize).min(n - 1);
                self.help = Some(idx);
                scroll_to_top()
            }
            KeyCode::Up | KeyCode::Char('k') => scroll_by(-LINE),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char(' ') => scroll_by(LINE),
            KeyCode::PageUp => scroll_by(-PAGE),
            KeyCode::PageDown => scroll_by(PAGE),
            _ => Task::none(),
        }
    }

    /// Actions whose execution is irreducibly shell-side (`Effect::ShellAction`).
    fn run_shell_action(&mut self, action: ShellAction) -> Task<Message> {
        use ShellAction as A;
        match action {
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
            A::PlaceCursor(place) => {
                let task = self.place_cursor(place);
                Task::batch([task, self.maybe_fetch()])
            }
            A::ToggleWrap => {
                let Some(viewport_id) = self.session.viewport_id else {
                    return Task::none();
                };
                self.session.wrap = match self.session.wrap {
                    WrapMode::Soft => WrapMode::None,
                    WrapMode::None => WrapMode::Soft,
                };
                self.scroll_x_px = 0.0;
                let wrap = self.session.wrap;
                self.rpc::<ViewportSetWrap>(
                    ViewportSetWrapParams { viewport_id, wrap },
                    Message::WindowUpdate,
                )
            }
            A::OpenHelp => {
                self.help = Some(0);
                // Fresh scrollable starts at the top, but reset defensively in case its widget
                // state persisted from a prior open.
                iced::widget::operation::scroll_to(
                    help_scroll_id(),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
        }
    }

    // ---- actions ----------------------------------------------------------------------------

    fn subscribe_task(&mut self) -> Task<Message> {
        let Some((cols, rows)) = self.sent_grid else {
            return Task::none(); // no metrics yet; the first Layout event subscribes
        };
        // A fresh subscribe invalidates any in-flight fetch (new viewport identity); the core no
        // longer resets these on switch/reconnect — they live here now.
        self.fetch_in_flight = false;
        self.refetch_queued = false;
        self.reveal_after_fetch = None;
        let scroll = self.session.buffer.scroll.unwrap_or(ScrollPosition {
            // A fresh jump target (no saved scroll) rests near the top — the cross-buffer
            // counterpart of the in-buffer jump reveal.
            logical_line: self
                .session
                .buffer
                .cursor
                .position
                .line
                .saturating_sub((rows as f32 * CURSOR_REST_FRACTION) as u32),
            sub_row: 0.0,
        });
        self.subscribe_scroll = scroll;
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
                diff_view: self.session.diff_view,
            },
            Message::Subscribed,
        )
    }

    // ---- save / reload / close (ask-then-confirm handshakes) --------------------------------

    /// Scroll the results list so the highlighted row is in view: `Top` aligns the row to
    /// the top of the pane unless it's already visible (grep file-jumps — landing on a new
    /// file reveals it from its first hit without yanking an in-view jump).
    fn picker_reveal_selected_with(&mut self, reveal: Reveal) -> Task<Message> {
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        reveal_picker_selection(p, &mut self.picker_scroll_y, reveal)
    }

    // ---- search ---------------------------------------------------------------------------

    // ---- RPC helpers ------------------------------------------------------------------------

    /// One reconnect attempt, after `attempt`'s backoff: dial the fixed address
    /// (a restarted daemon rebinds the same port), re-activate the workspace, and reopen the
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
        let server_url = self.server_url.clone();
        let workspace = s.workspace.clone();
        let path = s.buffer.path.clone();
        let buffer_id = s.buffer.buffer_id;
        let transient = s.buffer.transient;
        let cursor = s.buffer.cursor.position;
        self.task(
            async move {
                tokio::time::sleep(reconnect_backoff(attempt)).await;
                let (handle, rx) = crate::connection::connect(&server_url, &version)
                    .await
                    .map_err(|_| ReconnectError::NotUp)?;
                let activated = match handle
                    .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
                        name: workspace,
                        open_last: false,
                    })
                    .await
                {
                    Ok(a) => a,
                    // The workspace is gone (renamed/removed while away) — hand back a workspace-less
                    // reconnect; the shell raises the chooser over this connection.
                    Err(_) => {
                        return Ok(Box::new(Reestablished {
                            handle,
                            notifications: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
                            restore: None,
                            server_url,
                            // No workspace re-activated, so no fresh instance stamp; treat as
                            // unknown. The chooser is raised over this connection and the next
                            // activation re-establishes the baseline.
                            server_started_at: 0,
                        }));
                    }
                };
                let params = match &path {
                    Some(p) => strip_longest_root(p, &activated.workspace.paths).map(
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
                    restore: Some((activated.workspace, open)),
                    server_url,
                    server_started_at: activated.server_started_at,
                }))
            },
            Message::Reconnected,
        )
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
        iced::clipboard::read().map(move |t| Message::Core(CoreEvent::ClipboardRead(kind, t)))
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
        let handle = self.handle.clone();
        self.task(
            async move { handle.rpc::<M>(params).await.map_err(|e| e.to_string()) },
            f,
        )
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
        self.scroll_anim = None;
        self.scroll_px += delta_px;
        self.clamp_scroll();
    }

    /// Horizontal scroll (no-op under soft wrap, where content always fits).
    fn scroll_x_by(&mut self, delta_px: f32) {
        if self.session.wrap != WrapMode::None || delta_px == 0.0 {
            return;
        }
        self.scroll_x_px = (self.scroll_x_px + delta_px).clamp(0.0, self.max_scroll_x_px());
    }

    /// Consume a pending relayout content anchor (set before a wrap/diff toggle) and resolve it into
    /// the new `scroll_px`. `None` when no anchor is pending (or no cell metrics yet) — the caller
    /// then falls back to clamp + reveal-cursor.
    fn resolve_anchor_px(&mut self) -> Option<f32> {
        let cell = self.cell?;
        let row = self.session.resolve_scroll_anchor()?;
        Some(row as f32 * cell.height)
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
            (Some(w), Some(cell)) => (PAD * 2.0 + w.total_visual_rows as f32 * cell.height
                - self.view_size.height)
                .max(0.0),
            _ => 0.0,
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll_px = self.scroll_px.clamp(0.0, self.max_scroll_px());
    }

    /// Scroll to `target` px — animated when the move is short enough to look good (the web
    /// client's `scrollTopTo`): smooth within ~1.5 viewports, snap beyond (a long glide would
    /// sail over not-yet-loaded rows and storm the server with window fetches).
    fn scroll_to_px(&mut self, target: f32, smooth: bool) {
        let target = target.clamp(0.0, self.max_scroll_px());
        let delta = (target - self.scroll_px).abs();
        let max_smooth = self
            .cell
            .map(|c| self.visible_rows() as f32 * c.height * 1.5)
            .unwrap_or(0.0);
        if smooth && delta > 0.0 && delta <= max_smooth {
            self.scroll_anim = Some(ScrollAnim {
                from: self.scroll_px,
                to: target,
                started: std::time::Instant::now(),
            });
        } else {
            self.scroll_anim = None;
            self.scroll_px = target;
        }
    }

    /// Where the view is headed: the animation target while a glide is in flight, the current
    /// offset otherwise — keypress-repeat scrolling accumulates from here.
    fn scroll_target(&self) -> f32 {
        self.scroll_anim
            .as_ref()
            .map(|a| a.to)
            .unwrap_or(self.scroll_px)
    }

    /// Fetch a new window when the view nears the loaded range's edge (web's `onScroll`).
    fn maybe_fetch(&mut self) -> Task<Message> {
        // No window fetches while the socket is down — the RPC would fail instantly and (on the
        // per-frame AnimTick path) spin a doomed retry every frame. The reconnect re-subscribes.
        if self.session.conn != ConnState::Connected {
            return Task::none();
        }
        let (Some(window), Some(cell), Some(viewport_id)) =
            (&self.session.window, self.cell, self.session.viewport_id)
        else {
            return Task::none();
        };
        let top_row = (((self.scroll_px - PAD) / cell.height).floor()).max(0.0) as u32;
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
        if self.fetch_in_flight {
            self.refetch_queued = true;
            return Task::none();
        }
        self.fetch_in_flight = true;
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
    fn ensure_cursor_visible(&mut self, style: RevealStyle) -> Task<Message> {
        let blame = self.maybe_blame();
        let reveal = self.ensure_cursor_visible_inner(style);
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
        self.rpc::<GitBlameLine>(
            GitBlameLineParams {
                buffer_id,
                line,
                include_commit_info: false,
            },
            move |result| {
                // Format here: "3w ago" needs a clock, which the core deliberately lacks.
                let text = result.ok().and_then(|r| r.blame).map(|b| {
                    if b.is_uncommitted {
                        "uncommitted".into()
                    } else {
                        format!("{} · {}", b.author, time_ago(b.timestamp))
                    }
                });
                Message::Core(CoreEvent::BlameLine {
                    buffer_id,
                    line,
                    text,
                })
            },
        )
    }

    fn ensure_cursor_visible_inner(&mut self, style: RevealStyle) -> Task<Message> {
        let Some(window) = &self.session.window else {
            return Task::none();
        };
        let line = self.session.buffer.cursor.position.line;
        if line < window.first_logical_line || line >= window.last_logical_line_exclusive {
            let Some(viewport_id) = self.session.viewport_id else {
                return Task::none();
            };
            self.reveal_after_fetch = Some(style);
            self.fetch_in_flight = true;
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
        self.reveal_cursor_styled(style);
        self.maybe_fetch()
    }

    fn reveal_cursor_styled(&mut self, style: RevealStyle) {
        match style {
            RevealStyle::Follow => self.reveal_cursor(),
            RevealStyle::Jump => self.reveal_cursor_jump(),
        }
    }

    /// Jump reveal: leave the view if the cursor is already visible, else rest it near the top.
    /// `scroll_to_px` animates a short glide there and snaps when the target is far (> ~1.5 screens).
    fn reveal_cursor_jump(&mut self) {
        let (Some(cell), Some(window)) = (self.cell, &self.session.window) else {
            return;
        };
        let Some((row, _, _)) =
            grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH)
        else {
            return;
        };
        let h = cell.height;
        let top = PAD + row as f32 * h;
        let view_h = self.view_size.height;
        // Already fully visible → don't disturb the view.
        if top >= self.scroll_px && top + h <= self.scroll_px + view_h {
            return;
        }
        self.scroll_to_px(top - view_h * CURSOR_REST_FRACTION, true);
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
        if top - margin < self.scroll_px {
            self.scroll_to_px((top - margin).max(0.0), true);
        } else if top + h + margin > self.scroll_px + view_h {
            self.scroll_to_px(top + h + margin - view_h, true);
        }
        // Horizontal (no-wrap): keep the cursor's column clear of the gutter and right edge.
        if self.session.wrap == WrapMode::None {
            let cx = dcol as f32 * cell.width; // content-space x
            let content_w = self.view_size.width - (GUTTER_COLS as f32 + 1.0) * cell.width;
            if cx < self.scroll_x_px {
                self.scroll_x_px = cx;
            } else if cx + cell.width > self.scroll_x_px + content_w {
                self.scroll_x_px = cx + cell.width - content_w;
            }
            self.scroll_x_px = self.scroll_x_px.clamp(0.0, self.max_scroll_x_px());
        }
    }

    fn place_cursor(&mut self, place: ViewportPlace) -> Task<Message> {
        let line = self.session.buffer.cursor.position.line;
        let loaded = self
            .session
            .window
            .as_ref()
            .map(|w| (w.first_logical_line, w.last_logical_line_exclusive));
        let Some((first, last)) = loaded else {
            return Task::none();
        };
        // When the cursor's line has been scrolled out of the loaded window, its visual row is
        // unknown — pull that region from the server (scrolling the viewport to the line), then
        // place once it lands. Mirrors `ensure_cursor_visible_inner`.
        if line < first || line >= last {
            let Some(viewport_id) = self.session.viewport_id else {
                return Task::none();
            };
            self.place_after_fetch = Some(place);
            self.fetch_in_flight = true;
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
        self.place_cursor_in_window(place);
        Task::none()
    }

    /// Scroll so the cursor's line sits a fixed fraction down the viewport. Assumes the line is in
    /// the loaded window (the caller pulls it in first otherwise); a no-op if its cell is unknown.
    fn place_cursor_in_window(&mut self, place: ViewportPlace) {
        let (Some(cell), Some(window)) = (self.cell, &self.session.window) else {
            return;
        };
        let Some((row, _, _)) =
            grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH)
        else {
            return;
        };
        self.scroll_to_px(
            PAD + row as f32 * cell.height - self.view_size.height * place.fraction(),
            true,
        );
    }

    // ---- notifications ------------------------------------------------------------------------

    // ---- view ----------------------------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        if let Some(boot) = &self.boot {
            return self.boot_view(boot);
        }
        // Boot-connecting renders the normal editor chrome over a placeholder session (empty editor
        // + status bar), with the floating "Connecting…" banner — the same familiar feel as a
        // mid-session reconnect. The editor's Layout fires no RPC while not `Connected`
        // (`on_editor_event`), and `status_bar` is fully Option-guarded, so the placeholder is safe.
        let editor = editor::editor(
            editor::Content {
                window: self.session.window.as_ref(),
                cursor: self.session.buffer.cursor,
                insert_mode: self.session.mode == Mode::Insert,
                awaiting_key: !matches!(self.session.pending, Pending::None)
                    || self.session.count.is_some()
                    || self.session.sneak.is_some(),
                diff_view: self.session.diff_view,
                scroll_px: self.scroll_px,
                scroll_x_px: self.scroll_x_px,
                blame: self
                    .session
                    .blame
                    .as_ref()
                    .map(|(line, text)| (*line, text.as_str())),
                tab_width: TAB_WIDTH,
                ligatures: self.session.ligatures,
                font_size: self.session.font_size as f32,
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
        if self.hover.is_some() {
            layers.push(self.hover_overlay());
        }
        if let Some(p) = &self.session.picker {
            layers.push(
                Element::from(crate::picker::overlay(
                    p,
                    &self.session.workspace_paths,
                    self.picker_scroll_y,
                    self.spinner_phase,
                    true,
                    0,
                ))
                .map(|m| match m {
                    PickerMsg::Click(abs) => Message::Core(CoreEvent::PickerClicked(abs)),
                    PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
                    PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
                    PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
                    PickerMsg::ChipClicked(i) => Message::Core(CoreEvent::PickerChipClicked(i)),
                    PickerMsg::Query(q) => Message::OverlayInput(OverlayField::PickerQuery, q),
                    PickerMsg::EditorRoot(s) => Message::OverlayInput(OverlayField::ChipRoot, s),
                    PickerMsg::EditorPath(s) => Message::OverlayInput(OverlayField::ChipPath, s),
                    PickerMsg::CoreKey(code) => core_key_message(code),
                }),
            );
        }
        if self.session.workspace_settings.is_some() {
            layers.push(self.workspace_settings_overlay());
        }
        if self.session.app_settings.is_some() {
            layers.push(self.app_settings_overlay());
        }
        // The confirm prompt (e.g. remove-root) layers *above* the settings dialog.
        if self.session.prompt.is_some() {
            layers.push(self.prompt_overlay());
        }
        if let Some(tab) = self.help {
            layers.push(self.help_overlay(tab));
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
            // Boot before the daemon is up — distinct copy from a mid-session blip.
            ConnState::Connecting => ("Connecting…", theme::NORD13, theme::NORD0),
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

    /// The keyboard-shortcuts dialog (`Space ?`): a centred modal with a tab bar
    /// (Normal/Insert/Search/Application) over a scrollable, two-column list of the active tab's
    /// bindings — content from `keymap::help_entries`, matching the web client. Keyboard-driven
    /// (see `on_help_key`); `opaque` blocks the editor behind it.
    fn help_overlay(&self, tab: usize) -> Element<'_, Message> {
        // Tab bar: active tab in frost-blue bold, the rest dim.
        let mut tabs = row![].spacing(16).align_y(iced::Alignment::Center);
        for (i, name) in HELP_TABS.iter().enumerate() {
            let active = i == tab;
            tabs = tabs.push(
                text(*name)
                    .size(14)
                    .font(if active { SANS_BOLD_UI } else { SANS })
                    .color(if active {
                        theme::NORD8
                    } else {
                        theme::NORD3_BRIGHT
                    }),
            );
        }

        // Group the active tab's entries into ordered sections.
        let tab_name = HELP_TABS[tab];
        let mut sections: Vec<(&'static str, Vec<(String, &'static str)>)> = Vec::new();
        for e in crate::keymap::help_entries() {
            if e.tab != tab_name {
                continue;
            }
            match sections.iter_mut().find(|(g, _)| *g == e.group) {
                Some((_, rows)) => rows.push((e.keys, e.desc)),
                None => sections.push((e.group, vec![(e.keys, e.desc)])),
            }
        }

        // Spread sections across two columns, balancing by accumulated row count.
        let mut col_a = column![].spacing(16);
        let mut col_b = column![].spacing(16);
        let (mut a_rows, mut b_rows) = (0usize, 0usize);
        for (group, rows) in sections {
            let n = rows.len() + 1;
            let sec = help_section(group, rows);
            if a_rows <= b_rows {
                col_a = col_a.push(sec);
                a_rows += n;
            } else {
                col_b = col_b.push(sec);
                b_rows += n;
            }
        }
        let grid = row![
            col_a.width(Length::FillPortion(1)),
            col_b.width(Length::FillPortion(1)),
        ]
        .spacing(28);

        let body = iced::widget::scrollable(container(grid).padding([4, 2]))
            .id(help_scroll_id())
            .height(Length::Fill)
            .direction(iced::widget::scrollable::Direction::Vertical(
                iced::widget::scrollable::Scrollbar::new()
                    .width(5)
                    .margin(0)
                    .scroller_width(5),
            ));

        // Modal box, sized to the window with margins (web: min(760, 92vw) × 80vh).
        let w = (self.view_size.width - 64.0).clamp(320.0, 760.0);
        let h = (self.view_size.height * 0.8).max(200.0);
        let modal = container(column![tabs, body].spacing(12))
            .width(Length::Fixed(w))
            .max_height(h)
            .padding(16)
            .style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                border: iced::Border {
                    color: theme::NORD3,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba8(0, 0, 0, 0.4),
                    offset: iced::Vector::new(0.0, 12.0),
                    blur_radius: 40.0,
                },
                ..container::Style::default()
            });

        // Dimmed full-screen backdrop, centred. `opaque` swallows clicks so they don't fall through
        // to the editor (the dialog is keyboard-driven: Esc / q / ? close it).
        iced::widget::opaque(
            container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .style(|_| container::Style {
                    background: Some(iced::Color::from_rgba8(20, 24, 30, 0.5).into()),
                    ..container::Style::default()
                }),
        )
    }

    /// The workspace-settings dialog (`Space ,`): a centred modal with the editable workspace name,
    /// the list of roots, and an add-root input row — rendered from the core's
    /// `session.workspace_settings`. Keyboard-driven (keys route through `session.on_key`, which the
    /// core handles): Alt-j/k navigate, Enter renames / adds, Delete (then y) removes, Esc closes.
    /// Mirrors `help_overlay`'s NORD modal box + opaque backdrop.
    fn workspace_settings_overlay(&self) -> Element<'_, Message> {
        self.workspace_settings_body()
    }

    /// The dialog content. The name + add-root fields are controlled `text_input`s (web parity,
    /// syncing via `workspace_settings_set_name` / `_set_add`); the per-root delete buttons carry
    /// `WorkspaceSettingsMsg` mapped inline to `Message` (since the inputs already produce `Message`,
    /// the whole tree is `Message`-typed rather than mapped at the end).
    fn workspace_settings_body(&self) -> Element<'_, Message> {
        let s = self.session.workspace_settings.as_ref().unwrap();

        // An editable field: a controlled `text_input` keyed to its core setter. Wrapped in a
        // fixed-height row so the box never resizes between the focused/unfocused states. The
        // `text_input` itself shows the value (NORD6) or the dim placeholder when empty, and draws
        // its own caret/selection when focused — the focus follows the dialog's `selected` (the
        // shell re-focuses on selection change via `sync_focus`).
        let field =
            |fieldkind: OverlayField, value: &str, placeholder: &str| -> Element<'_, Message> {
                // No fixed height: a size-13 `text_input` needs ~17px, so clamping the row to 15 clipped
                // the text. Both states are the same widget now, so the box height is already consistent.
                overlay_input(fieldkind, placeholder, value)
            };

        // A boxed, optionally-highlighted input/row container.
        fn boxed_row<'a>(content: Element<'a, Message>, highlighted: bool) -> Element<'a, Message> {
            container(content)
                .padding([5, 8])
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(
                        if highlighted {
                            theme::NORD2
                        } else {
                            theme::NORD0
                        }
                        .into(),
                    ),
                    border: iced::Border {
                        color: if highlighted {
                            theme::NORD8
                        } else {
                            theme::NORD3
                        },
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..container::Style::default()
                })
                .into()
        }

        let label = |t: &str| {
            text(t.to_string())
                .size(12)
                .font(SANS)
                .color(theme::NORD3_BRIGHT)
        };

        // A label tucked tight above its field (~3px), so each label+field reads as one group
        // while the column's `spacing(8)` keeps groups apart.
        let name_group = column![
            label("Name"),
            boxed_row(
                field(OverlayField::WorkspaceName, &s.name.text, ""),
                s.on_name(),
            ),
        ]
        .spacing(3);

        let mut col = column![
            text("Workspace settings")
                .size(14)
                .font(SANS_BOLD_UI)
                .color(theme::NORD6),
            name_group,
        ]
        .spacing(8);

        // The Roots group: the label, then the root rows (each with a delete button), then the
        // always-present add-root input row.
        let mut roots_col = column![label("Roots")].spacing(2);
        if s.roots.is_empty() {
            roots_col = roots_col.push(
                text("(no roots — add one below)")
                    .size(12)
                    .font(SANS)
                    .color(theme::NORD3_BRIGHT),
            );
        }
        // A bulleted row: `• <content> …`, indented one bullet-gap from the label (web parity).
        // No row box — selection tints only the path text (see below).
        fn bulleted(inner: Element<'_, Message>) -> Element<'_, Message> {
            container(
                row![text("•").size(13).font(SANS).color(theme::NORD6), inner]
                    .align_y(iced::Alignment::Center)
                    .spacing(6),
            )
            .padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 6.0,
            })
            .into()
        }

        for (i, root) in s.roots.iter().enumerate() {
            let highlighted = s.selected == i + 1;
            let delete = iced::widget::button(text("✕").size(12).font(SANS).color(theme::NORD6))
                .padding([2, 8])
                .style(|_, status| iced::widget::button::Style {
                    background: Some(
                        if matches!(status, iced::widget::button::Status::Hovered) {
                            theme::NORD11
                        } else {
                            theme::NORD3
                        }
                        .into(),
                    ),
                    text_color: theme::NORD6,
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..iced::Border::default()
                    },
                    ..iced::widget::button::Style::default()
                })
                .on_press(WorkspaceSettingsMsg::RemoveRoot(i));
            // The delete button is the only `WorkspaceSettingsMsg` source; map it inline so this row
            // joins the `Message`-typed tree (the input fields already produce `Message`).
            let delete = Element::from(delete).map(|m| match m {
                WorkspaceSettingsMsg::RemoveRoot(i) => {
                    Message::Core(CoreEvent::WorkspaceSettingsRemoveRoot(i))
                }
            });
            // Selection tints just the path text (web/terminal parity), so the background hugs the
            // text — no padding, so the text lines up with the borderless add-root input below.
            let path = container(text(root.clone()).size(13).font(SANS).color(theme::NORD6)).style(
                move |_| container::Style {
                    background: highlighted.then(|| theme::NORD2.into()),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..iced::Border::default()
                    },
                    ..container::Style::default()
                },
            );
            let inner = row![path, iced::widget::Space::new().width(Length::Fill), delete,]
                .align_y(iced::Alignment::Center)
                .spacing(6);
            roots_col = roots_col.push(bulleted(inner.into()));
        }

        // The always-present add-root input row — a borderless input after its bullet, so the caret
        // is the focus cue (web/terminal parity), not a box.
        roots_col = roots_col.push(bulleted(field(
            OverlayField::WorkspaceAddRoot,
            &s.add.text,
            "Add root...",
        )));
        col = col.push(roots_col);

        if let Some(err) = &s.error {
            col = col.push(text(err.clone()).size(12).font(SANS).color(theme::NORD11));
        }

        let boxed = container(col.spacing(8))
            .width(480)
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

        // Opaque, dimmed backdrop, centred. Clicks on the dialog's delete buttons are handled;
        // clicks on the backdrop are swallowed (no fall-through to the editor).
        iced::widget::opaque(
            container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .style(|_| container::Style {
                    background: Some(iced::Color::from_rgba8(20, 24, 30, 0.5).into()),
                    ..container::Style::default()
                }),
        )
    }

    /// The application-settings overlay (`Space .`). Grouped checkbox settings: a frost-accent group
    /// header, then each setting as a left-aligned label + native checkbox on the right, with its
    /// description grouped on the line directly below. Clicking a checkbox toggles that setting
    /// (`AppSettingToggle`); keys also work (Alt-j/k or Up/Down move, Enter/Space toggles, Esc
    /// closes). Only the focused setting's *checkbox* is ringed (not the whole row). Mirrors the
    /// workspace-settings modal box + dimmed backdrop.
    fn app_settings_overlay(&self) -> Element<'_, Message> {
        let s = self.session.app_settings.as_ref().unwrap();
        let groups = self.session.app_setting_groups();

        let mut col = column![text("Application settings")
            .size(14)
            .font(SANS_BOLD_UI)
            .color(theme::NORD6)]
        .spacing(14);

        // Running flat row index across groups (the index `AppSettingToggle` / `selected` use).
        let mut flat = 0usize;
        for group in &groups {
            let mut gcol = column![text(group.title.to_string())
                .size(12)
                .font(SANS_BOLD_UI)
                .color(theme::NORD8)]
            .spacing(10);
            for r in &group.rows {
                let i = flat;
                flat += 1;
                let focused = s.selected == i;
                // The focus ring sits on just the control (a future row may carry several
                // controls, so highlighting the whole row would be ambiguous). A toggle renders a
                // checkbox; a stepped value (font size) renders a pill button — clicking either
                // activates the row (flip / step to the next preset), the same as Enter/Space.
                let control: Element<'_, Message> = match r.control {
                    AppSettingControl::Toggle(on) => iced::widget::checkbox(on)
                        .size(16)
                        .on_toggle(move |_| Message::Core(CoreEvent::AppSettingToggle(i)))
                        .into(),
                    AppSettingControl::Value(v) => {
                        // `button` needs a `Clone` press message and `Message` isn't `Clone`, so the
                        // button carries the row index (a `usize`) and we map it to `Message` — the
                        // same pattern as the workspace-settings delete button.
                        let btn = iced::widget::button(
                            text(v.to_string()).size(13).font(SANS).color(theme::NORD6),
                        )
                        .padding([2, 8])
                        .style(|_, status| iced::widget::button::Style {
                            background: Some(
                                if matches!(status, iced::widget::button::Status::Hovered) {
                                    theme::NORD3
                                } else {
                                    theme::NORD2
                                }
                                .into(),
                            ),
                            text_color: theme::NORD6,
                            border: iced::Border {
                                color: theme::NORD3,
                                width: 1.0,
                                radius: 4.0.into(),
                            },
                            ..iced::widget::button::Style::default()
                        })
                        .on_press(i);
                        Element::from(btn)
                            .map(|idx| Message::Core(CoreEvent::AppSettingToggle(idx)))
                    }
                };
                let check = container(control)
                    .padding(2)
                    .style(move |_| container::Style {
                        border: iced::Border {
                            color: if focused {
                                theme::NORD8
                            } else {
                                iced::Color::TRANSPARENT
                            },
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        ..container::Style::default()
                    });
                // Label + checkbox, then the description grouped tight beneath the label.
                let field = column![
                    row![
                        text(r.label.to_string())
                            .size(13)
                            .font(SANS)
                            .color(theme::NORD6),
                        iced::widget::Space::new().width(Length::Fill),
                        check,
                    ]
                    .align_y(iced::Alignment::Center)
                    .spacing(6),
                    text(r.hint.to_string())
                        .size(12)
                        .font(SANS)
                        .color(theme::NORD3_BRIGHT),
                ]
                .spacing(2);
                gcol = gcol.push(field);
            }
            col = col.push(gcol);
        }

        let boxed = container(col.spacing(14))
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

        iced::widget::opaque(
            container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .style(|_| container::Style {
                    background: Some(iced::Color::from_rgba8(20, 24, 30, 0.5).into()),
                    ..container::Style::default()
                }),
        )
    }

    /// The no-args start screen: just the Workspaces picker over the editor background.
    fn boot_view<'a>(&'a self, boot: &'a Boot) -> Element<'a, Message> {
        let backdrop = container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(theme::NORD0.into()),
                ..container::Style::default()
            });
        // The boot chooser's picker query lives outside the core session (it's driven by
        // `on_boot_key` / `boot.picker`), so it keeps the fake-caret rendering: `controlled=false`,
        // and `PickerMsg::Query` can never fire here.
        let picker = Element::from(crate::picker::overlay(
            &boot.picker,
            &[],
            self.picker_scroll_y,
            self.spinner_phase,
            false,
            boot.query_cursor,
        ))
        .map(|m| match m {
            PickerMsg::Click(abs) => Message::Core(CoreEvent::PickerClicked(abs)),
            PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
            PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
            PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
            PickerMsg::ChipClicked(i) => Message::Core(CoreEvent::PickerChipClicked(i)),
            // The boot chooser is the Workspaces picker — no query sync, no chip editor, no chip-row
            // boundary keys — so none of the controlled-input / chip messages can fire here.
            PickerMsg::Query(_)
            | PickerMsg::EditorRoot(_)
            | PickerMsg::EditorPath(_)
            | PickerMsg::CoreKey(_) => Message::Noop,
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
        // The query input is a controlled `text_input` (web parity): its value is the core's
        // search query, edits sync via `search_set_query`, and Enter/Up/Down/Esc bubble to
        // `on_key` (commit / history nav / cancel) since `on_submit` is unset. With option chips
        // present (and none yet selected), Left/Backspace at the query start steps into the chip
        // row instead of editing — the browser tag-input gesture, mirroring the picker query.
        let chips = self.session.search.option_chips();
        let input = {
            let inner = iced::widget::text_input("Search", &self.session.search.query)
                .id(OverlayField::Search.id())
                .on_input(SearchInputMsg::Typed)
                .font(SANS)
                .size(13)
                .padding(0)
                .width(Length::Fill)
                .style(|_theme, _status| iced::widget::text_input::Style {
                    background: iced::Background::Color(iced::Color::TRANSPARENT),
                    border: iced::Border::default(),
                    icon: theme::NORD6,
                    placeholder: theme::NORD3_BRIGHT,
                    value: theme::NORD6,
                    selection: theme::NORD8,
                });
            let intercept = !chips.is_empty() && self.session.search.chip_selected.is_none();
            let wrapped = if intercept {
                crate::alt_filter::alt_passthrough_intercept(
                    inner,
                    self.session.search.query.clone(),
                    move |key, at_start| {
                        use iced::keyboard::key::Named;
                        if !at_start {
                            return None;
                        }
                        match key {
                            iced::keyboard::Key::Named(Named::ArrowLeft) => {
                                Some(SearchInputMsg::CoreKey(KeyCode::Left))
                            }
                            iced::keyboard::Key::Named(Named::Backspace) => {
                                Some(SearchInputMsg::CoreKey(KeyCode::Backspace))
                            }
                            _ => None,
                        }
                    },
                )
            } else {
                crate::alt_filter::alt_passthrough(inner)
            };
            wrapped.map(|m| match m {
                SearchInputMsg::Typed(s) => Message::OverlayInput(OverlayField::Search, s),
                SearchInputMsg::CoreKey(code) => core_key_message(code),
            })
        };
        // Active match options (case / whole-word / literal) lead the row as chips, styled like
        // the grep picker's filter chips. The chip row is *always* the first child (empty when no
        // options are set) so the query input keeps a stable tree position — prepending a chip must
        // not knock focus off the `text_input`.
        let selected = self.session.search.chip_selected;
        let mut chips_row = row![].spacing(4).align_y(iced::Alignment::Center);
        for (i, chip) in chips.iter().enumerate() {
            chips_row = chips_row.push(option_chip(chip, selected == Some(i)));
        }
        if !chips.is_empty() {
            chips_row = chips_row.push(iced::widget::Space::new().width(6));
        }
        let mut bar = row![chips_row, input]
            .spacing(0)
            .width(Length::Fill)
            .align_y(iced::Alignment::Center);
        bar = bar.push(iced::widget::Space::new().width(Length::Fill));
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
        // The save-as arm embeds a controlled `text_input` (which produces `Message`), so the
        // whole body is built in `Message` space: the Clone-only buttons map their `PromptMsg`
        // immediately rather than the whole tree being mapped at the end.
        // The modal button roles, mirroring the web client's `.modal-btn` classes: `Default` is the
        // safe, Enter-target option (Cancel/No) — a plain, subtly bordered button; `Danger` is a
        // destructive confirm (Yes) in red; `Primary` is a non-destructive affirmative (Save) in
        // frost blue.
        #[derive(Clone, Copy)]
        enum BtnRole {
            Default,
            Danger,
            Primary,
        }
        let btn = |label: &str, role: BtnRole, msg: PromptMsg| -> Element<'_, Message> {
            Element::from(
                iced::widget::button(
                    text(label.to_string())
                        .size(13)
                        .font(SANS)
                        .color(theme::NORD6),
                )
                .padding([5, 14])
                .style(move |_, _| {
                    let (bg, border_width, border_color) = match role {
                        BtnRole::Default => (theme::NORD2, 1.0, theme::NORD3),
                        BtnRole::Danger => (theme::NORD11, 0.0, iced::Color::TRANSPARENT),
                        BtnRole::Primary => (theme::NORD10, 0.0, iced::Color::TRANSPARENT),
                    };
                    iced::widget::button::Style {
                        background: Some(bg.into()),
                        text_color: theme::NORD6,
                        border: iced::Border {
                            radius: 4.0.into(),
                            width: border_width,
                            color: border_color,
                        },
                        ..iced::widget::button::Style::default()
                    }
                })
                .on_press(msg),
            )
            .map(|m| match m {
                PromptMsg::Accept => Message::Core(CoreEvent::PromptAccept),
                PromptMsg::Cancel => Message::Core(CoreEvent::PromptCancel),
            })
        };
        let body: Element<'_, Message> = match prompt {
            Prompt::LspInfo(info) => {
                let busy = matches!(info.status, LspStatus::Ready) && !info.progress.is_empty();
                let dot = if busy {
                    theme::NORD13
                } else {
                    theme::lsp_status_color(&info.status)
                };
                let kv = |k: &str, v: String| {
                    row![
                        container(
                            text(k.to_string())
                                .size(13)
                                .font(SANS)
                                .color(theme::NORD3_BRIGHT)
                        )
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
                        text(info.name.clone())
                            .size(13)
                            .font(SANS_BOLD_UI)
                            .color(theme::NORD6),
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
                col.spacing(10).into()
            }
            Prompt::Confirm { kind, .. } => column![
                text(format!("{}?", confirm_phrase(kind)))
                    .size(13)
                    .font(SANS)
                    .color(theme::NORD6),
                row![
                    iced::widget::Space::new().width(Length::Fill),
                    btn("No", BtnRole::Default, PromptMsg::Cancel),
                    btn("Yes", BtnRole::Danger, PromptMsg::Accept),
                ]
                .spacing(8),
            ]
            .spacing(14)
            .into(),
            Prompt::SaveAs(ed) => {
                // The save-as editor mirrors the dir chip editor's directory-completion UX: in
                // multi-root workspaces a leading root-filter segment (smartcase typeahead + gray
                // ghost), a `:` separator, then the root-relative path; single-root shows just the
                // path. Both segments are the controlled-`text_input`-over-ghost-layer shape from
                // the picker (`field_with_ghost`), so the look stays consistent. Edits sync via
                // `OverlayInput`; Enter / Esc / Tab / Alt-* bubble to `on_key`, and the `:` /
                // Backspace boundaries forward through `CoreKey` (web/TUI parity). The whole row is
                // built in `PickerMsg` space then mapped to `Message`.
                use crate::picker::{field_with_ghost, Boundary, PickerMsg};
                let roots = &self.session.workspace_paths;
                let labels = crate::labels::root_labels(roots);
                let multi_root = roots.len() > 1;
                let mut field = row![].align_y(iced::Alignment::Center);
                if multi_root {
                    let invalid = ed.root_invalid(&labels);
                    // The root segment and its flush `:` separator sit at zero spacing (the colon
                    // hugs the root rather than dangling 6px off it); the row gap separates this
                    // group from the path that follows.
                    let mut root_group = row![].spacing(0).align_y(iced::Alignment::Center);
                    if ed.field == crate::chips::ChipEditorField::Root {
                        let ghost = ed.root_ghost(&labels).map(|(_, suffix)| suffix);
                        root_group = root_group.push(field_with_ghost(
                            &ed.root_filter,
                            ghost,
                            invalid,
                            OverlayField::SaveAsRoot.id(),
                            "",
                            PickerMsg::EditorRoot,
                            true,
                            Boundary::ConfirmRoot,
                        ));
                    } else {
                        // Unfocused root: the chosen label in breadcrumb blue — or the raw filter
                        // text, red, when it matches nothing.
                        let display = if invalid {
                            ed.root_filter.text.clone()
                        } else {
                            labels
                                .get(ed.chosen_root(&labels) as usize)
                                .cloned()
                                .unwrap_or_default()
                        };
                        let color = if invalid { theme::NORD11 } else { theme::NORD8 };
                        root_group =
                            root_group.push(text(display).size(13).font(SANS).color(color));
                    }
                    root_group =
                        root_group.push(text(":").size(13).font(SANS).color(theme::NORD3_BRIGHT));
                    field = field.push(root_group).spacing(6);
                }
                // The path field: typed value plus the gray `path_ghost` suffix, red on invalid
                // (parent dir failed to list). Only a multi-root path can step back into the root.
                let path_boundary = if multi_root {
                    Boundary::PathToRoot
                } else {
                    Boundary::None
                };
                field = field.push(field_with_ghost(
                    &ed.input,
                    ed.path_ghost(),
                    ed.path_invalid(),
                    OverlayField::SaveAs.id(),
                    "",
                    PickerMsg::EditorPath,
                    false,
                    path_boundary,
                ));
                let field: Element<'_, Message> = Element::from(field).map(|m| match m {
                    PickerMsg::EditorRoot(s) => Message::OverlayInput(OverlayField::SaveAsRoot, s),
                    PickerMsg::EditorPath(s) => Message::OverlayInput(OverlayField::SaveAs, s),
                    PickerMsg::CoreKey(code) => core_key_message(code),
                    // The save-as segments never emit row/scroll/chip messages.
                    _ => Message::Noop,
                });
                column![
                    text("Save as").size(13).font(SANS).color(theme::NORD6),
                    container(field)
                        .padding([5, 8])
                        .width(Length::Fill)
                        .style(|_| {
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
                        btn("Cancel", BtnRole::Default, PromptMsg::Cancel),
                        btn("Save", BtnRole::Primary, PromptMsg::Accept),
                    ]
                    .spacing(8),
                ]
                .spacing(14)
                .into()
            }
            Prompt::OpenPath(field) => {
                // A plain single-line path input — no root chips, unlike save-as. Edits sync via
                // `OverlayInput`; Enter (open) / Esc (cancel) bubble to `on_key` since `on_submit`
                // is unset (focused inputs report Enter `Ignored` and Esc is force-forwarded — see
                // `subscription`).
                // `on_input` produces `String` (a `Clone` message, which `text_input` requires),
                // then the element is mapped to `Message`, mirroring the search bar.
                let inner = iced::widget::text_input("path to open", &field.text)
                    .id(OverlayField::OpenPath.id())
                    .on_input(|s| s)
                    .font(SANS)
                    .size(13)
                    .padding(0)
                    .width(Length::Fill)
                    .style(|_theme, _status| iced::widget::text_input::Style {
                        background: iced::Background::Color(iced::Color::TRANSPARENT),
                        border: iced::Border::default(),
                        icon: theme::NORD6,
                        placeholder: theme::NORD3_BRIGHT,
                        value: theme::NORD6,
                        selection: theme::NORD8,
                    });
                let input: Element<'_, Message> = Element::from(inner)
                    .map(|s: String| Message::OverlayInput(OverlayField::OpenPath, s));
                column![
                    text("Open file").size(13).font(SANS).color(theme::NORD6),
                    container(input).padding([5, 8]).width(Length::Fill).style(|_| {
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
                        btn("Cancel", BtnRole::Default, PromptMsg::Cancel),
                        btn("Open", BtnRole::Primary, PromptMsg::Accept),
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
            })
            .into()
    }

    /// The hover popover, anchored at the cursor cell: below it when there's room, above
    /// otherwise (estimated from the content's line count), clamped into the view.
    fn hover_overlay(&self) -> Element<'_, Message> {
        let content = self.hover.as_ref().unwrap();
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
                    // Sans-serif, matching the markdown (LSP) hover and the rest of the chrome —
                    // the app default font is monospace, so diagnostic/commit blocks must opt in.
                    // Diagnostic blocks (those with a severity) lead with the severity glyph,
                    // matching the status-bar count and picker.
                    let line: Element<'_, Message> = match b.severity {
                        Some(sev) => row![
                            text(theme::diag_glyph(sev))
                                .size(13)
                                .font(SANS)
                                .color(color),
                            text(b.text.clone()).size(13).font(SANS).color(color),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Start)
                        .into(),
                        None => text(b.text.clone()).size(13).font(SANS).color(color).into(),
                    };
                    col = col.push(line);
                }
                col.into()
            }
            HoverContent::Markdown {
                blocks,
                est_lines: n,
            } => {
                est_lines = *n;
                md_doc(blocks)
            }
        };
        // Anchor at the cursor cell. Pick below/above by the room each side has for the
        // (estimated) height, then cap the popover to that room so tall (scrolled) content fits
        // *within* the window instead of overflowing its edge. The popover stays open while the
        // buffer scrolls, and even once the cursor scrolls out of the loaded window it keeps its
        // horizontal column and parks against the edge it left by (rather than jumping to a corner).
        const MARGIN: f32 = 4.0;
        const MAX_H: f32 = 380.0;
        let est_h = est_lines as f32 * 19.0 + 20.0;
        let mut anchor = None;
        let mut max_h = MAX_H;
        if let (Some(cell), Some(window)) = (self.cell, &self.session.window) {
            let pc = grid::position_cell(window, self.session.buffer.cursor.position, TAB_WIDTH);
            // Horizontal anchor: refreshed while the cursor is in the loaded window, and retained
            // when it scrolls out of range so the popover keeps its column instead of jumping left.
            let x = match pc {
                Some((_, dcol, _)) => {
                    let x = ((GUTTER_COLS + dcol) as f32 * cell.width)
                        .min((self.view_size.width - 360.0).max(8.0))
                        .max(4.0);
                    self.hover_anchor_x.set(x);
                    x
                }
                None => self.hover_anchor_x.get(),
            };
            let view_h = self.view_size.height;
            // Constant size once open (like the web client): a fixed height cap, never resized by
            // how much room is left as the buffer scrolls. `h_est` is the assumed rendered height,
            // used only to clamp the anchor so the popover stays within the view.
            max_h = MAX_H.min((view_h - 2.0 * MARGIN).max(40.0));
            let h_est = est_h.min(max_h);
            let place = match pc {
                // Cursor scrolled out of the loaded window: park against the edge it left by
                // (orientation no longer matters — the line isn't visible).
                None if self.session.buffer.cursor.position.line < window.first_logical_line => {
                    HoverPlace::Top(MARGIN)
                }
                None => HoverPlace::Bottom(view_h - MARGIN),
                Some((row, _, _)) => {
                    let line_top = PAD + row as f32 * cell.height - self.scroll_px;
                    let line_bottom = line_top + cell.height;
                    // Orientation is decided once (the first frame, line on-screen) and retained, so
                    // the popover never flips sides mid-scroll: below if it fits there, else above if
                    // it fits, else the roomier side.
                    let below = match self.hover_below.get() {
                        Some(b) => b,
                        None => {
                            let ab = view_h - (line_bottom + 2.0) - MARGIN;
                            let aa = (line_top - 2.0) - MARGIN;
                            let b = if est_h <= ab {
                                true
                            } else if est_h <= aa {
                                false
                            } else {
                                ab >= aa
                            };
                            self.hover_below.set(Some(b));
                            b
                        }
                    };
                    // Hang on the chosen side, following the line; once it no longer fits there,
                    // pin to that edge — *edge-anchored* so the clamped position is exact regardless
                    // of the height estimate (the estimate only decides when to switch, not where it
                    // lands, so the clamp is consistent for short and tall popovers alike).
                    if below {
                        if line_bottom + 2.0 + h_est <= view_h - MARGIN {
                            HoverPlace::Top((line_bottom + 2.0).max(MARGIN))
                        } else {
                            HoverPlace::Bottom(view_h - MARGIN)
                        }
                    } else if line_top - 2.0 - h_est >= MARGIN {
                        HoverPlace::Bottom((line_top - 2.0).min(view_h - MARGIN))
                    } else {
                        HoverPlace::Top(MARGIN)
                    }
                }
            };
            anchor = Some((x, place));
        }

        // Long content scrolls within the popover rather than growing past the view. The
        // padding lives inside the scrollable so its scrollbar sits against the popover edge.
        let boxed = container(
            iced::widget::scrollable(container(body).padding([8, 10]))
                .id(hover_scroll_id())
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::new()
                        .width(5)
                        .margin(0)
                        .scroller_width(5),
                )),
        )
        .max_width(640)
        .max_height(max_h)
        .style(|_| container::Style {
            background: Some(theme::NORD1.into()),
            border: iced::Border {
                color: theme::NORD3,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        });
        // Make the box opaque to mouse presses so a click on it doesn't fall through to the editor
        // below (which would dismiss the popover *and* move the cursor). `opaque` updates its content
        // first, so link clicks inside still open; it only swallows presses that nothing else
        // consumed. Clicks in the transparent area *outside* the box still reach — and dismiss — the
        // editor.
        let boxed = iced::widget::opaque(boxed);
        match anchor {
            // Hangs down: top edge at `top`. `clip` keeps a height-underestimated popover from
            // drawing past the editor (over the status bar).
            Some((x, HoverPlace::Top(top))) => container(boxed)
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .padding(iced::Padding {
                    top,
                    right: 12.0,
                    bottom: 0.0,
                    left: x,
                })
                .into(),
            // Hangs up: a box ending at `bottom`, the popover hugging its lower edge.
            Some((x, HoverPlace::Bottom(bottom))) => container(
                container(boxed)
                    .width(Length::Fill)
                    .height(bottom.max(40.0))
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(iced::Padding {
                        right: 12.0,
                        left: x,
                        ..iced::Padding::ZERO
                    }),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .clip(true)
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
    /// mode lives in the cursor shape). Left: state dot, `[workspace] file` (italic when
    /// transient), git cluster. Right: grep position, diagnostic counts, cursor position, LSP
    /// health dot.
    fn status_bar(&self) -> Element<'_, Message> {
        let t = |s: String, color: iced::Color| text(s).size(13).font(SANS).color(color);

        let mut left = row![];
        if let Some(color) = self.buffer_state_color() {
            left = left.push(t("● ".into(), color));
        }
        // Persisted workspace → `[name] ` prefix. No workspace (boot/connecting/chooser) or an
        // ephemeral "(no workspace)" context → no prefix, so the bar shows just the file label
        // rather than a stray `[]` or a `[(no workspace)]` that reads like a real workspace.
        if crate::labels::shows_workspace_chrome(&self.session.workspace) {
            left = left.push(t(format!("[{}] ", self.session.workspace), theme::NORD4));
        }
        // Segment-elide long labels to roughly half the bar so the filename survives (the
        // web's `truncatePath`; chars approximate px since the bar is sans).
        let budget = ((self.view_size.width * 0.5 / 6.5) as usize).max(12);
        let name = text(crate::labels::truncate_path(&self.session.buffer.label, budget))
            .size(13)
            .color(theme::NORD4)
            .font(
                // A transient (preview) buffer slants the file label, like the other clients.
                if self.session.buffer.transient {
                    SANS_ITALIC
                } else {
                    SANS
                },
            );
        left = left.push(name);
        // Git cluster: `⎇  branch  +u(s) ~u(s) -u(s)` — per-class counts combine unstaged with
        // the staged count in parens, each omitted when zero.
        if let Some(gs) = self
            .session
            .window
            .as_ref()
            .and_then(|w| w.git_status.as_ref())
        {
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
                    right = right.push(t(
                        format!("{}/{}", s.current_index, format_total(s)),
                        theme::NORD4,
                    ));
                }
            }
        }
        if let Some(grep) = self.session.buffer.cursor.grep_position {
            right = right.push(t(
                format!("grep {}/{}", grep.current, grep.total),
                theme::NORD4,
            ));
        }
        // Diagnostic counts, as a tight cluster left of the position. Text glyphs stand in for
        // the web client's SVG icons (same forms as the TUI).
        if !self.session.diagnostics.is_empty() {
            use aether_protocol::viewport::DiagnosticSeverity as S;
            let mut diag = row![].spacing(8);
            for (n, sev) in [
                (self.session.diagnostics.errors, S::Error),
                (self.session.diagnostics.warnings, S::Warning),
                (self.session.diagnostics.infos, S::Information),
                (self.session.diagnostics.hints, S::Hint),
            ] {
                if n > 0 {
                    diag = diag.push(t(
                        format!("{} {n}", theme::diag_glyph(sev)),
                        theme::diagnostic_color(sev),
                    ));
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
            row![left, iced::widget::Space::new().width(Length::Fill), right,].width(Length::Fill),
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
            // The accent left strip is rendered the way the web does a rounded `border-left`: an
            // accent-coloured rounded base (outer) showing through a 3px left inset, with the NORD1
            // content layer (inner) covering everything else. So the strip's left corners ARE the
            // base's rounded corners — matching the rounded right corners — and the height is just
            // the content's (no `Fill` to bound).
            stack_col = stack_col.push(
                container(
                    container(
                        text(toast.message.clone())
                            .size(13)
                            .font(SANS)
                            .color(theme::NORD4),
                    )
                    .padding([6, 12])
                    .style(|_| container::Style {
                        background: Some(theme::NORD1.into()),
                        // Square against the accent strip on the left; rounded on the right (just
                        // inside the 1px border, so ~3) to sit within the base's rounded corners.
                        border: iced::Border {
                            radius: iced::border::Radius {
                                top_left: 0.0,
                                bottom_left: 0.0,
                                top_right: 3.0,
                                bottom_right: 3.0,
                            },
                            ..iced::Border::default()
                        },
                        ..container::Style::default()
                    }),
                )
                // Reveal a 3px accent strip down the left; the content is flush on the other sides.
                .padding(iced::Padding {
                    left: 3.0,
                    ..iced::Padding::ZERO
                })
                .style(move |_| container::Style {
                    background: Some(accent.into()),
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

/// A filter chip for the search bar's match options — same look as the grep picker's chips
/// (`picker::chip_el`): compact label on a raised NORD2 background, NORD8 text, the whole-word chip
/// underlined; the keyboard-selected chip inverts (NORD8 background, NORD0 text). Chips are
/// keyboard-driven (Left/Right select, Backspace removes, Enter cycles), so this is non-interactive.
fn option_chip<'a>(chip: &crate::chips::Chip, selected: bool) -> Element<'a, Message> {
    let underline = matches!(chip.id, crate::chips::ChipId::Word);
    let (bg, fg) = if selected {
        (theme::NORD8, theme::NORD0)
    } else {
        (theme::NORD2, theme::NORD8)
    };
    let spans: Vec<iced::widget::text::Span<'a>> = vec![iced::widget::span(chip.label.clone())
        .size(12)
        .font(SANS)
        .color(fg)
        .underline(underline)];
    container(iced::widget::rich_text(spans))
        .padding([1, 7])
        .style(move |_| container::Style {
            background: Some(bg.into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        })
        .into()
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
/// Monospace, for the help dialog's key-chord column.
const MONO: iced::Font = iced::Font::MONOSPACE;

/// A controlled overlay text field: an `iced::widget::text_input` whose value is the core's
/// current text (so a core-driven reset — clearing the search query on Esc, seeding save-as —
/// flows straight into the widget) and whose edits sync back via [`Message::OverlayInput`].
///
/// Styled to sit transparently on the overlay panel: NORD6 value, NORD8 caret/selection, a dim
/// NORD3_BRIGHT placeholder, no border or background of its own (the surrounding container draws
/// the box). `on_submit` is deliberately left unset so a single-line `text_input` lets Enter
/// bubble (`Ignored`) to the core's key handler — the picker's Enter-to-select, save-as accept,
/// and workspace-settings rename/add all stay on the existing `on_key` path.
///
/// `iced::widget::text_input`'s builder requires `Message: Clone`, which the app's `Message` is
/// not, so it's built in the tiny `Clone` [`Typed`] space and `.map`'d to `Message` (the same
/// indirection the picker/prompt overlays use for their Clone-only button messages).
fn overlay_input<'a>(field: OverlayField, placeholder: &str, value: &str) -> Element<'a, Message> {
    // `alt_passthrough` keeps Alt-chords (the nav idiom) out of the input — winit delivers
    // `Alt+letter` as text on some platforms, which a focused `text_input` would otherwise insert.
    crate::alt_filter::alt_passthrough(
        iced::widget::text_input(placeholder, value)
            .id(field.id())
            .on_input(Typed)
            .font(SANS)
            .size(13)
            .padding(0)
            .style(|_theme, _status| iced::widget::text_input::Style {
                background: iced::Background::Color(iced::Color::TRANSPARENT),
                border: iced::Border::default(),
                icon: theme::NORD6,
                placeholder: theme::NORD3_BRIGHT,
                value: theme::NORD6,
                selection: theme::NORD8,
            }),
    )
    .map(move |Typed(s)| Message::OverlayInput(field, s))
}

/// The `Clone` carrier for an overlay `text_input`'s typed value — `text_input` requires a
/// `Clone` message, so [`overlay_input`] builds in this space then maps to `Message`.
#[derive(Debug, Clone)]
struct Typed(String);

/// The search query input's `Clone` message space: typed text, or a chip-boundary key intercepted
/// before the input (Left/Backspace at the query start → step into the option-chip row). Mapped to
/// `Message` after building (`Message` isn't `Clone`, which `text_input` requires).
#[derive(Debug, Clone)]
enum SearchInputMsg {
    Typed(String),
    CoreKey(KeyCode),
}

/// Help-dialog tabs, in display order. Indexes `App::help`.
const HELP_TABS: [&str; 4] = ["Normal", "Insert", "Search", "Application"];

fn pump(notifications: NotifRx) -> Task<Message> {
    Task::perform(
        async move { notifications.lock().await.recv().await },
        Message::Notified,
    )
}

/// A chip-editor boundary key (intercepted before its `text_input`, see `picker::PickerMsg::CoreKey`)
/// reissued as a `Message::Key` so it runs through the core keymap exactly as if the key subscription
/// had forwarded it. No modifiers; a `Char` carries its text.
fn core_key_message(code: KeyCode) -> Message {
    let text = match code {
        KeyCode::Char(c) => Some(c.to_string()),
        _ => None,
    };
    Message::Key {
        code,
        mods: Mods::NONE,
        text,
    }
}

fn loaded_rows(window: &Window) -> u32 {
    window.lines.iter().map(grid::line_rows).sum()
}

/// Where the hover popover hangs relative to the cursor line: `Top(y)` puts its top edge at `y`
/// (hangs down — below the line, or clamped to the top edge); `Bottom(y)` puts its bottom edge at
/// `y` (hangs up — above the line, or clamped to the bottom edge).
enum HoverPlace {
    Top(f32),
    Bottom(f32),
}

// ---- hover Markdown rendering (the shared AST → iced widgets) ----------------------------------
//
// Renders `aether_client::markdown` directly, so the native client matches the web (Nord0 code
// blocks, accent inline code with no background, white headings, underlined links). Sizes/spacing
// mirror the web client's CSS.

const MD_TEXT: f32 = 13.0;
const MD_CODE: f32 = 12.0;
const MD_SPACING: f32 = 11.0;

/// Render the hover Markdown AST: a column of block elements. Everything is cloned, so the result
/// doesn't borrow the AST (`'static`).
fn md_doc(blocks: &[MdBlock]) -> Element<'static, Message> {
    let mut col = column![].spacing(MD_SPACING);
    for b in blocks {
        col = col.push(md_block(b));
    }
    col.into()
}

fn md_block(b: &MdBlock) -> Element<'static, Message> {
    match b {
        MdBlock::Heading { level, content } => {
            let size = match level {
                1 => 16.0,
                2 => 15.0,
                3 => 14.0,
                _ => MD_TEXT,
            };
            md_rich(content, true, theme::NORD6, size)
        }
        MdBlock::Paragraph { content } => md_rich(content, false, theme::NORD4, MD_TEXT),
        MdBlock::Code { code, .. } => container(
            text(code.clone())
                .font(iced::Font::MONOSPACE)
                .size(MD_CODE)
                .color(theme::NORD4),
        )
        .width(Length::Fill)
        .padding([6, 8])
        .style(|_| container::Style {
            background: Some(theme::NORD0.into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        })
        .into(),
        MdBlock::List { ordered, items } => {
            let mut col = column![].spacing(MD_SPACING * 0.5);
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", i + 1)
                } else {
                    "•".to_string()
                };
                col = col.push(
                    row![text(marker).size(MD_TEXT).color(theme::NORD4), md_doc(item),].spacing(6),
                );
            }
            col.into()
        }
        MdBlock::Quote { content } => row![md_bar(), md_doc(content)].spacing(8).into(),
        MdBlock::Rule => container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(1)
            .style(md_bar_style)
            .into(),
    }
}

/// A thin Nord3 bar (the blockquote rule / horizontal rule fill).
fn md_bar() -> Element<'static, Message> {
    container(iced::widget::Space::new())
        .width(2)
        .height(Length::Fill)
        .style(md_bar_style)
        .into()
}

fn md_bar_style(_: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(theme::NORD3.into()),
        ..container::Style::default()
    }
}

/// A `rich_text` of the inline AST. `bold`/`base_color` seed the styling (headings pass bold +
/// white); code and link spans override colour, and links also get an underline + click handler.
fn md_rich(
    inlines: &[MdInline],
    bold: bool,
    base_color: iced::Color,
    size: f32,
) -> Element<'static, Message> {
    let mut spans = Vec::new();
    md_spans(inlines, bold, false, None, base_color, &mut spans);
    iced::widget::rich_text(spans)
        .size(size)
        .on_link_click(Message::OpenLink)
        .into()
}

fn md_spans(
    inlines: &[MdInline],
    bold: bool,
    italic: bool,
    link: Option<&str>,
    base: iced::Color,
    out: &mut Vec<iced::advanced::text::Span<'static, String>>,
) {
    for inl in inlines {
        match inl {
            MdInline::Text { text } => out.push(md_span(text, bold, italic, false, link, base)),
            MdInline::Code { text } => out.push(md_span(text, bold, italic, true, link, base)),
            MdInline::Strong { content } => md_spans(content, true, italic, link, base, out),
            MdInline::Emphasis { content } => md_spans(content, bold, true, link, base, out),
            MdInline::Link { href, content } => {
                md_spans(content, bold, italic, Some(href), base, out)
            }
        }
    }
}

fn md_span(
    text: &str,
    bold: bool,
    italic: bool,
    code: bool,
    link: Option<&str>,
    base: iced::Color,
) -> iced::advanced::text::Span<'static, String> {
    let font = if code {
        iced::Font::MONOSPACE
    } else {
        iced::Font {
            weight: if bold {
                iced::font::Weight::Bold
            } else {
                iced::font::Weight::Normal
            },
            style: if italic {
                iced::font::Style::Italic
            } else {
                iced::font::Style::Normal
            },
            ..iced::Font::default()
        }
    };
    let color = if link.is_some() {
        theme::NORD9
    } else if code {
        theme::NORD8
    } else {
        base
    };
    let s = iced::widget::span(text.to_string()).font(font).color(color);
    match link {
        Some(href) => s.link(href.to_string()).underline(true),
        None => s.link_maybe(None::<String>),
    }
}

/// Estimate the rendered height (wrapped rows) of the AST, for the place-above-or-below decision.
fn md_estimate(blocks: &[MdBlock]) -> usize {
    blocks.iter().map(md_estimate_block).sum()
}

fn md_estimate_block(b: &MdBlock) -> usize {
    match b {
        MdBlock::Heading { content, .. } | MdBlock::Paragraph { content } => {
            1 + md_text_len(content) / 80
        }
        MdBlock::Code { code, .. } => code.lines().count().max(1) + 1,
        MdBlock::List { items, .. } => items.iter().map(|it| md_estimate(it)).sum::<usize>().max(1),
        MdBlock::Quote { content } => md_estimate(content),
        MdBlock::Rule => 1,
    }
}

fn md_text_len(inlines: &[MdInline]) -> usize {
    inlines
        .iter()
        .map(|i| match i {
            MdInline::Text { text } | MdInline::Code { text } => text.len(),
            MdInline::Strong { content }
            | MdInline::Emphasis { content }
            | MdInline::Link { content, .. } => md_text_len(content),
        })
        .sum()
}

/// Open a hover-link URL in the OS's default handler. Restricted to web/mail/file schemes so an
/// LSP-supplied link can't run an arbitrary command via the shell-out.
fn open_link(url: &str) {
    if !["http://", "https://", "mailto:", "file:"]
        .iter()
        .any(|p| url.starts_with(p))
    {
        return;
    }
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .arg(url)
        .spawn();
}

/// The hover popover's scrollable id, for programmatic `scroll_by` (keyboard panning).
fn hover_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("hover-scroll")
}

fn help_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("help-scroll")
}

/// One help section: an uppercase group heading over its `keys → desc` rows (key column monospace,
/// matching the web's `.help-section`). Mirrors the web layout in iced.
fn help_section(group: &str, rows: Vec<(String, &'static str)>) -> Element<'static, Message> {
    let mut col = column![text(group.to_uppercase())
        .size(11)
        .font(SANS)
        .color(theme::NORD13)]
    .spacing(2);
    for (keys, desc) in rows {
        col = col.push(
            row![
                text(keys)
                    .size(12)
                    .font(MONO)
                    .color(theme::NORD8)
                    .width(Length::FillPortion(2)),
                text(desc)
                    .size(13)
                    .font(SANS)
                    .color(theme::NORD4)
                    .width(Length::FillPortion(3)),
            ]
            .spacing(8),
        );
    }
    col.into()
}

/// Vertical scroll delta (px) for a resolved popover [`HoverAction::Scroll`]: a line is one cell
/// height, half/page use the popover's max content height (its `max_height` less padding) as the
/// page proxy — mirroring the editor's scroll units.
fn hover_scroll_px(dir: ScrollDir, unit: ScrollUnit, cell: Option<Size>) -> f32 {
    const PAGE: f32 = 360.0;
    let mag = match unit {
        ScrollUnit::Line => cell.map_or(18.0, |c| c.height),
        ScrollUnit::Half => PAGE / 2.0,
        ScrollUnit::Page => PAGE,
    };
    if matches!(dir, ScrollDir::Down) {
        mag
    } else {
        -mag
    }
}

/// Scroll the picker's results list so the highlighted row is in view. `Minimal` moves the
/// least distance; `Top` aligns the row to the top unless it's already fully visible.
fn reveal_picker_selection(p: &PickerState, scroll_y: &mut f32, reveal: Reveal) -> Task<Message> {
    let Some(y) = reveal_target(p, *scroll_y, reveal) else {
        return Task::none();
    };
    *scroll_y = y;
    iced::widget::operation::scroll_to(
        crate::picker::list_id(),
        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
    )
}

/// The scroll offset that reveals the picker's highlighted row, or `None` if it's already
/// in view. Grep hits reserve one row of clearance above (the web client's
/// `scroll-margin-top`): the sticky file header pins over the list's first visible row, so
/// a hit revealed flush to the top edge would sit hidden underneath it.
fn reveal_target(p: &PickerState, scroll_y: f32, reveal: Reveal) -> Option<f32> {
    let sd = p.selected_display_row()?;
    let top = sd as f32 * crate::picker::ROW_H;
    let bottom = top + crate::picker::ROW_H;
    // File-grouped pickers pin a sticky file header over the top row, so a revealed row must clear
    // one row's height or it slides under the header (the bug grep already hit).
    let clearance = if p.kind.groups_by_file() {
        crate::picker::ROW_H
    } else {
        0.0
    };
    let m_top = (top - clearance).max(0.0);
    let h = crate::picker::list_height(p);
    let visible = m_top >= scroll_y && bottom <= scroll_y + h;
    match reveal {
        Reveal::Top if !visible => Some(m_top),
        Reveal::Top => None,
        Reveal::Minimal if m_top < scroll_y => Some(m_top),
        Reveal::Minimal if bottom > scroll_y + h => Some(bottom - h),
        Reveal::Minimal => None,
    }
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

/// `"47"` or `"10000+"` when the server hit its match cap.
fn format_total(s: &SearchSummary) -> String {
    if s.truncated {
        format!("{}+", s.total)
    } else {
        s.total.to_string()
    }
}

/// Compose the prompt phrasing for a confirmation. The core supplies the structured reason
/// ([`ConfirmKind`]); wording is the native client's presentational choice (the dialog then
/// appends `?` and offers Yes/No).
fn confirm_phrase(kind: &ConfirmKind) -> String {
    match kind {
        ConfirmKind::Overwrite { path: Some(p) } => format!("Overwrite {p}"),
        ConfirmKind::Overwrite { path: None } => "Overwrite".into(),
        ConfirmKind::OverwriteModified => "File changed on disk — overwrite".into(),
        ConfirmKind::RecreateDeleted => "File removed on disk — recreate".into(),
        ConfirmKind::DiscardOnReload => "Discard local changes and reload".into(),
        ConfirmKind::DiscardOnClose { label } => format!("Discard unsaved changes in {label}"),
        ConfirmKind::Delete { noun, name } => format!("Delete {noun} \"{name}\""),
        ConfirmKind::RemoveRoot { path } => format!("Remove root \"{path}\""),
        ConfirmKind::DeleteWorkspace { name } => format!("Delete workspace \"{name}\""),
    }
}

fn nord_theme(_app: &App) -> iced::Theme {
    iced::Theme::Nord
}

/// Dial the daemon and bootstrap once, landing the outcome as [`Message::Booted`]. Used for the
/// initial boot attempt from the `Connecting` launch state.
fn spawn_connect(args: ConnectingBootstrap) -> Task<Message> {
    Task::perform(connect_and_bootstrap(args), Message::Booted)
}

/// Like [`spawn_connect`] but after a short delay — the retry between failed boot dials (the
/// daemon may still be coming up). Localhost dials are cheap, so a flat 500ms keeps it responsive
/// without busy-looping.
fn spawn_connect_delayed(args: ConnectingBootstrap) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            connect_and_bootstrap(args).await
        },
        Message::Booted,
    )
}

/// One boot-connect attempt: dial the fixed address, then (with a CLI workspace) activate it and
/// open the file / MRU buffer, or (without one) hand back a bare connection for the chooser.
/// Returns the connected [`Bootstrap`] to install, or an error string to retry / surface.
async fn connect_and_bootstrap(args: ConnectingBootstrap) -> Result<Bootstrap, String> {
    let base_url = args.server_url.clone();
    let (handle, rx) = crate::connection::connect(&base_url, &args.client_version)
        .await
        .map_err(|e| e.to_string())?;
    let notifications = std::sync::Arc::new(tokio::sync::Mutex::new(rx));

    // No workspace on the CLI. An existing file outside any configured workspace (`ae /etc/hosts`)
    // opens directly in an ephemeral "(no workspace)" context; otherwise hand back the bare
    // connection so the chooser browses on it.
    let Some(workspace) = args.workspace.clone() else {
        let resolved = match &args.file {
            Some(f) => Some(resolve_cli_path(f)?),
            None => None,
        };
        if let Some(abs) = resolved.filter(|p| p.is_file()) {
            let opened = handle
                .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                    path: abs.display().to_string(),
                    transient: None,
                })
                .await
                .map_err(|e| e.to_string())?;
            let workspace_paths = opened.workspace.paths.clone();
            let open = opened
                .opened
                .ok_or_else(|| "workspace/open_path returned no buffer".to_string())?;
            return Ok(Bootstrap::Session(Box::new(SessionBootstrap {
                handle,
                notifications,
                client_version: args.client_version,
                server_url: args.server_url,
                server_started_at: opened.server_started_at,
                workspace: opened.workspace.name,
                buffer: buffer_info(open, &workspace_paths),
                workspace_paths,
                explorer_dir: None,
                launched_with_file: true,
            })));
        }
        return Ok(Bootstrap::Choose(ChooseBootstrap {
            handle,
            notifications,
            client_version: args.client_version,
            server_url: args.server_url,
            server_started_at: 0,
        }));
    };

    let activated = handle
        .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
            name: workspace,
            open_last: false,
        })
        .await
        .map_err(|e| e.to_string())?;
    let server_started_at = activated.server_started_at;
    let workspace_paths = activated.workspace.paths.clone();

    // Resolve the CLI path once, then branch on file vs directory. A directory lands in a
    // transient scratch and opens the file explorer over it (`explorer_dir`, run once the session
    // installs); a file opens normally.
    let resolved = match &args.file {
        Some(f) => Some(resolve_cli_path(f)?),
        None => None,
    };

    let open = match &resolved {
        Some(abs) if abs.is_dir() => handle
            .rpc::<BufferOpen>(BufferOpenParams {
                transient: Some(true),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?,
        Some(abs) => {
            let abs_str = abs.display().to_string();
            match strip_longest_root(&abs_str, &workspace_paths) {
                // Inside a workspace root: ordinary workspace-relative open.
                Some((path_index, relative_path)) => handle
                    .rpc::<BufferOpen>(BufferOpenParams {
                        path_index: Some(path_index),
                        relative_path: Some(relative_path),
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| e.to_string())?,
                // Outside the named workspace's roots: open as an external (guest) buffer in it.
                None => handle
                    .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                        path: abs_str,
                        transient: None,
                    })
                    .await
                    .map_err(|e| e.to_string())?
                    .opened
                    .ok_or_else(|| "workspace/open_path returned no buffer".to_string())?,
            }
        }
        // No file: attach to the most recent buffer, or a transient scratch placeholder.
        None => handle
            .rpc::<BufferOpen>(BufferOpenParams {
                buffer_id: activated.last_buffer_id,
                transient: activated.last_buffer_id.is_none().then_some(true),
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?,
    };

    let explorer_dir = match &resolved {
        Some(abs) if abs.is_dir() => Some(abs.display().to_string()),
        _ => None,
    };

    Ok(Bootstrap::Session(Box::new(SessionBootstrap {
        handle,
        notifications,
        client_version: args.client_version,
        server_url: args.server_url,
        server_started_at,
        workspace: activated.workspace.name,
        buffer: buffer_info(open, &workspace_paths),
        workspace_paths,
        explorer_dir,
        launched_with_file: false,
    })))
}

/// Resolve a CLI path against the current working directory (shell-conventional).
fn resolve_cli_path(input: &str) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(input);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| e.to_string())?.join(p)
    };
    abs.canonicalize()
        .map_err(|e| format!("resolving {}: {e}", abs.display()))
}

/// Run the iced application. `main` hands it a `Connecting` bootstrap — the app dials from within
/// and renders an immersive "Connecting…" state until the daemon answers.
pub fn run(bootstrap: Bootstrap) -> iced::Result {
    iced::application(move || App::new(bootstrap.clone()), App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        // Everything we draw sets explicit Nord colours, but theme-inheriting surfaces (markdown
        // hover body text, scrollbars) must not default to the Light theme.
        .theme(nord_theme)
        // The buffer's font + size (chrome sets explicit fonts/sizes): web's 14px monospace.
        .settings(iced::Settings {
            // Bundle Fira Code for the editor (chrome stays on the default monospace). Registered
            // here so `Font::with_name("Fira Code")` resolves; the editor toggles its ligatures via
            // shaping mode (see `editor::EDITOR_FONT`).
            fonts: vec![include_bytes!("../fonts/FiraCode-Regular.ttf")
                .as_slice()
                .into()],
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
    use crate::picker::ROW_H;

    /// A grep window: rows [0]=hdr a.rs, [1..=3]=hits, [4]=hdr b.rs, [5..=24]=hits.
    fn grep_state() -> PickerState {
        let hit = |path: &str, line: u32| PickerItem::GrepHit {
            path_index: 0,
            relative_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::Grep);
        let mut items: Vec<_> = (1..=3).map(|l| hit("a.rs", l)).collect();
        items.extend((1..=20).map(|l| hit("b.rs", l)));
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Grep,
            generation: 0,
            offset: 0,
            items: Some(items),
            total_matches: 23,
            total_candidates: 23,
            ticking: false,
            grep_display_offset: Some(0),
            grep_total_display_rows: Some(25),
            center_on: None,
            explorer_peek_missing: false,
        }));
        s
    }

    /// Moving up to the first visible row must scroll one extra row: the sticky file header
    /// pins over that row, so flush-to-the-top means hidden (web's `scroll-margin-top`).
    #[test]
    fn grep_reveal_clears_the_sticky_header() {
        let mut s = grep_state();
        // Scrolled so display row 6 (a b.rs hit) is first visible, pinned header over it.
        let scroll = 6.0 * ROW_H;
        s.selected = 4; // display row 6 — the first visible row, pinned header over it
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Minimal),
            Some(5.0 * ROW_H),
            "selection on the pinned-over first row needs a one-row scroll"
        );
        // One row below the top edge is genuinely visible — no scroll.
        s.selected = 5; // display row 7
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal), None);
        // Top-aligned reveals (grep file jumps) leave the same clearance.
        s.selected = 22; // display row 24 — below the 18-row viewport (rows 6..24)
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Top),
            Some(23.0 * ROW_H),
            "the row aligns with its clearance row at the top"
        );
        // The first hit of the list reveals to 0 — its real header row is above it.
        s.selected = 0; // display row 1
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal), Some(0.0));
    }

    /// Non-grep pickers have no headers: the first row is revealed flush to the top.
    #[test]
    fn plain_reveal_needs_no_clearance() {
        let mut s = PickerState::new(PickerKind::Workspaces);
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Workspaces,
            generation: 0,
            offset: 0,
            items: Some(
                (0..30)
                    .map(|i| PickerItem::Workspace {
                        name: format!("p{i}"),
                        unsaved_buffers: 0,
                        match_indices: vec![],
                    })
                    .collect(),
            ),
            total_matches: 30,
            total_candidates: 30,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        let scroll = 6.0 * ROW_H;
        s.selected = 6; // first visible row — visible as-is
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal), None);
        s.selected = 5;
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Minimal),
            Some(5.0 * ROW_H)
        );
    }

    /// Every overlay field maps to a distinct, stable widget id — the focus task and the
    /// `text_input`'s own `.id()` must agree, or focus would never land.
    #[test]
    fn overlay_field_ids_are_distinct() {
        use std::collections::HashSet;
        let fields = [
            OverlayField::PickerQuery,
            OverlayField::Search,
            OverlayField::SaveAs,
            OverlayField::SaveAsRoot,
            OverlayField::WorkspaceName,
            OverlayField::WorkspaceAddRoot,
            OverlayField::ChipRoot,
            OverlayField::ChipPath,
        ];
        let ids: HashSet<_> = fields.iter().map(|f| f.id()).collect();
        assert_eq!(ids.len(), fields.len(), "overlay field ids must be unique");
        // The id is stable across calls (so re-focus targets the same widget).
        assert_eq!(OverlayField::Search.id(), OverlayField::Search.id());
    }

    /// The picker query `text_input`'s id (set in `picker.rs`) must equal the shell's focus
    /// target id, or opening the picker wouldn't focus its query input.
    #[test]
    fn picker_query_id_matches_focus_target() {
        assert_eq!(
            crate::picker::query_input_id(),
            OverlayField::PickerQuery.id()
        );
    }

    /// The chip-editor inputs' ids (set in `picker.rs`) must equal the shell's focus target ids,
    /// or `sync_focus` would never land on the active chip-editor segment.
    #[test]
    fn chip_editor_ids_match_focus_targets() {
        assert_eq!(crate::picker::editor_root_id(), OverlayField::ChipRoot.id());
        assert_eq!(crate::picker::editor_path_id(), OverlayField::ChipPath.id());
    }
}
