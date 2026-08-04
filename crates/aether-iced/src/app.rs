//! Application state and message loop.
//!
//! Mirrors the TUI's `app.rs` in miniature, restructured for iced's architecture: key events
//! resolve through `keymap` to `Action`s, actions become RPC `Task`s, and responses /
//! server notifications come back as `Message`s that update state. The scroll model is the web
//! client's: a pixel offset into the full document height, with window fetches when the view
//! nears the loaded range's edge.

use crate::connection::Handle;
use crate::connection::NotifRx;
pub use crate::core::effect::{
    Effect, Effects, RevealStyle, ShellAction, ToastKind, WindowOpen, WindowTarget,
};
use crate::core::markdown::{AlertKind, Block as MdBlock, Inline as MdInline, Span as MdSpan};
pub use crate::core::session::*;
use crate::core::update::Event as CoreEvent;
use crate::editor::{self, ClickKind, EditorEvent, GUTTER_COLS, PAD};
use crate::grid;
use crate::keymap::{
    hover_action, HoverAction, KeyCode, Mods, ScrollDir, ScrollUnit, ViewportPlace,
    CURSOR_REST_FRACTION,
};
use crate::picker::{PickerMsg, PickerState, Reveal};
use crate::theme;
use aether_protocol::buffer::{BufferOpen, BufferOpenParams, BufferOpenResult};
use aether_protocol::cursor::Granularity;
use aether_protocol::envelope::RpcMethod;
use aether_protocol::git::{GitBlameLine, GitBlameLineParams};
use aether_protocol::lsp::LspStatus;
use aether_protocol::picker::PickerKind;
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{
    ScrollPosition, ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams,
    ViewportScrollToRow, ViewportScrollToRowParams, ViewportSetWrap, ViewportSetWrapParams,
    ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult, ViewportWindowResult,
    Window, WrapMode,
};
use aether_protocol::workspace::{
    WorkspaceActivate, WorkspaceActivateParams, WorkspaceInfo, WorkspaceOpenPath,
    WorkspaceOpenPathParams,
};
use aether_protocol::{BufferId, LogicalPosition};
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
    /// A location (0-based line/col) to jump to once the CLI `file` opens (`ae src/main.rs:42:10`,
    /// and the grep-hit "open in new window"). Applies only to a file *inside* a workspace root —
    /// an external open goes through `workspace/open_path`, which carries no jump. `None` for a bare
    /// file open.
    pub jump_to: Option<LogicalPosition>,
    /// Re-open an existing buffer by id instead of a file — the scratch-buffer "open in new window"
    /// (`--buffer <id>`). Takes precedence over `file`; the id is daemon-session scoped, so a stale
    /// one falls back to the workspace's MRU/scratch.
    pub buffer_id: Option<BufferId>,
    /// Tether the client to the buffer `file` opens (docs/tether.md): the quick-edit invocation —
    /// a file positional without an explicit `--workspace` — where closing that buffer exits the
    /// window. Window-spawns ([`spawn_target`]) always name the workspace, so they never set it.
    pub tether: bool,
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
    /// The activation's `WorkspaceInfo` — name, roots and declared projects. Carried whole rather
    /// than field-by-field: boot is the one path that seeds a session without a
    /// `sync_workspace_info`, so anything dropped here stays missing for the client's lifetime.
    pub workspace: aether_protocol::workspace::WorkspaceInfo,
    pub buffer: BufferInfo,
    /// Set when the CLI path was a directory: the absolute dir to open the file explorer at,
    /// over the transient scratch in `buffer`. `None` for the file / no-path cases.
    pub explorer_dir: Option<String>,
    /// The session was launched to quick-edit `buffer` (`ae file`): tether the client to it, so
    /// closing that buffer exits the window (see `Session::tether`, docs/tether.md).
    pub tethered: bool,
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

impl From<crate::connection::ConnectError> for ReconnectError {
    /// A version mismatch is terminal — the running daemon is a different build, so retrying can't
    /// help; surface the message. Any other dial failure is just "not up yet", so retry.
    fn from(e: crate::connection::ConnectError) -> Self {
        use crate::connection::ConnectError;
        match e {
            ConnectError::VersionMismatch(m) => ReconnectError::Fatal(m),
            ConnectError::Down(_) => ReconnectError::NotUp,
        }
    }
}

/// Why a boot dial didn't produce a session. Mirrors [`ReconnectError`] for the initial-boot path,
/// which merges connect failures with bootstrap failures (`String`) — so `?` on a bootstrap error
/// folds into [`BootError::Retry`] via `From<String>`, and only a version mismatch is `Fatal`.
#[derive(Debug)]
pub enum BootError {
    /// Daemon not up yet, or a bootstrap RPC hiccuped — keep dialing.
    Retry(String),
    /// A version mismatch (a stale daemon holds the port) — surface it and stop retrying.
    Fatal(String),
}

impl From<String> for BootError {
    fn from(s: String) -> Self {
        BootError::Retry(s)
    }
}

impl From<crate::connection::ConnectError> for BootError {
    fn from(e: crate::connection::ConnectError) -> Self {
        use crate::connection::ConnectError;
        match e {
            ConnectError::VersionMismatch(m) => BootError::Fatal(m),
            ConnectError::Down(e) => BootError::Retry(e.to_string()),
        }
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
    /// The delete button on project row `index` (0-based) was clicked.
    RemoveProject(usize),
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
    /// The workspace-settings add-project path segment.
    WorkspaceAddProject,
    /// Its leading root-typeahead segment (multi-root workspaces).
    WorkspaceAddProjectRoot,
    /// Its trailing language-typeahead segment.
    WorkspaceAddProjectLanguage,
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
            OverlayField::WorkspaceAddProject => "overlay-workspace-addproject",
            OverlayField::WorkspaceAddProjectRoot => "overlay-workspace-addproject-root",
            OverlayField::WorkspaceAddProjectLanguage => "overlay-workspace-addproject-language",
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
#[derive(Clone, Copy)]
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
    /// The boot-connect dial resolved (from the `Connecting` launch state): either a connected
    /// `Session`/`Choose` bootstrap to install, or a failure to retry (or, for a version mismatch,
    /// to surface and stop).
    Booted(Result<Bootstrap, BootError>),
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
    /// A click on a reading-view block/item: focus it — the source byte is the clicked
    /// element's span start, and the core turns it into the cursor move focus derives from
    /// (docs/markdown-view.md §2.3).
    ReadClick(u32),
    /// A click that landed on a rendered link / footnote-ref / image chip (the
    /// [`READ_ARM_PREFIX`] sentinel): focus it AND follow it like `Enter` — the core keeps
    /// images arm-only. Ctrl-click opens a relative link in a new window (the pointer
    /// sibling of `Ctrl-Enter`, matching the picker rows).
    ReadClickActivate(u32),
    /// A remote reading-view image download resolved: raw bytes (sniffed raster-vs-SVG on
    /// receipt) or an error, keyed by URL (docs/markdown-view.md §2.8).
    RemoteImageFetched(String, Result<Vec<u8>, String>),
    /// The [`ReadRevealProbe`] measured the focused block: `Some(offset)` = scroll the read
    /// view there; `None` = already comfortably visible.
    ReadRevealMeasured(Option<f32>),
    /// The read scrollable scrolled (any cause — our glide ticks or the user's wheel): its
    /// new offset and scroll range, mirrored for targeting/clamping.
    ReadScrolled {
        y: f32,
        max: f32,
    },
    Noop,
    /// Frame tick while a smooth scroll is in flight.
    AnimTick(std::time::Instant),
    /// Periodic hint tick (docs/hints.md): stamps the wall clock into the core's hint
    /// engine, which runs the corner hint's display timer. Subscribed only while a session with
    /// hints enabled is connected.
    HintTick,
    Subscribed(Result<ViewportSubscribeResult, String>),
    WindowUpdate(Result<ViewportWindowResult, String>),

    /// A core event (docs/client-core.md): forwarded to `Session::on_event`, whose effects
    /// the shell executes. Grows a subsystem at a time as update logic migrates into core.
    Core(CoreEvent),
    /// Keyboard modifier state changed — stashed in `App::modifiers` for click-time reads (Ctrl-click).
    ModifiersChanged(keyboard::Modifiers),
    /// The picker's jumplist scrolled natively (absolute y in px).
    PickerScrolled(f32),
    /// Pointer entered (`Some(abs)`) or left (`None`-if-still-current, see mapping) a row.
    PickerHovered(Option<u32>),
    PickerUnhovered(u32),
    Notified(Option<aether_protocol::envelope::Notification>),
    /// A reconnect attempt resolved (the backoff sleep rides inside the attempt task).
    Reconnected(Result<Box<Reestablished>, ReconnectError>),
}

pub struct App {
    /// Set while the app is in the boot-connecting state (`ConnState::Connecting`): the CLI args
    /// the dial task needs, retained so a failed attempt can retry. Cleared the moment a
    /// connection lands and the real session/chooser is installed. While `Some`, input is parked
    /// and the immersive "Connecting…" backdrop shows.
    boot_args: Option<ConnectingBootstrap>,
    /// How many boot dials have failed so far — paces the retry delay (`boot_backoff`). Reset
    /// when a connection lands.
    boot_attempt: u32,
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
    /// Live keyboard modifier state, kept current from `ModifiersChanged` in every phase. Read at
    /// click time for Ctrl-click on picker rows — iced's `mouse_area::on_press` carries no modifiers.
    modifiers: keyboard::Modifiers,
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
    /// The picker jumplist's scroll offset in px. The core tracks rows, not pixels; resets
    /// arrive as `Effect::PickerScrollReset`.
    picker_scroll_y: f32,
    /// The reading-view focus last revealed (`(buffer, span.start, span.end)`), so the document
    /// scrolls only when the focus *changes* (docs/markdown-view.md §2.8).
    read_last_focus: Option<(u64, u32, u32)>,
    /// The pending reveal is a *placement* — the first into a freshly-appeared document (a
    /// cross-file landing, or the reading view just opening) — so it snaps instead of gliding
    /// (the editor's cross-buffer jump contract, [`RevealStyle::Jump`]). Armed at the
    /// focus-change trigger, consumed by `ReadRevealMeasured`.
    read_reveal_snap: bool,
    /// A pending click-focus target (span start): when the focus change lands on it, the reveal
    /// snap is skipped — the clicked element was visible, so scrolling would only jolt.
    read_click_target: Option<u32>,
    /// The read scrollable's current offset (mirrored from its `on_scroll` — the widget owns
    /// the truth) and the scroll range, for smooth-scroll targeting and clamping.
    read_scroll_px: f32,
    read_scroll_max: Option<f32>,
    /// The read view's glide (the editor's [`ScrollAnim`], driving `scroll_to` per tick).
    read_scroll_anim: Option<ScrollAnim>,
    /// The offset the read glide last emitted — an `on_scroll` that deviates is user input
    /// (wheel/drag), which snaps the glide off, like the editor.
    read_anim_last: f32,
    /// Remote (http/https) reading-view images by URL, fetched once per session
    /// (docs/markdown-view.md §2.8). `Loading`/`Failed` render placeholders.
    remote_images: std::collections::HashMap<String, RemoteImage>,
    /// The `(buffer, revision)` last scanned for remote images, so the fetch fan-out runs once
    /// per parse rather than per frame.
    read_remote_scan: Option<(u64, u64)>,
    /// The picker search throbber's rotation (radians), advanced from frame ticks while a search is
    /// in progress, with the time of the last tick so the step is frame-rate independent.
    spinner_phase: f32,
    last_anim_tick: Option<std::time::Instant>,
    /// The hover popover (hover info / diagnostics-at-cursor / commit details), anchored at
    /// the cursor; holds *parsed* iced markdown. Dismissed by any key, click, or scroll.
    hover: Option<HoverContent>,
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
        let shell = |session: Session,
                     handle: Handle,
                     notifications: NotifRx,
                     client_version: String,
                     server_url: String,
                     server_started_at: u64| App {
            boot_args: None,
            boot_attempt: 0,
            session,
            handle,
            notifications,
            client_version,
            server_url,
            server_started_at,
            cell: None,
            view_size: Size::ZERO,
            modifiers: keyboard::Modifiers::default(),
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
            read_last_focus: None,
            read_reveal_snap: false,
            read_click_target: None,
            read_scroll_px: 0.0,
            read_scroll_max: None,
            read_scroll_anim: None,
            read_anim_last: 0.0,
            remote_images: std::collections::HashMap::new(),
            read_remote_scan: None,
            spinner_phase: 0.0,
            last_anim_tick: None,
            hover: None,
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
                let tether = b.tethered.then_some(b.buffer.buffer_id);
                let mut session = Session::new(b.workspace, b.buffer);
                session.tether = tether;
                // Fetch persisted app settings (e.g. the soft-wrap default) as the session comes up.
                let startup = session.startup();
                let mut app = shell(
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
                // The core Workspaces picker over a placeholder session — the same chooser the
                // TUI/web shells boot into. Picking a workspace activates it and the session
                // adopts in place (`PickerSelected` → `WorkspaceActivated` → `adopt_switch`).
                let pump = pump(b.notifications.clone());
                let mut app = shell(
                    Session::placeholder(),
                    b.handle,
                    b.notifications,
                    b.client_version,
                    b.server_url,
                    b.server_started_at,
                );
                let chooser =
                    app.session
                        .open_picker(PickerKind::Workspaces, None, None, false, None);
                // Fetch the app settings + hint snapshot on the boot connection: the chooser
                // shows the first hint a fresh install ever sees (docs/hints.md), and the engine
                // is dormant until the snapshot adopts.
                let startup = app.session.startup();
                let fx = app.run_core(chooser.and(startup));
                (app, Task::batch([pump, fx]))
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
                // Shift-Tab arrives as Tab + Shift here; the core wants it as its own key.
                let code = aether_client::keymap::apply_backtab(code, mods);
                // Forward to the core when no focused widget consumed the key (`Ignored`), PLUS two
                // forced exceptions a focused `text_input` would otherwise swallow:
                //   - `Escape` (the input captures it to unfocus itself), and
                //   - any `Alt`-chord — `Alt-j/k/l` is the app's universal navigation idiom (move
                //     between picker results / settings fields); `text_input` reports it Captured,
                //     so force it through. (The `alt_filter::alt_passthrough` wrapper around each
                //     overlay input also drops the `Alt` press before the input can insert it as
                //     text, which some platforms' winit delivers — so the field stays clean.)
                //   - `Tab` / `Shift-Tab`, which are field traversal (the settings dialogs, the
                //     path editor's segments). A focused `text_input` captures them and iced then
                //     moves *widget* focus on its own — which drifts away from the core's idea of
                //     the focused row and leaves keys landing on a field the dialog doesn't think
                //     is selected. Forcing them through keeps the core authoritative.
                let forward = status == iced::event::Status::Ignored
                    || code == KeyCode::Esc
                    || code == KeyCode::Tab
                    || code == KeyCode::BackTab
                    || mods.alt;
                forward.then(|| Message::Key {
                    code,
                    mods,
                    text: text.map(|t| t.to_string()),
                })
            }
            // Track modifier state for click-time reads (Ctrl-click on picker rows). `ModifiersChanged`
            // self-heals on focus loss, so the state can't get stuck held.
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            _ => None,
        });
        let mut subs = vec![keys];
        // Frame ticks drive the scroll easing and the picker's search throbber; subscribe to them
        // only while one of those is actually animating — and never while disconnected, where a
        // picker throbber stuck mid-search (the server stopped answering) would otherwise pin the
        // 60fps redraw loop for the whole reconnect window.
        let animating =
            self.scroll_anim.is_some() || self.read_scroll_anim.is_some() || self.picker_ticking();
        if animating && self.session.conn == ConnState::Connected {
            subs.push(iced::window::frames().map(Message::AnimTick));
        }
        // The hint engine's clock (docs/hints.md): a slow tick while a session with hints on is
        // connected — including the boot chooser, which is just the Workspaces picker over a
        // placeholder session. The engine's own idle gate handles unattended windows, so this
        // needs no focus tracking; with hints off there are no wakeups at all.
        if self.session.hints_enabled && self.session.conn == ConnState::Connected {
            subs.push(
                iced::time::every(std::time::Duration::from_secs(2)).map(|_| Message::HintTick),
            );
        }
        Subscription::batch(subs)
    }

    /// Whether a picker search is still streaming (drives the throbber animation).
    fn picker_ticking(&self) -> bool {
        self.session.picker.as_ref().is_some_and(|p| p.ticking)
    }

    // ---- update ---------------------------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        // Keep live modifier state current in every phase (boot/connecting/session) — Ctrl-click on a
        // picker row reads it, and a `mouse_area` press hands over no modifiers of its own.
        if let Message::ModifiersChanged(m) = message {
            self.modifiers = m;
            return Task::none();
        }
        // Boot-connecting (no socket yet): input is parked; only the dial result moves us on.
        let task = if self.boot_args.is_some() {
            self.update_connecting(message)
        } else {
            self.update_inner(message)
        };
        // After every update, snap focus to the overlay field that should own the keyboard (web
        // parity: `ensureFocus`). Only fires a focus operation when the target *changes*, so it
        // doesn't fight the user (e.g. re-grab focus every keystroke).
        Task::batch([task, self.sync_focus()])
    }

    /// The overlay `text_input` that should hold focus right now, given session state. Mirrors the
    /// web's `focusTarget`. `None` means "no overlay field" (the editor owns the keyboard).
    fn desired_focus(&self) -> Option<OverlayField> {
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
            // The name field or one of the two add inputs — a highlighted root/project row is a
            // selection, not a text field, so there's nothing to focus there.
            return match s.row() {
                SettingsRow::Name => Some(OverlayField::WorkspaceName),
                SettingsRow::AddRoot => Some(OverlayField::WorkspaceAddRoot),
                // Two segments — focus follows whichever the editor has active.
                SettingsRow::AddProject => Some(if s.on_add_project_language {
                    OverlayField::WorkspaceAddProjectLanguage
                } else if self.session.workspace_paths.len() > 1
                    && s.add_project.field == aether_client::chips::ChipEditorField::Root
                {
                    OverlayField::WorkspaceAddProjectRoot
                } else {
                    OverlayField::WorkspaceAddProject
                }),
                SettingsRow::Root(_) | SettingsRow::Project(_) => None,
            };
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

    /// Drop to the workspace chooser over the live connection: swap in a fresh placeholder
    /// session (nothing renders behind the picker) and raise the core Workspaces picker — the
    /// same chooser the TUI/web shells use. Reached from the no-args boot (`Booted` → `Choose`),
    /// [`Effect::ToChooser`] (an ephemeral context lost its last buffer), and the workspace-gone
    /// reconnect recovery.
    fn enter_chooser(&mut self) -> Task<Message> {
        self.session = Session::placeholder(); // conn = Connected, so notifications keep flowing
        let fx = self
            .session
            .open_picker(PickerKind::Workspaces, None, None, false, None);
        self.run_core(fx)
    }

    /// Boot-connecting state (`ConnState::Connecting`): the editor chrome is live but there's no
    /// socket yet. The dial's `Booted` result installs the real session (workspace on the CLI) or
    /// chooser (no workspace), or retries after a short delay (the daemon may still be starting).
    /// Everything else flows to the normal handler so client-side keys behave as in a reconnect —
    /// the core drops any RPC while not `Connected`, so the dummy transport is never exercised.
    fn update_connecting(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Booted(Ok(Bootstrap::Session(b))) => {
                let jump_boot = self.boot_args.as_ref().is_some_and(|a| a.jump_to.is_some());
                self.boot_args = None;
                self.boot_attempt = 0;
                self.server_started_at = b.server_started_at;
                self.handle = b.handle;
                self.notifications = b.notifications.clone();
                let tether = b.tethered.then_some(b.buffer.buffer_id);
                self.session = Session::new(b.workspace, b.buffer);
                self.session.tether = tether;
                // The connecting editor already laid out (recording cell metrics) without
                // subscribing, so its Layout may not fire again — subscribe explicitly now that
                // we're Connected. `subscribe_task` is a no-op if no metrics arrived yet, and the
                // first real Layout then handles it.
                self.sent_grid = self.current_grid();
                // A directory CLI arg opens the file explorer over the scratch buffer.
                let startup = match b.explorer_dir {
                    Some(dir) => {
                        self.session
                            .open_picker(PickerKind::Explorer, Some(dir), None, false, None)
                    }
                    None => Effects::none(),
                };
                // Fetch the persisted app settings (e.g. the soft-wrap default) on this connection.
                let startup = startup.and(self.session.startup());
                // Boot installs the session directly (no `adopt_switch`), so the markdown
                // reading-view default is applied here (docs/markdown-view.md §1.6); an
                // `ae file:line` launch is jump-shaped and lands in the editor.
                let jumped = jump_boot;
                let startup = startup.and(self.session.boot_read_presentation(jumped));
                Task::batch([
                    pump(b.notifications),
                    self.subscribe_task(),
                    self.run_core(startup),
                ])
            }
            Message::Booted(Ok(Bootstrap::Choose(b))) => {
                self.boot_args = None;
                self.boot_attempt = 0;
                self.server_started_at = b.server_started_at;
                self.handle = b.handle;
                self.notifications = b.notifications.clone();
                let chooser = self.enter_chooser();
                // Fetch the app settings + hint snapshot on this connection: the chooser shows
                // the first hint a fresh install ever sees (docs/hints.md).
                let startup = self.session.startup();
                let startup = self.run_core(startup);
                Task::batch([pump(b.notifications), chooser, startup])
            }
            // The dial only ever yields Session/Choose; Connecting can't come back.
            Message::Booted(Ok(Bootstrap::Connecting(_))) => Task::none(),
            Message::Booted(Err(BootError::Retry(e))) => {
                tracing::debug!(error = %e, "boot connect failed; retrying");
                match &self.boot_args {
                    Some(args) => {
                        self.boot_attempt += 1;
                        spawn_connect_delayed(args.clone(), self.boot_attempt)
                    }
                    None => Task::none(),
                }
            }
            // Version mismatch on the initial dial: a stale daemon holds the port. Retrying can't
            // help, so stop dialing (drop `boot_args`) and drop the connecting placeholder into the
            // terminal `Failed` state with a persistent error toast naming the fix.
            Message::Booted(Err(BootError::Fatal(e))) => {
                tracing::warn!(error = %e, "boot connect rejected: version mismatch");
                self.boot_args = None;
                let fx = self.session.on_event(CoreEvent::ReconnectFatal(e));
                self.run_core(fx)
            }
            // Client-side input runs against the placeholder session (RPCs dropped while
            // Connecting), giving the reconnect-style "some keys work" feel.
            other => self.update_inner(other),
        }
    }

    /// Wall clock in Unix ms — the hint engine's injected clock (the core never reads time).
    pub(crate) fn now_unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Editor(ev) => self.on_editor_event(ev),
            Message::Key { code, mods, text } => self.on_key(code, mods, text),
            // Handled upstream in `update` (tracked in every phase); listed here only for exhaustiveness.
            Message::ModifiersChanged(_) => Task::none(),

            Message::HintTick => {
                let fx = self.session.on_hint_tick(Self::now_unix_ms());
                self.run_core(fx)
            }

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

            // Ctrl-click on a picker row opens it in a new window — the mouse sibling of Ctrl-Enter.
            // `mouse_area::on_press` carries no modifiers, so we consult the tracked `self.modifiers`;
            // a plain click falls through to the generic arm below (normal open in this window).
            Message::Core(CoreEvent::PickerClicked(abs)) if self.modifiers.control() => {
                let fx = self.session.picker_click_new_window(abs);
                self.run_core(fx)
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
                let ui = self.ui();
                let Some(p) = &mut self.session.picker else {
                    return Task::none();
                };
                self.picker_scroll_y = y;
                match p.scrolled_refetch(crate::picker::first_visible_row(y, ui)) {
                    Some(offset) => {
                        // Free pixel scroll — the view moved, not the selection — so the reply must
                        // not chase the highlight back (`chase_selection = false`).
                        let fx = self.session.picker_refetch(offset, false);
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
            Message::ReadClick(byte) => {
                // The clicked element is on screen by definition — remember the target so the
                // focus-reveal snap skips this landing instead of jolting the scroll.
                self.read_click_target = Some(byte);
                let fx = self.session.read_click(byte);
                self.run_core(fx)
            }
            Message::ReadClickActivate(byte) => {
                self.read_click_target = Some(byte);
                // `mouse_area` hands over no modifiers — consult the tracked state, as the
                // picker rows do: Ctrl-click opens a relative link in a new window.
                let fx = if self.modifiers.control() {
                    self.session.read_click_new_window(byte)
                } else {
                    self.session.read_click_activate(byte)
                };
                self.run_core(fx)
            }
            Message::RemoteImageFetched(url, result) => {
                let entry = match result {
                    Ok(bytes) => {
                        // SVG is a document, not a raster — it rides `widget::svg`. Sniff the
                        // payload (servers get extensions wrong): XML/SVG starts with `<`,
                        // optionally after whitespace/BOM.
                        let looks_svg = url
                            .split('?')
                            .next()
                            .is_some_and(|p| p.to_ascii_lowercase().ends_with(".svg"))
                            || bytes
                                .iter()
                                .find(|b| !b.is_ascii_whitespace())
                                .is_some_and(|b| *b == b'<');
                        if looks_svg {
                            RemoteImage::Svg(iced::widget::svg::Handle::from_memory(bytes))
                        } else {
                            RemoteImage::Raster(iced::widget::image::Handle::from_bytes(bytes))
                        }
                    }
                    Err(_) => RemoteImage::Failed,
                };
                self.remote_images.insert(url, entry);
                Task::none()
            }
            Message::ReadRevealMeasured(offset) => match offset {
                // Through the read glide: smooth when short, snap when far — the editor's
                // reveal feel. A placement (fresh document) snaps outright.
                Some(y) => {
                    let smooth = !std::mem::take(&mut self.read_reveal_snap);
                    self.read_scroll_to(y, smooth)
                }
                None => {
                    self.read_reveal_snap = false;
                    Task::none()
                }
            },
            Message::ReadScrolled { y, max } => {
                // An offset the glide didn't emit is user input (wheel/drag): snap the glide
                // off rather than fighting it — the editor's wheel behaviour.
                if self.read_scroll_anim.is_some() && (y - self.read_anim_last).abs() > 1.0 {
                    self.read_scroll_anim = None;
                }
                self.read_scroll_px = y;
                self.read_scroll_max = Some(max);
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
                // Scroll easing (independent of the throbber) — the editor's own pixel
                // scroll, and the read view's glide (same curve, driving the scrollable
                // widget by absolute `scroll_to` per frame).
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if let Some(anim) = self.scroll_anim {
                    let t = ((now - anim.started).as_secs_f32() * 1000.0 / SCROLL_ANIM_MS).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3); // cubic ease-out
                    self.scroll_px = anim.from + (anim.to - anim.from) * eased;
                    if t >= 1.0 {
                        self.scroll_anim = None;
                    }
                    self.clamp_scroll();
                    tasks.push(self.maybe_fetch());
                }
                if let Some(anim) = self.read_scroll_anim {
                    let t = ((now - anim.started).as_secs_f32() * 1000.0 / SCROLL_ANIM_MS).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3);
                    let y = anim.from + (anim.to - anim.from) * eased;
                    if t >= 1.0 {
                        self.read_scroll_anim = None;
                    }
                    self.read_anim_last = y;
                    tasks.push(iced::widget::operation::scroll_to(
                        read_scroll_id(),
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
                    ));
                }
                Task::batch(tasks)
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
                    // No workspace to restore: either none was active (the chooser was up when
                    // the connection dropped — land back in it, quietly) or the one we had is
                    // gone (renamed/removed while away). Recover into the chooser over the
                    // fresh connection, mirroring a no-args start.
                    None => {
                        let toast = if self.session.is_placeholder() {
                            // Grouped "connection", replacing the "reconnecting…" toast in
                            // place — the same evolution the restore path gets from the core.
                            self.toast("Reconnected", ToastKind::Success, Some("connection".into()))
                        } else {
                            self.toast(
                                "Workspace no longer exists — pick another",
                                ToastKind::Warning,
                                None,
                            )
                        };
                        let chooser = self.enter_chooser();
                        Task::batch([pump(r.notifications), chooser, toast])
                    }
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
            // `Booted` is handled in `update_connecting`; once a session exists it's a stale
            // dial result — nothing to do.
            Message::Booted(_) => Task::none(),
        }
    }

    fn toast(
        &mut self,
        message: impl Into<String>,
        kind: ToastKind,
        group: Option<String>,
    ) -> Task<Message> {
        let message = message.into();
        // A grouped toast replaces any existing toast with the same key, so an evolving status
        // (LSP restart → ready, the diff toggle) updates one toast in place. Ungrouped toasts
        // stack; repeat-prone messages (search errors, nav boundaries) carry a group in the core,
        // so this replacement is what coalesces them — uniformly across every shell.
        if let Some(g) = &group {
            self.toasts
                .retain(|t| t.group.as_deref() != Some(g.as_str()));
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
                Effect::HintTickNow => {
                    let fx = self.session.on_hint_tick(Self::now_unix_ms());
                    tasks.push(self.run_core(fx));
                }
                Effect::Exit => tasks.push(iced::exit()),
                Effect::ToChooser => tasks.push(self.enter_chooser()),
                Effect::ReadClipboard(kind) => tasks.push(self.read_clipboard(kind)),
                Effect::ShellAction(action) => tasks.push(self.run_shell_action(action)),
                Effect::RestoreScrollAnchor => {
                    if let Some(px) = self.scroll_anchor.take() {
                        self.scroll_to_px(px, false);
                    }
                }
            }
        }
        // Reading-view focus reveal: when the focused element changed (a `j`/`k` step, an
        // outline jump, search `n`), glide the document toward it. Widget layout heights aren't
        // knowable here, so position approximates as the focus span's fraction of the source —
        // the §2.7 best-effort contract.
        if let Some(read) = self.session.read.as_ref() {
            // Keyed to the Enter target when the cursor sits inside one (a Tab step must
            // reveal the link, not just its paragraph), else the block-grain position.
            let cursor = self.session.buffer.cursor.position;
            let focus = read
                .target_focus(cursor)
                .or_else(|| read.block_focus(cursor))
                .map(|i| {
                    let sp = read.elements[i].span();
                    (read.buffer_id, sp.start, sp.end)
                });
            if focus != self.read_last_focus {
                // A reveal into a buffer this view hasn't revealed in yet — a cross-file
                // landing, or the reading view just appearing — is a placement, not a
                // motion: it snaps (the editor's cross-buffer jump contract). Same-document
                // reveals keep glide-when-short.
                let fresh = match (self.read_last_focus, focus) {
                    (Some((prev, ..)), Some((cur, ..))) => prev != cur,
                    (None, Some(_)) => true,
                    _ => false,
                };
                self.read_last_focus = focus;
                if let Some((_, start, _)) = focus {
                    // A click-focus landing skips the reveal (the element was under the
                    // pointer).
                    if self.read_click_target.take() == Some(start) {
                        // consumed
                    } else {
                        // Measure the focused block's real position, then scroll to it via
                        // `ReadRevealMeasured` — block heights vary wildly (images, code
                        // panels), so no source-derived approximation survives contact.
                        self.read_reveal_snap = fresh;
                        tasks.push(
                            iced::advanced::widget::operate(ReadRevealProbe::default())
                                .map(Message::ReadRevealMeasured),
                        );
                    }
                }
            }
        } else {
            self.read_last_focus = None;
            // The read scrollable is gone with its widget state — drop the glide + mirrors.
            self.read_scroll_anim = None;
            self.read_scroll_px = 0.0;
            self.read_scroll_max = None;
        }
        // Remote-image fetch fan-out (docs/markdown-view.md §2.8): once per parse, download any
        // http(s) display image the document references; results land as `RemoteImageFetched`
        // and paint in as they arrive. The cache is URL-keyed and session-lived, so revisits and
        // re-parses are free.
        let scan_key = self
            .session
            .read
            .as_ref()
            .filter(|r| !r.blocks.is_empty())
            .map(|r| (r.buffer_id, r.revision));
        if let Some(key) = scan_key {
            if self.read_remote_scan != Some(key) {
                self.read_remote_scan = Some(key);
                let urls = remote_image_sources(
                    &self.session.read.as_ref().expect("scanned above").blocks,
                );
                for url in urls {
                    if !self.remote_images.contains_key(&url) {
                        self.remote_images.insert(url.clone(), RemoteImage::Loading);
                        let for_message = url.clone();
                        tasks.push(Task::perform(fetch_remote_image(url), move |r| {
                            Message::RemoteImageFetched(for_message.clone(), r)
                        }));
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
                // With a picker or a modal prompt open, the overlay's own scrollable owns wheel
                // input over the box; wheel over the backdrop shouldn't scroll the editor behind
                // it either — a modal owns the screen, matching the press path above.
                if self.session.picker.is_some() || self.session.prompt.is_some() {
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
        // The workspace-settings add-project row is the same path editor, so it needs the same
        // caret snap: `Alt-l` rewrites the value under the controlled `text_input`, which otherwise
        // leaves its caret at the old index — mid-string, right after an accept.
        if let Some(ps) = &self.session.workspace_settings {
            if ps.row() == SettingsRow::AddProject {
                let multi_root = self.session.workspace_paths.len() > 1;
                return Some(
                    if multi_root && ps.add_project.field == crate::chips::ChipEditorField::Root {
                        (
                            OverlayField::WorkspaceAddProjectRoot,
                            ps.add_project.root_filter.text.clone(),
                        )
                    } else {
                        (
                            OverlayField::WorkspaceAddProject,
                            ps.add_project.input.text.clone(),
                        )
                    },
                );
            }
        }
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
            OverlayField::WorkspaceAddProject => {
                self.session.workspace_settings_set_add_project(value)
            }
            OverlayField::WorkspaceAddProjectRoot => {
                self.session.workspace_settings_set_add_project_root(value)
            }
            OverlayField::WorkspaceAddProjectLanguage => self
                .session
                .workspace_settings_set_add_project_language(value),
            OverlayField::ChipRoot => self.session.chip_editor_set_root_filter(value),
            OverlayField::ChipPath => self.session.chip_editor_set_input(value),
        }
    }

    /// Actions whose execution is irreducibly shell-side (`Effect::ShellAction`).
    fn run_shell_action(&mut self, action: ShellAction) -> Task<Message> {
        use ShellAction as A;
        match action {
            // The reading view's Enter on an external link or image. The core sends either a
            // URL or an absolute path; paths ride the `file:` scheme through the same
            // allow-listed opener hover links use.
            A::OpenUrl(url) => {
                if let Some(path) = url.strip_prefix('/') {
                    open_link(&format!("file:///{path}"));
                } else {
                    open_link(&url);
                }
                Task::none()
            }
            // A local image beside the buffer: open the absolute path with the system handler
            // (the web half of this action rides the asset route instead).
            A::OpenBufferFile { absolute, .. } => {
                if let Some(path) = absolute.strip_prefix('/') {
                    open_link(&format!("file:///{path}"));
                }
                Task::none()
            }
            A::Scroll { dir, unit } => {
                // In the reading view the vertical scroll is the document scrollable's;
                // Left/Right pan the *focused* code panel (its scrollable carries
                // `read_code_scroll_id` only while focused, so this no-ops elsewhere).
                if self.session.read.is_some() {
                    if matches!(dir, ScrollDir::Left | ScrollDir::Right) {
                        let step = match unit {
                            ScrollUnit::Line => 48.0,
                            ScrollUnit::Half => 160.0,
                            ScrollUnit::Page => 320.0,
                        };
                        let dx = if matches!(dir, ScrollDir::Left) {
                            -step
                        } else {
                            step
                        };
                        return iced::widget::operation::scroll_by(
                            read_code_scroll_id(),
                            iced::widget::scrollable::AbsoluteOffset { x: dx, y: 0.0 },
                        );
                    }
                    let line = self.session.buffer_font_size as f32 * READ_SCALE * 1.6;
                    let vh = (self.visible_rows() as f32).max(1.0)
                        * self.cell.map(|c| c.height).unwrap_or(line);
                    let mag = match unit {
                        ScrollUnit::Line => line,
                        ScrollUnit::Half => (vh * 0.5).max(line),
                        ScrollUnit::Page => vh.max(line),
                    };
                    let dy = match dir {
                        ScrollDir::Up => -mag,
                        ScrollDir::Down => mag,
                        ScrollDir::Left | ScrollDir::Right => unreachable!("handled above"),
                    };
                    // The same glide as the editor's keys: accumulate on the target so key
                    // repeat extends the motion instead of restarting it.
                    let target = self.read_scroll_target() + dy;
                    return self.read_scroll_to(target, true);
                }
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
                // Reading view: edge-matched placement of the focused block — measured
                // against real widget geometry by the reveal probe, in explicit mode.
                if self.session.read.is_some() {
                    return iced::advanced::widget::operate(ReadRevealProbe {
                        place: Some(place),
                        ..Default::default()
                    })
                    .map(Message::ReadRevealMeasured);
                }
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
            A::NewWindow(target) => {
                spawn_target(&target);
                Task::none()
            }
        }
    }

    // ---- actions ----------------------------------------------------------------------------

    fn subscribe_task(&mut self) -> Task<Message> {
        if self.session.is_placeholder() {
            return Task::none(); // no buffer to subscribe — the mandatory chooser is up
        }
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

    /// Scroll the jumplist so the highlighted row is in view: `Top` aligns the row to
    /// the top of the pane unless it's already visible (grep file-jumps — landing on a new
    /// file reveals it from its first hit without yanking an in-view jump).
    fn picker_reveal_selected_with(&mut self, reveal: Reveal) -> Task<Message> {
        let ui = self.ui();
        let Some(p) = &self.session.picker else {
            return Task::none();
        };
        reveal_picker_selection(p, &mut self.picker_scroll_y, reveal, ui)
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
                    .map_err(ReconnectError::from)?;
                // The connection died while the chooser was up (a placeholder session — its
                // workspace name is empty, and no real one can be: create validates non-empty).
                // Nothing to restore; hand back a workspace-less reconnect and the shell
                // re-raises the chooser. No workspace is activated, so no instance stamp to
                // learn yet (0) — the next activation re-establishes the baseline.
                if workspace.is_empty() {
                    return Ok(Box::new(Reestablished {
                        handle,
                        notifications: std::sync::Arc::new(tokio::sync::Mutex::new(rx)),
                        restore: None,
                        server_url,
                        server_started_at: 0,
                    }));
                }
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

    /// The read view's [`Self::scroll_to_px`]: glide when the move is short (≤ ~1.5 views),
    /// snap when far — driving the scrollable widget by per-tick `scroll_to` operations,
    /// against the `on_scroll`-mirrored offset. Clamped once the mirror has seen the range
    /// (before any scroll event the range is unknown; `scroll_to` clamps on apply anyway).
    fn read_scroll_to(&mut self, target: f32, smooth: bool) -> Task<Message> {
        let target = match self.read_scroll_max {
            Some(max) => target.clamp(0.0, max),
            None => target.max(0.0),
        };
        let delta = (target - self.read_scroll_px).abs();
        let max_smooth = self.view_size.height * 1.5;
        if smooth && delta > 0.0 && delta <= max_smooth {
            self.read_scroll_anim = Some(ScrollAnim {
                from: self.read_scroll_px,
                to: target,
                started: std::time::Instant::now(),
            });
            Task::none() // the AnimTick frames drive it
        } else {
            self.read_scroll_anim = None;
            self.read_anim_last = target;
            iced::widget::operation::scroll_to(
                read_scroll_id(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: target },
            )
        }
    }

    /// [`Self::scroll_target`] for the read view: keypress repeat accumulates on the glide's
    /// destination.
    fn read_scroll_target(&self) -> f32 {
        self.read_scroll_anim
            .as_ref()
            .map(|a| a.to)
            .unwrap_or(self.read_scroll_px)
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

    /// The chrome's sizing scale, from the `ui_font_size` app setting. Every chrome size in the
    /// view goes through this (the buffer has its own `buffer_font_size`, read by the editor
    /// widget) — see [`theme::Ui`].
    fn ui(&self) -> theme::Ui {
        theme::Ui::new(self.session.ui_font_size)
    }

    pub fn view(&self) -> Element<'_, Message> {
        // No workspace picked yet (the mandatory chooser, or the beat after a pick while the
        // activation is in flight): a plain backdrop with no editor chrome behind the overlays,
        // matching the TUI/web no-workspace views. The boot-*connecting* placeholder is the
        // exception — it renders the normal chrome + "Connecting…" banner below (the same
        // familiar feel as a mid-session reconnect; the editor's Layout fires no RPC while not
        // `Connected`, and `status_bar` is fully Option-guarded, so the placeholder is safe).
        let base: Element<'_, Message> =
            if self.session.is_placeholder() && self.session.conn != ConnState::Connecting {
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(theme::NORD0.into()),
                        ..container::Style::default()
                    })
                    .into()
            } else if self.session.read.is_some() {
                // The markdown reading view replaces the editor wholesale while active
                // (docs/markdown-view.md §2.8) — the same status bar and overlays around it.
                column![self.read_view(), self.status_bar()].into()
            } else {
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
                        font_size: self.session.buffer_font_size as f32,
                    },
                    Message::Editor,
                );
                column![Element::from(editor), self.status_bar()].into()
            };
        let mut layers: Vec<Element<'_, Message>> = vec![base];
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
                    self.session.tether,
                    self.picker_scroll_y,
                    self.spinner_phase,
                    self.ui(),
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
                    // The chip editor has only two segments; the third is settings-only.
                    PickerMsg::EditorExtra(_) => Message::Noop,
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
        if !self.toasts.is_empty() {
            layers.push(self.toast_overlay());
        }
        // The hint (docs/hints.md): above the overlays (a picker context's hints must
        // show over the picker) but below the connection banner. Top-right, so it collides with
        // nothing else.
        if let Some(hint) = self.session.hint_view() {
            layers.push(self.hint_corner(hint));
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

    /// The hint corner (docs/hints.md): a quiet top-right "Hint: …" chip with the key label
    /// emphasized. Deliberately subtler than a toast: no shadow-heavy card, no animation, no
    /// icon; it should read as ambient chrome, not a notification.
    fn hint_corner(&self, hint: aether_client::hints::HintView) -> Element<'_, Message> {
        let ui = self.ui();
        let (before, keys, after) = hint.parts();
        let dim = |s: &'static str| {
            text(s)
                .size(ui.small())
                .font(SANS)
                .color(theme::NORD3_BRIGHT)
        };
        let body = iced::widget::row![
            dim("Hint: "),
            dim(before),
            text(keys)
                .size(ui.small())
                .font(SANS_BOLD_UI)
                .color(theme::NORD8),
            dim(after),
        ];
        let chip = container(body)
            .padding([4, 10])
            .style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                border: iced::Border {
                    radius: 5.0.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            });
        container(chip)
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Right)
            .padding(iced::Padding {
                top: 8.0,
                right: 12.0,
                ..iced::Padding::ZERO
            })
            .into()
    }

    /// Floating connection banner (the web's `#conn-banner`): a top-centred pill while the
    /// connection isn't healthy — yellow while the retry loop dials, red once
    /// re-establishing failed terminally.
    fn conn_banner(&self) -> Element<'_, Message> {
        let ui = self.ui();
        let (label, bg, fg) = match self.session.conn {
            ConnState::Failed => ("Disconnected", theme::NORD11, theme::NORD6),
            // Boot before the daemon is up — distinct copy from a mid-session blip.
            ConnState::Connecting => ("Connecting…", theme::NORD13, theme::NORD0),
            _ => ("Reconnecting…", theme::NORD13, theme::NORD0),
        };
        let pill = container(text(label).size(ui.small()).font(SANS).color(fg))
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

    /// The workspace-settings dialog (`Space ,`): a centred modal with the editable workspace name,
    /// the list of roots, and an add-root input row — rendered from the core's
    /// `session.workspace_settings`. Keyboard-driven (keys route through `session.on_key`, which the
    /// core handles): Alt-j/k navigate, Enter renames / adds, Delete (then y) removes, Esc closes.
    /// NORD modal box + opaque backdrop.
    fn workspace_settings_overlay(&self) -> Element<'_, Message> {
        self.workspace_settings_body()
    }

    /// The dialog content. The name + add-root fields are controlled `text_input`s (web parity,
    /// syncing via `workspace_settings_set_name` / `_set_add`); the per-root delete buttons carry
    /// `WorkspaceSettingsMsg` mapped inline to `Message` (since the inputs already produce `Message`,
    /// the whole tree is `Message`-typed rather than mapped at the end).
    fn workspace_settings_body(&self) -> Element<'_, Message> {
        let ui = self.ui();
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
                overlay_input(fieldkind, placeholder, value, ui)
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
                .size(ui.small())
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
                .size(ui.heading())
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
                    .size(ui.small())
                    .font(SANS)
                    .color(theme::NORD3_BRIGHT),
            );
        }
        // A bulleted row: `• <content> …`, indented one bullet-gap from the label (web parity).
        // No row box — selection tints only the path text (see below).
        fn bulleted<'a>(inner: Element<'a, Message>, ui: theme::Ui) -> Element<'a, Message> {
            container(
                row![
                    text("•").size(ui.body()).font(SANS).color(theme::NORD6),
                    inner
                ]
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
            let highlighted = s.row() == SettingsRow::Root(i);
            let delete =
                iced::widget::button(text("✕").size(ui.small()).font(SANS).color(theme::NORD6))
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
                WorkspaceSettingsMsg::RemoveProject(i) => {
                    Message::Core(CoreEvent::WorkspaceSettingsRemoveProject(i))
                }
            });
            // Selection tints just the path text (web/terminal parity), so the background hugs the
            // text — no padding, so the text lines up with the borderless add-root input below.
            let path = container(
                text(root.clone())
                    .size(ui.body())
                    .font(SANS)
                    .color(theme::NORD6),
            )
            .style(move |_| container::Style {
                background: highlighted.then(|| theme::NORD2.into()),
                border: iced::Border {
                    radius: 3.0.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            });
            let inner = row![path, iced::widget::Space::new().width(Length::Fill), delete,]
                .align_y(iced::Alignment::Center)
                .spacing(6);
            roots_col = roots_col.push(bulleted(inner.into(), ui));
        }

        // The always-present add-root input row — a borderless input after its bullet, so the caret
        // is the focus cue (web/terminal parity), not a box.
        // Placeholder only while unfocused — once you're typing here the caret is the cue, and a
        // greyed prompt sitting under it is noise. Same rule as the add-project row.
        let add_root_placeholder = if s.row() == SettingsRow::AddRoot {
            ""
        } else {
            "Add root..."
        };
        roots_col = roots_col.push(bulleted(
            field(
                OverlayField::WorkspaceAddRoot,
                &s.add.text,
                add_root_placeholder,
            ),
            ui,
        ));
        col = col.push(roots_col);

        // The Projects group (docs/projects.md): declared directories whose language servers stay
        // pinned while the workspace is active. Same shape as Roots — bulleted rows with a delete
        // button, then an always-present add input — with a trailing tag per row carrying either the
        // language it pins or, in red, why it can't be used.
        let mut projects_col = column![label("Projects")].spacing(2);
        for (i, project) in s.projects.iter().enumerate() {
            let highlighted = s.row() == SettingsRow::Project(i);
            let delete =
                iced::widget::button(text("✕").size(ui.small()).font(SANS).color(theme::NORD6))
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
                    .on_press(WorkspaceSettingsMsg::RemoveProject(i));
            let delete = Element::from(delete).map(|m| match m {
                WorkspaceSettingsMsg::RemoveRoot(i) => {
                    Message::Core(CoreEvent::WorkspaceSettingsRemoveRoot(i))
                }
                WorkspaceSettingsMsg::RemoveProject(i) => {
                    Message::Core(CoreEvent::WorkspaceSettingsRemoveProject(i))
                }
            });
            let path = container(
                text(aether_client::labels::root_relative_display(
                    &self.session.workspace_paths,
                    project.path_index,
                    &project.relative_path,
                ))
                .size(ui.body())
                .font(SANS)
                .color(theme::NORD6),
            )
            .style(move |_| container::Style {
                background: highlighted.then(|| theme::NORD2.into()),
                border: iced::Border {
                    radius: 3.0.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            });
            let (tag_text, tag_color) = match &project.error {
                Some(e) => (e.clone(), theme::NORD11),
                None => (project.language.clone(), theme::NORD3_BRIGHT),
            };
            let tag = text(tag_text).size(ui.small()).font(SANS).color(tag_color);
            let inner = row![
                path,
                iced::widget::Space::new().width(Length::Fill),
                tag,
                delete,
            ]
            .align_y(iced::Alignment::Center)
            .spacing(6);
            projects_col = projects_col.push(bulleted(inner.into(), ui));
        }
        // The add-project row is the save-as path editor, rendered the same way while it has focus:
        // the root segment shows its inline typeahead ghost, the path segment its completion ghost.
        // Rendering the ghosts is what makes `Alt-j/k` cycling *visible* — without them the
        // candidate changes underneath and the row looks inert.
        //
        // Unfocused and empty, it collapses to a plain "Add project..." label instead — the same
        // affordance the add-root row gives.
        use crate::picker::{field_with_ghost, Boundary, PickerMsg};
        let ed = &s.add_project;
        let roots = &self.session.workspace_paths;
        let labels = crate::labels::root_labels(roots);
        let multi_root = roots.len() > 1;
        let focused = s.row() == SettingsRow::AddProject;
        let project_row: Element<'_, Message> = if !focused && ed.input.text.is_empty() {
            text("Add project...")
                .size(ui.body())
                .font(SANS)
                .color(theme::NORD3_BRIGHT)
                .into()
        } else {
            let mut project_row = row![].align_y(iced::Alignment::Center);
            if multi_root {
                let invalid = ed.root_invalid(&labels);
                let mut root_group = row![].spacing(0).align_y(iced::Alignment::Center);
                // A live input only while this row *and* the root segment have focus; otherwise the
                // settled label, so an unfocused row never shows a stray caret.
                if focused && ed.field == crate::chips::ChipEditorField::Root {
                    root_group = root_group.push(field_with_ghost(
                        &ed.root_filter,
                        ed.root_ghost(&labels).map(|(_, suffix)| suffix),
                        invalid,
                        OverlayField::WorkspaceAddProjectRoot.id(),
                        "",
                        ":",
                        PickerMsg::EditorRoot,
                        true,
                        Boundary::ConfirmRoot,
                        ui,
                    ));
                } else {
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
                        root_group.push(text(display).size(ui.body()).font(SANS).color(color));
                    // Only the settled label needs its separator pushed; the focused segment draws
                    // its own (flush against the text — see `field_with_ghost`'s `trailing`).
                    root_group = root_group.push(
                        text(":")
                            .size(ui.body())
                            .font(SANS)
                            .color(theme::NORD3_BRIGHT),
                    );
                }
                project_row = project_row.push(root_group).spacing(6);
            }
            // A live input only while the path segment itself has focus. Otherwise static text —
            // a ghost left showing while another segment is focused reads as part of the value
            // (`databricks/` trailed by `.databricks/` looks like the path you're committing).
            if s.on_add_project_language {
                project_row = project_row.push(
                    text(ed.input.text.clone())
                        .size(ui.body())
                        .font(SANS)
                        .color(if ed.path_invalid() {
                            theme::NORD11
                        } else {
                            theme::NORD6
                        }),
                );
            } else {
                // Placeholder is always empty here: the ghost layer behind the input is what shows
                // suggestions, and a placeholder drawn on top of it would overlap.
                project_row = project_row.push(field_with_ghost(
                    &ed.input,
                    ed.path_ghost(),
                    ed.path_invalid(),
                    OverlayField::WorkspaceAddProject.id(),
                    "",
                    "",
                    PickerMsg::EditorPath,
                    false,
                    if multi_root {
                        Boundary::PathToRoot
                    } else {
                        Boundary::None
                    },
                    ui,
                ));
            }
            // The optional language override, right-aligned like the language tags on the project
            // rows above. Only appears once it has focus or a value — an empty segment on every row
            // would be noise for the nine-in-ten projects whose language is inferable.
            if s.on_add_project_language || !s.add_project_language.text.is_empty() {
                // A plain gap rather than a glyph: the row already reads `root: path`, and a second
                // punctuation mark competes with the `:` for meaning. When the path renders as
                // static text it hugs its content, so the gap does the pushing (`Fill`); a focused
                // path is a `Fill` input that already pushes the segment to the right edge, and a
                // second `Fill` would steal half its width.
                let gap: Length = if s.on_add_project_language {
                    Length::Fill
                } else {
                    ui.at(12.0).into()
                };
                project_row = project_row.push(iced::widget::Space::new().width(gap));
                if s.on_add_project_language {
                    project_row = project_row.push(field_with_ghost(
                        &s.add_project_language,
                        s.language_ghost(),
                        s.language_invalid(),
                        OverlayField::WorkspaceAddProjectLanguage.id(),
                        "language",
                        "",
                        PickerMsg::EditorExtra,
                        true,
                        Boundary::None,
                        ui,
                    ));
                } else {
                    project_row = project_row.push(
                        text(s.add_project_language.text.clone())
                            .size(ui.body())
                            .font(SANS)
                            .color(if s.language_invalid() {
                                theme::NORD11
                            } else {
                                theme::NORD3_BRIGHT
                            }),
                    );
                }
            }
            Element::from(project_row).map(|m| match m {
                PickerMsg::EditorRoot(s) => {
                    Message::OverlayInput(OverlayField::WorkspaceAddProjectRoot, s)
                }
                PickerMsg::EditorPath(s) => {
                    Message::OverlayInput(OverlayField::WorkspaceAddProject, s)
                }
                PickerMsg::EditorExtra(s) => {
                    Message::OverlayInput(OverlayField::WorkspaceAddProjectLanguage, s)
                }
                PickerMsg::CoreKey(code) => core_key_message(code),
                _ => Message::Noop,
            })
        };
        projects_col = projects_col.push(bulleted(project_row, ui));
        col = col.push(projects_col);

        if let Some(err) = &s.error {
            col = col.push(
                text(err.clone())
                    .size(ui.small())
                    .font(SANS)
                    .color(theme::NORD11),
            );
        }

        // Wider than the other overlays: the add-project row carries three segments (root, path,
        // language) and cramping them makes the path unreadable.
        let boxed = container(col.spacing(8))
            .width(ui.at(640.0))
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
        let ui = self.ui();
        let s = self.session.app_settings.as_ref().unwrap();
        let groups = self.session.app_setting_groups();

        let mut col = column![text("Application settings")
            .size(ui.heading())
            .font(SANS_BOLD_UI)
            .color(theme::NORD6)]
        .spacing(14);

        // Running flat row index across groups (the index `AppSettingToggle` / `selected` use).
        let mut flat = 0usize;
        for group in &groups {
            let mut gcol = column![text(group.title.to_string())
                .size(ui.small())
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
                        .size(ui.control())
                        .on_toggle(move |_| Message::Core(CoreEvent::AppSettingToggle(i)))
                        .into(),
                    AppSettingControl::Value(v) => {
                        // `button` needs a `Clone` press message and `Message` isn't `Clone`, so the
                        // button carries the row index (a `usize`) and we map it to `Message` — the
                        // same pattern as the workspace-settings delete button.
                        let btn = iced::widget::button(
                            text(v.to_string())
                                .size(ui.body())
                                .font(SANS)
                                .color(theme::NORD6),
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
                            .size(ui.body())
                            .font(SANS)
                            .color(theme::NORD6),
                        iced::widget::Space::new().width(Length::Fill),
                        check,
                    ]
                    .align_y(iced::Alignment::Center)
                    .spacing(6),
                    text(r.hint.to_string())
                        .size(ui.small())
                        .font(SANS)
                        .color(theme::NORD3_BRIGHT),
                ]
                .spacing(2);
                gcol = gcol.push(field);
            }
            col = col.push(gcol);
        }

        let boxed = container(col.spacing(14))
            .width(ui.at(420.0))
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

    /// The floating search prompt, bottom-left above the status bar — mirrors the web client's
    /// `#searchbar` (query + beam cursor, match count on the right).
    fn search_bar(&self) -> Element<'_, Message> {
        let ui = self.ui();
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
                .size(ui.body())
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
            chips_row = chips_row.push(option_chip(chip, selected == Some(i), ui));
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
            bar = bar.push(text(count).size(ui.body()).font(SANS).color(theme::NORD4));
        }
        let prompt = container(bar)
            .width(ui.at(420.0))
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
    ///
    /// Geometry comes from [`prompt_box`] — see there for why the info dialog is shaped differently
    /// from the question-shaped prompts.
    fn prompt_overlay(&self) -> Element<'_, Message> {
        let ui = self.ui();
        let prompt = self.session.prompt.as_ref().unwrap();
        // The save-as arm embeds a controlled `text_input` (which produces `Message`), so the
        // whole body is built in `Message` space: the Clone-only buttons map their `PromptMsg`
        // immediately rather than the whole tree being mapped at the end.
        // The modal button roles, mirroring the web client's `.modal-btn` classes: `Default` is the
        // safe option (Cancel/No) — a plain, subtly bordered button; `Danger` is a destructive
        // confirm (Yes) in red; `Primary` is a non-destructive affirmative (Save) in frost blue.
        // Whichever button Enter triggers sits right-most, macOS-alert style: Primary for Save/Open,
        // but Default for the destructive confirms (Enter declines those — see `on_prompt_key`).
        #[derive(Clone, Copy)]
        enum BtnRole {
            Default,
            Danger,
            Primary,
        }
        // `key` is the shortcut hint rendered dimly after the label (the confirm buttons advertise
        // `y`/`n`); the save/open dialogs pass `None` — their Enter/Esc mapping is conventional.
        let btn = |label: &str,
                   key: Option<&str>,
                   role: BtnRole,
                   msg: PromptMsg|
         -> Element<'_, Message> {
            let mut content = row![text(label.to_string())
                .size(ui.body())
                .font(SANS)
                .color(theme::NORD6)]
            .spacing(7)
            .align_y(iced::Alignment::Center);
            if let Some(key) = key {
                content = content.push(text(format!("({key})")).size(ui.fine()).font(SANS).color(
                    iced::Color {
                        a: 0.55,
                        ..theme::NORD6
                    },
                ));
            }
            Element::from(
                iced::widget::button(content)
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
                                .size(ui.body())
                                .font(SANS)
                                .color(theme::NORD3_BRIGHT)
                        )
                        .width(ui.at(90.0)),
                        text(v).size(ui.body()).font(SANS).color(theme::NORD6),
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
                        text("● ").size(ui.heading()).color(dot),
                        text(info.name.clone())
                            .size(ui.body())
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
            // Application info (`Space ?`). Rows come from the core so all three shells agree on
            // content and wording; the GUI contributes the heading treatment and the label column.
            // Long values (absolute paths) wrap inside the fixed-width prompt box rather than being
            // truncated — a half path is worse than a two-line one.
            Prompt::AppInfo(info) => {
                use aether_client::app_info::InfoTone;
                // Two nested spacings, not one: rows within a section sit tight (3px) and the gap
                // between sections (12px) does the grouping. A single uniform spacing made the
                // dialog tall enough to overflow a small window.
                let mut col = column![text("Aether")
                    .size(ui.heading())
                    .font(SANS_BOLD_UI)
                    .color(theme::NORD6)]
                .spacing(12);
                for section in
                    aether_client::app_info::sections(info.as_deref(), &self.session.conn)
                {
                    let mut rows = column![text(section.title)
                        .size(ui.small())
                        .font(SANS_BOLD_UI)
                        .color(theme::NORD8)]
                    .spacing(3);
                    for r in section.rows {
                        // Yellow marks the client/server build mismatch — the only row here that
                        // reports a problem rather than a fact.
                        let value_color = match r.tone {
                            InfoTone::Warn => theme::NORD13,
                            InfoTone::Normal => theme::NORD6,
                        };
                        rows = rows.push(
                            row![
                                container(
                                    text(r.label)
                                        .size(ui.body())
                                        .font(SANS)
                                        .color(theme::NORD3_BRIGHT)
                                )
                                .width(ui.at(84.0)),
                                text(r.value).size(ui.body()).font(SANS).color(value_color),
                            ]
                            .spacing(8),
                        );
                    }
                    col = col.push(rows);
                }
                col.into()
            }
            Prompt::Confirm { kind, .. } => column![
                text(format!("{}?", confirm_phrase(kind)))
                    .size(ui.body())
                    .font(SANS)
                    .color(theme::NORD6),
                row![
                    iced::widget::Space::new().width(Length::Fill),
                    btn("Yes", Some("y"), BtnRole::Danger, PromptMsg::Accept),
                    btn("No", Some("n"), BtnRole::Default, PromptMsg::Cancel),
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
                            ":",
                            PickerMsg::EditorRoot,
                            true,
                            Boundary::ConfirmRoot,
                            ui,
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
                            root_group.push(text(display).size(ui.body()).font(SANS).color(color));
                        // The focused segment draws its own separator flush against the text.
                        root_group = root_group.push(
                            text(":")
                                .size(ui.body())
                                .font(SANS)
                                .color(theme::NORD3_BRIGHT),
                        );
                    }
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
                    "",
                    PickerMsg::EditorPath,
                    false,
                    path_boundary,
                    ui,
                ));
                let field: Element<'_, Message> = Element::from(field).map(|m| match m {
                    PickerMsg::EditorRoot(s) => Message::OverlayInput(OverlayField::SaveAsRoot, s),
                    PickerMsg::EditorPath(s) => Message::OverlayInput(OverlayField::SaveAs, s),
                    PickerMsg::CoreKey(code) => core_key_message(code),
                    // The save-as segments never emit row/scroll/chip messages.
                    _ => Message::Noop,
                });
                column![
                    text("Save as")
                        .size(ui.body())
                        .font(SANS)
                        .color(theme::NORD6),
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
                        btn("Cancel", None, BtnRole::Default, PromptMsg::Cancel),
                        btn("Save", None, BtnRole::Primary, PromptMsg::Accept),
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
                    .size(ui.body())
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
                    text("Open file")
                        .size(ui.body())
                        .font(SANS)
                        .color(theme::NORD6),
                    container(input)
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
                        btn("Cancel", None, BtnRole::Default, PromptMsg::Cancel),
                        btn("Open", None, BtnRole::Primary, PromptMsg::Accept),
                    ]
                    .spacing(8),
                ]
                .spacing(14)
                .into()
            }
        };
        let info_dialog = matches!(prompt, Prompt::AppInfo(_));
        let PromptBox { width, top, max_h } = prompt_box(info_dialog, self.view_size.height, ui);
        let body: Element<'_, Message> = if info_dialog {
            // Both widths are `Fill` on purpose. A scrollable defaults to `Shrink`, so it would
            // size to its widest row and draw the scrollbar against the *text* rather than the box
            // edge; filling makes its bounds the box's, which is where the bar belongs. The padding
            // sits inside the scrollable so the bar clears the box border rather than the content.
            iced::widget::scrollable(container(body).padding(16).width(Length::Fill))
                .width(Length::Fill)
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::new()
                        .width(theme::SCROLLBAR_W)
                        .margin(0)
                        .scroller_width(theme::SCROLLBAR_W),
                ))
                .into()
        } else {
            container(body).padding(16).into()
        };
        let boxed = container(body)
            .width(width)
            .max_height(max_h)
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
                top,
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
        let ui = self.ui();
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
                                .size(ui.body())
                                .font(SANS)
                                .color(color),
                            text(b.text.clone()).size(ui.body()).font(SANS).color(color),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Start)
                        .into(),
                        None => text(b.text.clone())
                            .size(ui.body())
                            .font(SANS)
                            .color(color)
                            .into(),
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
                md_doc(blocks, ui, Message::OpenLink)
            }
        };
        // Anchor at the cursor cell. Pick below/above by the room each side has for the
        // (estimated) height, then cap the popover to that room so tall (scrolled) content fits
        // *within* the window instead of overflowing its edge. The popover stays open while the
        // buffer scrolls, and even once the cursor scrolls out of the loaded window it keeps its
        // horizontal column and parks against the edge it left by (rather than jumping to a corner).
        const MARGIN: f32 = 4.0;
        const MAX_H: f32 = 380.0;
        let est_h = est_lines as f32 * ui.line_height() + 20.0;
        let mut anchor = None;
        let mut max_h = MAX_H;
        if self.session.read.is_some() {
            // Reading view: there's no cursor cell to hang from — park bottom-left over the
            // document (the terminal's bottom-anchored popover placement), where the read
            // target reveal (`Tab`) expects it.
            let view_h = self.view_size.height;
            max_h = MAX_H.min((view_h - 2.0 * MARGIN).max(40.0));
            anchor = Some((MARGIN + 4.0, HoverPlace::Bottom(view_h - MARGIN)));
        } else if let (Some(cell), Some(window)) = (self.cell, &self.session.window) {
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
                        .width(theme::SCROLLBAR_W)
                        .margin(0)
                        .scroller_width(theme::SCROLLBAR_W),
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
        let ui = self.ui();
        let t = |s: String, color: iced::Color| text(s).size(ui.body()).font(SANS).color(color);

        let mut left = row![];
        if let Some(color) = self.buffer_state_color() {
            // Through `state_dot` (== body size), the one knob the picker rows share.
            left = left.push(text("● ").size(ui.state_dot()).font(SANS).color(color));
        }
        // Persisted workspace → `[name] ` prefix. No workspace (boot/connecting/chooser) or an
        // ephemeral "(no workspace)" context → no prefix, so the bar shows just the file label
        // rather than a stray `[]` or a `[(no workspace)]` that reads like a real workspace.
        if crate::labels::shows_workspace_chrome(&self.session.workspace) {
            left = left.push(t(format!("[{}] ", self.session.workspace), theme::NORD4));
        }
        // Segment-elide long labels to roughly half the bar so the filename survives (the
        // web's `truncatePath`; chars approximate px since the bar is sans).
        let budget = ((self.view_size.width * 0.5 / ui.char_width()) as usize).max(12);
        let name = text(crate::labels::truncate_path(
            &self.session.buffer.label,
            budget,
        ))
        .size(ui.body())
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
        // The tether mark (docs/tether.md): a dim ` *` after the file label — closing this buffer
        // exits the window. Upright even on a slanted transient label, like the terminal client.
        if self.session.tethered() {
            left = left.push(t(" *".into(), theme::NORD3_BRIGHTER));
        }
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
        if let Some(results) = self.session.buffer.cursor.jumplist_position {
            right = right.push(t(
                format!("({}/{})", results.current, results.total),
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
        let ui = self.ui();
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
                            .size(ui.body())
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
fn option_chip<'a>(
    chip: &crate::chips::Chip,
    selected: bool,
    ui: theme::Ui,
) -> Element<'a, Message> {
    let underline = matches!(chip.id, crate::chips::ChipId::Word);
    let (bg, fg) = if selected {
        (theme::NORD8, theme::NORD0)
    } else {
        (theme::NORD2, theme::NORD8)
    };
    let spans: Vec<iced::widget::text::Span<'a>> = vec![iced::widget::span(chip.label.clone())
        .size(ui.small())
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
fn overlay_input<'a>(
    field: OverlayField,
    placeholder: &str,
    value: &str,
    ui: theme::Ui,
) -> Element<'a, Message> {
    // `alt_passthrough` keeps Alt-chords (the nav idiom) out of the input — winit delivers
    // `Alt+letter` as text on some platforms, which a focused `text_input` would otherwise insert.
    crate::alt_filter::alt_passthrough(
        iced::widget::text_input(placeholder, value)
            .id(field.id())
            .on_input(Typed)
            .font(SANS)
            .size(ui.body())
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

/// Hover-Markdown sizing, as tuned-at-the-default-base pixels (see [`theme::Ui::at`]): body text,
/// code blocks, and the gap between blocks.
const MD_TEXT: f32 = 13.0;
const MD_CODE: f32 = 12.0;
const MD_SPACING: f32 = 11.0;

/// Render the hover Markdown AST: a column of block elements. Everything is cloned, so the result
/// doesn't borrow the AST (`'static`).
fn md_doc<M: 'static>(
    blocks: &[MdBlock],
    ui: theme::Ui,
    on_link: fn(String) -> M,
) -> Element<'static, M> {
    let mut col = column![].spacing(ui.at(MD_SPACING));
    for b in blocks {
        col = col.push(md_block(b, ui, on_link));
    }
    col.into()
}

fn md_block<M: 'static>(
    b: &MdBlock,
    ui: theme::Ui,
    on_link: fn(String) -> M,
) -> Element<'static, M> {
    match b {
        MdBlock::Heading { level, content, .. } => {
            let size = match level {
                1 => 16.0,
                2 => 15.0,
                3 => 14.0,
                _ => MD_TEXT,
            };
            md_rich(content, true, theme::NORD6, ui.at(size), on_link)
        }
        MdBlock::Paragraph { content, .. } => {
            md_rich(content, false, theme::NORD4, ui.at(MD_TEXT), on_link)
        }
        MdBlock::Code { code, .. } => container(
            text(code.clone())
                .font(iced::Font::MONOSPACE)
                .size(ui.at(MD_CODE))
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
        MdBlock::List {
            ordered,
            start,
            items,
            ..
        } => {
            let mut col = column![].spacing(ui.at(MD_SPACING) * 0.5);
            for (i, item) in items.iter().enumerate() {
                let mut marker = if *ordered {
                    format!("{}.", start + i as u64)
                } else {
                    "•".to_string()
                };
                // Task-list items carry their checkbox on the marker.
                if let Some(done) = item.checked {
                    marker = format!("{} {}", marker, if done { "☑" } else { "☐" });
                }
                col = col.push(
                    row![
                        text(marker).size(ui.at(MD_TEXT)).color(theme::NORD4),
                        md_doc(&item.blocks, ui, on_link),
                    ]
                    .spacing(6),
                );
            }
            col.into()
        }
        MdBlock::Quote { content, .. } => row![md_bar(), md_doc(content, ui, on_link)]
            .spacing(8)
            .into(),
        MdBlock::Rule { .. } => container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(1)
            .style(md_bar_style)
            .into(),
        // The remaining kinds only occur in document (reading-view) parses; hover content never
        // produces them, but a fallback keeps hover total. The reading view has its own renderer.
        MdBlock::Table { head, rows, .. } => {
            let mut col = column![].spacing(ui.at(MD_SPACING) * 0.5);
            for row_cells in std::iter::once(head).chain(rows.iter()) {
                if row_cells.is_empty() {
                    continue;
                }
                let joined = row_cells
                    .iter()
                    .map(|c| md_plain(c))
                    .collect::<Vec<_>>()
                    .join("  |  ");
                col = col.push(text(joined).size(ui.at(MD_TEXT)).color(theme::NORD4));
            }
            col.into()
        }
        MdBlock::Image { alt, .. } => text(format!("[image: {alt}]"))
            .size(ui.at(MD_TEXT))
            .color(theme::NORD3)
            .into(),
        MdBlock::FrontMatter { .. } => iced::widget::Space::new().into(),
        MdBlock::FootnoteDef { label, content, .. } => row![
            text(format!("[{label}]:"))
                .size(ui.at(MD_TEXT))
                .color(theme::NORD3),
            md_doc(content, ui, on_link),
        ]
        .spacing(6)
        .into(),
        MdBlock::Html { raw, .. } => text(raw.clone())
            .font(iced::Font::MONOSPACE)
            .size(ui.at(MD_CODE))
            .color(theme::NORD3)
            .into(),
    }
}

/// `(natural, minimum)` estimated pixel widths of a table cell's inline run at `size`: the
/// whole run on one line, and the widest whitespace-unbreakable token (the cell can't wrap
/// below it). Serif glyphs average ~0.52 em; the monospace code face advances exactly 0.6 em,
/// padded to 0.62. Estimates only — real shaping happens at draw time, so column widths carry
/// padding slack on top.
fn cell_text_width(inlines: &[MdInline], size: f32) -> (f32, f32) {
    fn chars(
        text: &str,
        code: bool,
        size: f32,
        natural: &mut f32,
        word: &mut f32,
        minimum: &mut f32,
    ) {
        let w = if code { size * 0.62 } else { size * 0.52 };
        for c in text.chars() {
            if c.is_whitespace() {
                *word = 0.0;
            } else {
                *word += w;
                *minimum = minimum.max(*word);
            }
            *natural += w;
        }
    }
    fn walk(inlines: &[MdInline], size: f32, natural: &mut f32, word: &mut f32, minimum: &mut f32) {
        for inl in inlines {
            match inl {
                MdInline::Text { text } => chars(text, false, size, natural, word, minimum),
                MdInline::Code { text } => chars(text, true, size, natural, word, minimum),
                MdInline::Emphasis { content }
                | MdInline::Strong { content }
                | MdInline::Strikethrough { content }
                | MdInline::Link { content, .. } => walk(content, size, natural, word, minimum),
                MdInline::Image { alt, .. } => {
                    chars(&format!("▨ [{alt}]"), false, size, natural, word, minimum)
                }
                MdInline::FootnoteRef { label, .. } => {
                    chars(&format!("[{label}]"), false, size, natural, word, minimum)
                }
                MdInline::HardBreak => *word = 0.0,
            }
        }
    }
    let (mut natural, mut word, mut minimum) = (0f32, 0f32, 0f32);
    walk(inlines, size, &mut natural, &mut word, &mut minimum);
    (natural, minimum)
}

/// Flatten inlines to plain text (hover fallbacks for table cells).
fn md_plain(inlines: &[MdInline]) -> String {
    let mut out = String::new();
    for inl in inlines {
        match inl {
            MdInline::Text { text } | MdInline::Code { text } => out.push_str(text),
            MdInline::Strong { content }
            | MdInline::Emphasis { content }
            | MdInline::Strikethrough { content }
            | MdInline::Link { content, .. } => out.push_str(&md_plain(content)),
            MdInline::Image { alt, .. } => out.push_str(alt),
            MdInline::FootnoteRef { label, .. } => out.push_str(&format!("[{label}]")),
            MdInline::HardBreak => out.push(' '),
        }
    }
    out
}

// ---- the markdown reading view (docs/markdown-view.md §2.8, iced) -------------------------------

/// The reading body size in ems of the buffer font size — one step above the editor: reading
/// wants larger type than code. Matches the web's `#buffer.md-read-host { font-size: 1.125em }`.
const READ_SCALE: f32 = 1.125;

/// The reading measure in ems of the body size — the column tracks the reading size (a
/// bigger type setting keeps the same ~characters-per-line), like the web's `max-width: 42.5em`.
/// 42.5 × the 18px default reading size = 765px.
const READ_MEASURE_EM: f32 = 42.5;

/// Heading text size by level — the web's ladder (1.75/1.45/1.25/1.1 em). Shared by the
/// heading arm and the view loop's air-above-headings computation.
fn read_heading_size(level: u8, body: f32) -> f32 {
    match level {
        1 => body * 1.75,
        2 => body * 1.45,
        3 => body * 1.25,
        _ => body * 1.1,
    }
}

fn read_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("read-view")
}

/// The container wrapping the block that carries the reading-position bar this frame — the
/// [`ReadRevealProbe`]'s measurement anchor. Exactly one per view (the focused band, or the
/// focused list item's wrapper).
fn read_focus_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("read-focus")
}

/// The *focused* code panel's horizontal scrollable — Left/Right pan it
/// (docs/markdown-view.md §2.3); at most one panel carries the id per frame, and the
/// `scroll_by` no-ops when the focus isn't a code block.
fn read_code_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("read-code-scroll")
}

/// Sentinel link payload (`{prefix}{span.start}`) for rich-text runs whose click should
/// *arm* (focus) rather than open: internal links, inline-image chips, footnote refs — the
/// web's click-to-arm, for runs that live inside `rich_text` where only links are clickable.
/// The read view's message map intercepts it; hover popovers route it to `OpenLink`, whose
/// scheme allow-list drops it.
const READ_ARM_PREFIX: &str = "aether-arm:";

/// Measure-then-reveal for the reading view (docs/markdown-view.md §2.7): captures the read
/// scrollable's viewport + current offset and the [`read_focus_id`] container's real bounds
/// (scrollable children operate in untranslated content coordinates), and finishes with the
/// absolute offset that rests the block ~20% down the viewport — `None` when it's already
/// comfortably visible. Replaces the source-byte-fraction snap, which drifted off screen as
/// soon as images and code panels made block heights non-uniform. Safe to run from the reveal
/// task: the winit runtime executes widget operations *after* rebuilding the view, so the
/// probe always measures the freshly focused block.
#[derive(Default)]
struct ReadRevealProbe {
    /// The read scrollable's `(viewport, current translation, content bounds)`.
    viewport: Option<(iced::Rectangle, iced::Vector, iced::Rectangle)>,
    focus: Option<iced::Rectangle>,
    /// `None` = reveal (skip when comfortably visible, rest ~20% down); `Some(place)` =
    /// explicit edge-matched placement (`;`/`Alt-;`): always reposition, leaving
    /// [`ViewportPlace::READ_GAP`] between the view's edge and the block's matching edge
    /// (top-to-top for `Upper`, bottom-to-bottom for `Lower`).
    place: Option<ViewportPlace>,
}

impl iced::advanced::widget::Operation<Option<f32>> for ReadRevealProbe {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<Option<f32>>),
    ) {
        operate(self);
    }

    fn container(&mut self, id: Option<&iced::advanced::widget::Id>, bounds: iced::Rectangle) {
        if id == Some(&read_focus_id()) {
            self.focus = Some(bounds);
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        content_bounds: iced::Rectangle,
        translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&read_scroll_id()) {
            self.viewport = Some((bounds, translation, content_bounds));
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<Option<f32>> {
        use iced::advanced::widget::operation::Outcome;
        let (Some((view, translation, content)), Some(focus)) = (self.viewport, self.focus) else {
            return Outcome::Some(None);
        };
        let y = focus.y - content.y; // the block's y in content space
        let top = y - translation.y; // …relative to the viewport
        let margin = 48.0_f32.min(view.height * 0.08);
        if let Some(place) = self.place {
            // Explicit edge-matched placement: READ_GAP between the view edge and the block's
            // matching edge — a tall block placed "near the bottom" really ends there.
            let gap = view.height * ViewportPlace::READ_GAP;
            let raw = match place {
                ViewportPlace::Upper => y - gap,
                ViewportPlace::Lower => (y + focus.height) - (view.height - gap),
            };
            let offset = raw.clamp(0.0, (content.height - view.height).max(0.0));
            return Outcome::Some(Some(offset));
        }
        if top >= margin && top + focus.height <= view.height - margin {
            return Outcome::Some(None); // comfortably visible — don't fight manual scrolling
        }
        // Rest ~20% down (the editor's jump placement); an element taller than the viewport
        // pins nearer the top. Mirrors the web shell's revealFocus math.
        let rest = (view.height * 0.2).min((view.height - focus.height - margin).max(margin));
        let offset = (y - rest).clamp(0.0, (content.height - view.height).max(0.0));
        Outcome::Some(Some(offset))
    }
}

impl App {
    /// The reading-view document: a centered, measure-capped column of typographic blocks in a
    /// scrollable. The focused element's top-level block is tinted (focus is derived from the
    /// server cursor; per-span focus painting is renderer polish).
    fn read_view(&self) -> Element<'_, Message> {
        let ui = self.ui();
        let Some(read) = self.session.read.as_ref() else {
            return iced::widget::Space::new().into();
        };
        let body = self.session.buffer_font_size as f32 * READ_SCALE;
        // Two projections of the one server cursor (docs/markdown-view.md §1.3): the block bar
        // always marks the reading position; the target pill inverts the interactive span the
        // cursor sits inside, on top of it.
        let cursor = self.session.buffer.cursor.position;
        let block_span = read.block_focus(cursor).map(|i| read.elements[i].span());
        let target_span = read.target_focus(cursor).map(|i| read.elements[i].span());
        // Full-width rows: each block is a window-wide band (the selection tint fills it)
        // that centers its own measure-capped content column.
        let mut col = column![].spacing(body * 0.8);
        if read.loading && read.blocks.is_empty() {
            col = col.push(
                container(text("Loading…").size(body).color(theme::NORD3))
                    .width(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center),
            );
        }
        for (i, b) in read.blocks.iter().enumerate() {
            // The position bar sits on the block — except lists, whose items bar individually
            // inside `read_block` (item-grain position).
            let focused = !matches!(b, MdBlock::List { .. }) && block_span == Some(b.span());
            let block = self.read_block(b, body, ui, block_span, target_span);
            // Clicking a block focuses it (list items carry their own inner mouse areas, which
            // capture the press first — see `read_block`'s List arm).
            let block: Element<'_, ReadMsg> = iced::widget::mouse_area(block)
                .on_press(ReadMsg::Click(b.span().start))
                .into();
            // The reading cursor is a frost bar down the block's left edge (settled on after
            // trying tints and full-width bands). Lists skip the block-level wrap — their items
            // carry their own (see the List arm), and nesting the two would indent item bars a
            // wrapper-inset right of every other bar.
            let wrapped: Element<'_, ReadMsg> = if matches!(b, MdBlock::List { .. }) {
                block
            } else {
                read_focus_wrap(focused, block)
            };
            // Air above headings (web parity: `margin: 1.6em 0 0.5em` in the heading's own
            // size — sections breathe): the uniform column spacing supplies 0.8 body of the
            // gap; the rest rides the band's top padding, outside the focus wrap so the bar
            // and the click target still hug the heading itself. The document's first block
            // keeps the plain padding (the web strips `:first-child` margin the same way).
            let air = match b {
                MdBlock::Heading { level, .. } if i > 0 => {
                    read_heading_size(*level, body) * 1.6 - body * 0.8
                }
                _ => 0.0,
            };
            let inner = container(wrapped)
                .width(Length::Fill)
                .max_width(body * READ_MEASURE_EM)
                .padding(iced::Padding {
                    top: 3.0 + air,
                    bottom: 3.0,
                    left: 16.0,
                    right: 16.0,
                });
            let outer = container(inner)
                .width(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center);
            // The focused band anchors the reveal probe (lists anchor their focused item
            // inside `read_block` instead).
            let outer = if focused {
                outer.id(read_focus_id())
            } else {
                outer
            };
            col = col.push(outer);
        }
        Element::from(
            iced::widget::scrollable(container(col).width(Length::Fill).padding([24, 0]))
                .id(read_scroll_id())
                // Styled explicitly — the default scrollbar is 10px of chrome; the document
                // scroll is buffer-level, so it gets the editor tier.
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::new()
                        .width(theme::SCROLLBAR_W)
                        .margin(0)
                        .scroller_width(theme::SCROLLBAR_W),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .on_scroll(|vp| {
                    let max = (vp.content_bounds().height - vp.bounds().height).max(0.0);
                    ReadMsg::Scrolled(vp.absolute_offset().y, max)
                }),
        )
        .map(|m| match m {
            ReadMsg::Click(byte) => Message::ReadClick(byte),
            // A click on an internal link / inline-image chip / footnote ref — the sentinel
            // carries the span start; the core follows links/refs like Enter (images arm).
            ReadMsg::Link(url) => match url
                .strip_prefix(READ_ARM_PREFIX)
                .and_then(|b| b.parse::<u32>().ok())
            {
                Some(byte) => Message::ReadClickActivate(byte),
                None => Message::OpenLink(url),
            },
            ReadMsg::Scrolled(y, max) => Message::ReadScrolled { y, max },
        })
    }

    /// One reading-view block. Reuses the hover renderers for inline runs; headings, tables and
    /// images get document-scale treatment. `block`/`target` are the two focus projections
    /// (position bar / Enter-target pill) — see `read_view`.
    fn read_block(
        &self,
        b: &MdBlock,
        body: f32,
        ui: theme::Ui,
        block: Option<MdSpan>,
        target: Option<MdSpan>,
    ) -> Element<'static, ReadMsg> {
        match b {
            MdBlock::Heading { level, content, .. } => {
                let size = read_heading_size(*level, body);
                // The terminal's heading colour ladder: frost blue majors, teal H3, white
                // H4, body-grey H5/H6 — colour distinguishes the minor levels, which share
                // the smallest size.
                let color = match level {
                    1 | 2 => theme::NORD8,
                    3 => theme::NORD7,
                    4 => theme::NORD6,
                    _ => theme::NORD4,
                };
                let h = md_rich_in(
                    content,
                    true,
                    color,
                    size,
                    READ_FONT_FAMILY,
                    1.3, // headings stay tight; the body carries the airiness
                    target,
                    ReadMsg::Link,
                );
                if *level == 2 {
                    // Second-level headings carry an underline rule (matches the web's h2
                    // border and the terminal's `─` row).
                    column![
                        h,
                        container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(1)
                            .style(|_| container::Style {
                                background: Some(theme::NORD2.into()),
                                ..container::Style::default()
                            }),
                    ]
                    .spacing(6)
                    .into()
                } else {
                    h
                }
            }
            MdBlock::Paragraph { content, .. } => md_rich_in(
                content,
                false,
                theme::NORD4,
                body,
                READ_FONT_FAMILY,
                READ_LINE_HEIGHT,
                target,
                ReadMsg::Link,
            ),
            MdBlock::Table {
                alignments,
                head,
                rows,
                ..
            } => self.read_table(alignments, head, rows, body, target),
            MdBlock::Image {
                src,
                alt,
                inner_span,
                ..
            } => self.read_image(src, alt, *inner_span, body, target),
            MdBlock::List {
                ordered,
                start,
                items,
                ..
            } => {
                let mut col = column![].spacing(body * 0.35);
                for (i, item) in items.iter().enumerate() {
                    let mut marker = if *ordered {
                        format!("{}.", start + i as u64)
                    } else {
                        "•".to_string()
                    };
                    if let Some(done) = item.checked {
                        marker = if done { "☑".into() } else { "☐".into() };
                    }
                    let mut inner = column![].spacing(body * 0.35);
                    for ib in &item.blocks {
                        inner = inner.push(self.read_block(ib, body, ui, block, target));
                    }
                    // Item-grain click focus: this inner area captures the press before the
                    // whole-block area the outer loop wraps the list in. The position bar
                    // rides the same wrapper, so focusing an item marks *it*, not the list —
                    // and equality (not containment) keeps a nested item's bar off its parents.
                    let item_focused = block == Some(item.span);
                    let area = iced::widget::mouse_area(read_focus_wrap(
                        item_focused,
                        row![text(marker).size(body).color(theme::NORD4), inner,]
                            .spacing(8)
                            .into(),
                    ))
                    .on_press(ReadMsg::Click(item.span.start));
                    // The focused item anchors the reveal probe (item grain, not the list).
                    col = col.push(if item_focused {
                        Element::from(container(area).width(Length::Fill).id(read_focus_id()))
                    } else {
                        area.into()
                    });
                }
                col.into()
            }
            MdBlock::Quote { alert, content, .. } => {
                let mut inner = column![].spacing(body * 0.5);
                if let Some(kind) = alert {
                    // The alert's label row: the capitalized kind in its colour, semibold —
                    // the web's `.md-alert-label`, the terminal's AlertLabel span.
                    let (label, color) = alert_style(*kind);
                    inner =
                        inner.push(text(label).size(body * 0.95).color(color).font(iced::Font {
                            family: READ_FONT_FAMILY,
                            weight: iced::font::Weight::Semibold,
                            ..iced::Font::DEFAULT
                        }));
                }
                for cb in content {
                    inner = inner.push(self.read_block(cb, body, ui, block, target));
                }
                // A left bar, no panel shade (Joe's call: quotes read fine as bar + indent).
                // Same nested-container construction as `read_focus_wrap` — `md_bar()`'s Fill
                // height would blow up under the read scrollable's unbounded limits (the bug
                // that blanked lists): the outer paints the bar colour, the inner repaints the
                // canvas over everything but the 3px strip. Square corners, matching the
                // position bar. Alerts colour the bar by kind, matching their label.
                let bar = alert.map(|k| alert_style(k).1).unwrap_or(theme::NORD3);
                let panel = container(inner)
                    .width(Length::Fill)
                    .padding([8, 10])
                    .style(|_| container::Style {
                        background: Some(theme::NORD0.into()),
                        ..container::Style::default()
                    });
                container(panel)
                    .width(Length::Fill)
                    .padding(iced::Padding {
                        left: 3.0,
                        ..iced::Padding::ZERO
                    })
                    .style(move |_| container::Style {
                        background: Some(bar.into()),
                        ..container::Style::default()
                    })
                    .into()
            }
            MdBlock::Code {
                language,
                code,
                span,
            } => {
                // The panel's inset lives on the tag and the *scrollable content*, not the
                // panel container — the scrollable then spans the panel edge-to-edge, so its
                // bar sits flush with the block instead of floating inside the padding (and
                // the content's bottom inset gives the overlay bar a text-free strip).
                let mut panel = column![].spacing(4.0);
                if let Some(lang) = language {
                    panel = panel.push(
                        container(
                            text(lang.clone())
                                .font(iced::Font::MONOSPACE)
                                .size(body * 0.65)
                                .color(theme::NORD3_BRIGHT),
                        )
                        .padding(iced::Padding {
                            top: 10.0,
                            right: 12.0,
                            bottom: 0.0,
                            left: 12.0,
                        }),
                    );
                }
                // Tree-sitter runs when the server's snippet highlights have landed for this
                // fence (docs/markdown-view.md §2.8) — the editor's own token colours; plain
                // NORD4 monospace until then.
                let hls = self
                    .session
                    .read
                    .as_ref()
                    .and_then(|r| r.code_highlights.get(&span.start))
                    .filter(|h| !h.is_empty());
                let code_el: Element<'static, ReadMsg> = match hls {
                    Some(hls) => {
                        let mono = iced::Font::MONOSPACE;
                        let mut spans: Vec<iced::advanced::text::Span<'static, String>> =
                            Vec::new();
                        let mut push = |s: &str, color: iced::Color| {
                            if !s.is_empty() {
                                spans.push(
                                    iced::widget::span(s.to_string()).font(mono).color(color),
                                );
                            }
                        };
                        let mut pos = 0usize;
                        for h in hls {
                            let s = (h.start as usize).min(code.len());
                            let e = (h.end as usize).clamp(s, code.len());
                            push(&code[pos..s], theme::NORD4);
                            let color = theme::highlight_color(&h.kind).unwrap_or(theme::NORD4);
                            push(&code[s..e], color);
                            pos = e;
                        }
                        push(&code[pos..], theme::NORD4);
                        iced::widget::rich_text(spans)
                            .size(body * 0.85)
                            .wrapping(iced::widget::text::Wrapping::None)
                            .into()
                    }
                    None => text(code.clone())
                        .font(iced::Font::MONOSPACE)
                        .size(body * 0.85)
                        .color(theme::NORD4)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                };
                // Long lines don't wrap — the panel scrolls horizontally, like the web's
                // `<pre>`. A vertical wheel passes through: a scrollable only captures events
                // that actually scrolled it. The code's inset rides *inside* the scroll
                // content (Shrink — Fill under the unbounded horizontal limits is the layout
                // landmine), so text leads in/out by 12px at either scroll end.
                let code_padded = container(code_el).padding(iced::Padding {
                    top: if language.is_some() { 0.0 } else { 10.0 },
                    right: 12.0,
                    bottom: 10.0,
                    left: 12.0,
                });
                let scroll = iced::widget::scrollable(code_padded)
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        iced::widget::scrollable::Scrollbar::new()
                            .width(theme::SCROLLBAR_INLINE_W)
                            .scroller_width(theme::SCROLLBAR_INLINE_W),
                    ))
                    .width(Length::Fill);
                // The focused panel is Left/Right's pan target (see `read_code_scroll_id`).
                let scroll = if block == Some(*span) {
                    scroll.id(read_code_scroll_id())
                } else {
                    scroll
                };
                panel = panel.push(scroll);
                container(panel)
                    .width(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(theme::NORD1.into()),
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..iced::Border::default()
                        },
                        ..container::Style::default()
                    })
                    .into()
            }
            // Document-scale rule: the 1px line alone gives the focus bar nothing to stand
            // next to (a ~1px bar is invisible), so it sits in vertical padding.
            MdBlock::Rule { .. } => container(
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(1)
                    .style(md_bar_style),
            )
            .width(Length::Fill)
            .padding([body * 0.5, 0.0])
            .into(),
            // Front matter: the dim literal panel (docs/markdown-view.md) — raw YAML in dim
            // monospace behind a thin NORD2 rule, the web's `.md-front-matter`. The quote
            // arm's nested-container bar construction, but 2px and NORD2: literal metadata,
            // not speech. Must not fall through to `md_block`, whose hover-scale arm hides
            // front matter entirely (right for popovers, wrong here).
            MdBlock::FrontMatter { text: raw, .. } => {
                let panel = container(
                    text(raw.trim_end().to_string())
                        .size(body * 0.8)
                        .color(theme::NORD3_BRIGHT)
                        .font(iced::Font::MONOSPACE),
                )
                .width(Length::Fill)
                .padding([4, 10])
                .style(|_| container::Style {
                    background: Some(theme::NORD0.into()),
                    ..container::Style::default()
                });
                container(panel)
                    .width(Length::Fill)
                    .padding(iced::Padding {
                        left: 2.0,
                        ..iced::Padding::ZERO
                    })
                    .style(|_| container::Style {
                        background: Some(theme::NORD2.into()),
                        ..container::Style::default()
                    })
                    .into()
            }
            // The remaining kinds read fine at hover scale.
            other => md_block(other, ui, ReadMsg::Link),
        }
    }

    /// A real table: header row bold on a panel, body rows striped, columns at fixed
    /// pixel-estimated widths — natural content width capped at ~22 em, never below the widest
    /// unbreakable token (a cell can't wrap inside a word, and iced doesn't clip overflow, so
    /// underestimating painted cells over their neighbours — the old char-count weighting also
    /// undercounted monospace code chips). A table wider than the measure scrolls horizontally
    /// (the web's `.md-table-scroll` behaviour). Everything inside the scrollable sizes Shrink:
    /// `Fill` under its unbounded horizontal limits is the layout landmine.
    fn read_table(
        &self,
        _alignments: &[aether_client::markdown::ColAlign],
        head: &[Vec<MdInline>],
        rows: &[Vec<Vec<MdInline>>],
        body: f32,
        target: Option<MdSpan>,
    ) -> Element<'static, ReadMsg> {
        let ncols = rows
            .iter()
            .map(|r| r.len())
            .chain(std::iter::once(head.len()))
            .max()
            .unwrap_or(0);
        if ncols == 0 {
            return iced::widget::Space::new().into();
        }
        let cell_size = body * 0.92;
        let empty = Vec::new();
        let mut widths = vec![0f32; ncols];
        for (ci, w) in widths.iter_mut().enumerate() {
            let mut natural = 0f32;
            let mut minimum = 0f32;
            if let Some(c) = head.get(ci) {
                // Headers run bold — a nudge wider.
                let (n, m) = cell_text_width(c, cell_size * 1.05);
                natural = natural.max(n);
                minimum = minimum.max(m);
            }
            for r in rows {
                if let Some(c) = r.get(ci) {
                    let (n, m) = cell_text_width(c, cell_size);
                    natural = natural.max(n);
                    minimum = minimum.max(m);
                }
            }
            // 18px of cell padding plus estimate slack.
            *w = natural.min(body * 22.0).max(minimum) + 22.0;
        }
        let cell = |content: &[MdInline], header: bool, w: f32| -> Element<'static, ReadMsg> {
            let color = if header { theme::NORD6 } else { theme::NORD4 };
            container(md_rich_in(
                content,
                header,
                color,
                cell_size,
                READ_FONT_FAMILY,
                1.4,
                target,
                ReadMsg::Link,
            ))
            .width(Length::Fixed(w))
            .padding([5, 9])
            .into()
        };
        let mut col = column![];
        if !head.is_empty() {
            let mut r = row![];
            for (ci, w) in widths.iter().enumerate() {
                r = r.push(cell(head.get(ci).unwrap_or(&empty), true, *w));
            }
            col = col.push(container(r).style(|_| container::Style {
                background: Some(theme::NORD1.into()),
                ..container::Style::default()
            }));
        }
        for (ri, row_cells) in rows.iter().enumerate() {
            let mut r = row![];
            for (ci, w) in widths.iter().enumerate() {
                let c = row_cells.get(ci).unwrap_or(&empty);
                r = r.push(cell(c, false, *w));
            }
            let striped = ri % 2 == 1;
            col = col.push(container(r).style(move |_| container::Style {
                background: striped.then(|| theme::NORD1.scale_alpha(0.4).into()),
                ..container::Style::default()
            }));
        }
        let framed = container(col).style(|_| container::Style {
            border: iced::Border {
                color: theme::NORD3,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        });
        iced::widget::scrollable(framed)
            .direction(iced::widget::scrollable::Direction::Horizontal(
                iced::widget::scrollable::Scrollbar::new()
                    .width(theme::SCROLLBAR_INLINE_W)
                    .scroller_width(theme::SCROLLBAR_INLINE_W),
            ))
            .width(Length::Fill)
            .into()
    }

    /// A display image: relative sources resolve against the buffer's directory and load from
    /// disk; remote http(s) sources come from the session fetch cache ([`RemoteImage`], filled
    /// by the fan-out in `run_core`); anything else renders as a placeholder. SVGs — local or
    /// remote — ride `widget::svg` (the raster decoder doesn't read them). The frost ring
    /// appears only when the image is *armed* (`l` — its inner markup span is the target):
    /// display images join the opt-in model, so the ring means "Enter acts here".
    fn read_image(
        &self,
        src: &str,
        alt: &str,
        inner: MdSpan,
        body: f32,
        target: Option<MdSpan>,
    ) -> Element<'static, ReadMsg> {
        let label = if alt.is_empty() { "image" } else { alt };
        let placeholder = |note: &str| -> Element<'static, ReadMsg> {
            text(format!("▨ [{label}]  ({note})"))
                .size(body * 0.9)
                .color(theme::NORD3)
                .into()
        };
        let lower = src.to_ascii_lowercase();
        let el: Element<'static, ReadMsg> =
            if lower.starts_with("http://") || lower.starts_with("https://") {
                match self.remote_images.get(src) {
                    Some(RemoteImage::Raster(h)) => {
                        iced::widget::image(h.clone()).width(Length::Fill).into()
                    }
                    Some(RemoteImage::Svg(h)) => iced::widget::svg(h.clone())
                        .width(Length::Fill)
                        .height(Length::Shrink)
                        .into(),
                    Some(RemoteImage::Loading) => placeholder("loading…"),
                    Some(RemoteImage::Failed) | None => placeholder(src),
                }
            } else {
                // Local sources resolve through the core's link resolution (buffer-dir for
                // relative, workspace-root for a leading `/` — docs/markdown-view.md §2.4),
                // so images and links can't drift. `//host` protocol-relative isn't local
                // (no scheme, but not a path either) — it falls to the placeholder via the
                // exists() filter after the root-join defangs it.
                let external = src.contains("://");
                let resolved = (!external)
                    .then(|| {
                        self.session
                            .read_resolve_path(src)
                            .map(std::path::PathBuf::from)
                    })
                    .flatten()
                    .filter(|p| p.exists());
                match resolved {
                    Some(path) if lower.ends_with(".svg") => {
                        iced::widget::svg(iced::widget::svg::Handle::from_path(path))
                            .width(Length::Fill)
                            .height(Length::Shrink)
                            .into()
                    }
                    Some(path) => iced::widget::image(iced::widget::image::Handle::from_path(path))
                        .width(Length::Fill)
                        .into(),
                    None => placeholder(src),
                }
            };
        // The ring frame is ALWAYS present (transparent border when unarmed) so arming is a
        // paint-only change: a wrapper that appears on arm would inset the image, shrinking
        // it and forcing a relayout + re-scale — the same reserve-the-space trick as the
        // focus bar. Padding ≥ border width keeps the ring off the image's edge.
        let armed = target == Some(inner);
        container(el)
            .width(Length::Fill)
            .padding(3)
            .style(move |_| container::Style {
                border: iced::Border {
                    color: if armed {
                        theme::NORD8
                    } else {
                        iced::Color::TRANSPARENT
                    },
                    width: 3.0,
                    radius: 2.0.into(),
                },
                ..container::Style::default()
            })
            .into()
    }
}

/// A fetched remote reading-view image (docs/markdown-view.md §2.8), keyed by URL in
/// [`App::remote_images`].
enum RemoteImage {
    Loading,
    Raster(iced::widget::image::Handle),
    Svg(iced::widget::svg::Handle),
    Failed,
}

/// Every remote (http/https) display-image source in the document, deduplicated, recursively —
/// the reading view's fetch fan-out (inline images render as text, so only block images count).
fn remote_image_sources(blocks: &[MdBlock]) -> Vec<String> {
    fn walk(blocks: &[MdBlock], out: &mut Vec<String>) {
        for b in blocks {
            match b {
                MdBlock::Image { src, .. } => {
                    let lower = src.to_ascii_lowercase();
                    if (lower.starts_with("http://") || lower.starts_with("https://"))
                        && !out.contains(src)
                    {
                        out.push(src.clone());
                    }
                }
                MdBlock::Quote { content, .. } | MdBlock::FootnoteDef { content, .. } => {
                    walk(content, out)
                }
                MdBlock::List { items, .. } => {
                    for item in items {
                        walk(&item.blocks, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(blocks, &mut out);
    out
}

/// Download a remote reading-view image on the blocking pool: 15s timeout, 20 MB cap — a hung
/// or huge download must not wedge anything.
async fn fetch_remote_image(url: String) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let agent = ureq::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build();
        let resp = agent.get(&url).call().map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(20 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A thin Nord3 bar (the blockquote rule / horizontal rule fill).
fn md_bar<M: 'static>() -> Element<'static, M> {
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
/// Hover popovers render sans (the UI face); the reading view passes serif via [`md_rich_in`].
fn md_rich<M: 'static>(
    inlines: &[MdInline],
    bold: bool,
    base_color: iced::Color,
    size: f32,
    on_link: fn(String) -> M,
) -> Element<'static, M> {
    // Hover density: iced's default line height.
    md_rich_in(
        inlines,
        bold,
        base_color,
        size,
        iced::font::Family::SansSerif,
        1.3,
        None, // hover has no reading target
        on_link,
    )
}

#[allow(clippy::too_many_arguments)] // same styling-parameter family as `md_spans` below
fn md_rich_in<M: 'static>(
    inlines: &[MdInline],
    bold: bool,
    base_color: iced::Color,
    size: f32,
    family: iced::font::Family,
    line_height: f32,
    target: Option<MdSpan>,
    on_link: fn(String) -> M,
) -> Element<'static, M> {
    let mut spans = Vec::new();
    md_spans(
        inlines, bold, false, None, base_color, size, family, target, &mut spans,
    );
    iced::widget::rich_text(spans)
        .size(size)
        .line_height(iced::widget::text::LineHeight::Relative(line_height))
        .on_link_click(on_link)
        .into()
}

/// The reading view's body line height — matches the web reading view's 1.65.
const READ_LINE_HEIGHT: f32 = 1.65;

/// The reading view's body face — bundled Source Serif 4 (loaded with the JetBrains Mono
/// faces at boot; OFL, see fonts/OFL-SourceSerif4.txt).
const READ_FONT_FAMILY: iced::font::Family = iced::font::Family::Name("Source Serif 4");

/// Alert kind → (label, colour) — the ladder shared with the terminal's `alert_color` and
/// the web's `.md-alert-*` rules.
fn alert_style(kind: AlertKind) -> (&'static str, iced::Color) {
    match kind {
        AlertKind::Note => ("Note", theme::NORD8),
        AlertKind::Tip => ("Tip", theme::NORD14),
        AlertKind::Important => ("Important", theme::NORD15),
        AlertKind::Warning => ("Warning", theme::NORD13),
        AlertKind::Caution => ("Caution", theme::NORD11),
    }
}

/// The reading cursor: a 3px frost bar down the content's left edge. Built as two nested
/// containers — the outer paints the bar colour and insets the content by 3px, the inner
/// repaints the canvas over everything but that strip — because a `Fill`-height sibling
/// resolves against the scrollable's *unbounded* vertical limits and destroys the layout
/// (list rows collapsed to nothing). The strip is always reserved, so focus moves shift
/// nothing.
fn read_focus_wrap(on: bool, content: Element<'static, ReadMsg>) -> Element<'static, ReadMsg> {
    let inner = container(content)
        .width(Length::Fill)
        .padding(iced::Padding {
            left: 10.0,
            ..iced::Padding::ZERO
        })
        .style(|_| container::Style {
            background: Some(theme::NORD0.into()),
            ..container::Style::default()
        });
    container(inner)
        .width(Length::Fill)
        .padding(iced::Padding {
            left: 3.0,
            ..iced::Padding::ZERO
        })
        .style(move |_| container::Style {
            background: on.then(|| theme::NORD8.into()),
            ..container::Style::default()
        })
        .into()
}

/// Reading-view widget messages — a tiny `Clone` type so `mouse_area`/link spans can ride the
/// widget tree ([`Message`] itself can't be `Clone`: boot variants carry channels). `read_view`
/// maps them into [`Message`] at its boundary.
#[derive(Debug, Clone)]
enum ReadMsg {
    /// Focus the element whose source span starts here.
    Click(u32),
    /// Open a link href (external schemes only; the handler allow-lists).
    Link(String),
    /// The document scrollable scrolled: `(offset_y, max_scroll)` for the shell's mirror.
    Scrolled(f32, f32),
}

/// `target` is the reading view's Enter-target span (docs/markdown-view.md §1.3): the link,
/// inline image or footnote ref whose span matches renders as an inverted pill on top of the
/// block bar. Hover popovers pass `None`.
#[allow(clippy::too_many_arguments)]
fn md_spans(
    inlines: &[MdInline],
    bold: bool,
    italic: bool,
    link: Option<&str>,
    base: iced::Color,
    size: f32,
    family: iced::font::Family,
    target: Option<MdSpan>,
    out: &mut Vec<iced::advanced::text::Span<'static, String>>,
) {
    for inl in inlines {
        match inl {
            MdInline::Text { text } => {
                out.push(md_span(text, bold, italic, false, link, base, family))
            }
            MdInline::Code { text } => {
                out.push(md_span(text, bold, italic, true, link, base, family))
            }
            MdInline::Strong { content } => {
                md_spans(content, true, italic, link, base, size, family, target, out)
            }
            MdInline::Emphasis { content } => {
                md_spans(content, bold, true, link, base, size, family, target, out)
            }
            MdInline::Link {
                href,
                content,
                span,
            } => {
                // External hrefs are real link payloads (click opens); internal targets
                // (relative paths, anchors) can't open externally — their clicks *arm*
                // instead, via the [`READ_ARM_PREFIX`] sentinel the read view intercepts.
                let external = ["http://", "https://", "mailto:"]
                    .iter()
                    .any(|p| href.len() >= p.len() && href[..p.len()].eq_ignore_ascii_case(p));
                let value = if external {
                    href.clone()
                } else {
                    format!("{READ_ARM_PREFIX}{}", span.start)
                };
                if target == Some(*span) {
                    // The targeted link: invert its whole run (background pill, canvas text).
                    let mut inner = Vec::new();
                    md_spans(
                        content,
                        bold,
                        italic,
                        Some(&value),
                        base,
                        size,
                        family,
                        None,
                        &mut inner,
                    );
                    out.extend(inner.into_iter().map(|s| read_pill(s, size)));
                } else {
                    md_spans(
                        content,
                        bold,
                        italic,
                        Some(&value),
                        base,
                        size,
                        family,
                        target,
                        out,
                    )
                }
            }
            MdInline::Strikethrough { content } => {
                let mut inner = Vec::new();
                md_spans(
                    content, bold, italic, link, base, size, family, target, &mut inner,
                );
                out.extend(inner.into_iter().map(|s| s.strikethrough(true)));
            }
            MdInline::Image { alt, span, .. } => {
                // A text chip — iced's rich_text is text-runs-only, so a real inline image
                // can't ride a paragraph. ▨ marks it as an image (matching the TUI); the arm
                // sentinel makes it clickable (click focuses it, like the web/TUI).
                let s = md_span(
                    &format!("▨ [{alt}]"),
                    bold,
                    italic,
                    false,
                    None,
                    theme::NORD3,
                    family,
                )
                .link(format!("{READ_ARM_PREFIX}{}", span.start));
                out.push(if target == Some(*span) {
                    read_pill(s, size)
                } else {
                    s
                });
            }
            MdInline::FootnoteRef { label, span } => {
                let s = md_span(
                    &format!("[{label}]"),
                    bold,
                    italic,
                    false,
                    None,
                    theme::NORD3,
                    family,
                )
                .link(format!("{READ_ARM_PREFIX}{}", span.start));
                out.push(if target == Some(*span) {
                    read_pill(s, size)
                } else {
                    s
                });
            }
            MdInline::HardBreak => out.push(md_span("\n", bold, italic, false, link, base, family)),
        }
    }
}

/// The armed-target pill (inverted run) hugged to its text. iced draws a span highlight
/// over the full line-box region — 1.65 line-height in the read body, which read as a
/// chunky lozenge — so negative vertical padding shaves it back toward the em box, and a
/// 1px horizontal pad gives the glyphs the same hair of breathing as the web pill's
/// 1px outline. Padding is draw-only (span bounds are unchanged), so arming shifts nothing.
fn read_pill(
    s: iced::advanced::text::Span<'static, String>,
    size: f32,
) -> iced::advanced::text::Span<'static, String> {
    s.background(theme::NORD8)
        .color(theme::NORD0)
        .border(iced::border::rounded(2))
        .padding(iced::Padding {
            top: -0.2 * size,
            bottom: -0.2 * size,
            left: 1.0,
            right: 1.0,
        })
}

fn md_span(
    text: &str,
    bold: bool,
    italic: bool,
    code: bool,
    link: Option<&str>,
    base: iced::Color,
    family: iced::font::Family,
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
            family,
            ..iced::Font::default()
        }
    };
    let color = if link.is_some() {
        theme::NORD9
    } else if code {
        theme::NORD4
    } else {
        base
    };
    let mut s = iced::widget::span(text.to_string()).font(font).color(color);
    if code {
        // The web reading view's inline-code chip: body-coloured text on the panel shade.
        s = s.background(theme::NORD1);
    }
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
        MdBlock::Heading { content, .. } | MdBlock::Paragraph { content, .. } => {
            1 + md_text_len(content) / 80
        }
        MdBlock::Code { code, .. } | MdBlock::Html { raw: code, .. } => {
            code.lines().count().max(1) + 1
        }
        MdBlock::List { items, .. } => items
            .iter()
            .map(|it| md_estimate(&it.blocks))
            .sum::<usize>()
            .max(1),
        MdBlock::Quote { content, .. } | MdBlock::FootnoteDef { content, .. } => {
            md_estimate(content)
        }
        MdBlock::Rule { .. } | MdBlock::Image { .. } => 1,
        MdBlock::Table { rows, .. } => rows.len() + 1,
        MdBlock::FrontMatter { .. } => 0,
    }
}

fn md_text_len(inlines: &[MdInline]) -> usize {
    inlines
        .iter()
        .map(|i| match i {
            MdInline::Text { text } | MdInline::Code { text } => text.len(),
            MdInline::Strong { content }
            | MdInline::Emphasis { content }
            | MdInline::Strikethrough { content }
            | MdInline::Link { content, .. } => md_text_len(content),
            MdInline::Image { alt, .. } => alt.len(),
            MdInline::FootnoteRef { label, .. } => label.len() + 2,
            MdInline::HardBreak => 1,
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

/// Spawn a detached `ae --gui` sibling seeded from a [`WindowTarget`] — the body behind
/// [`ShellAction::NewWindow`] (both the `Space z` duplicate and "open picker item in a new
/// window"; the core builds the target either way). The sibling dials the same daemon (`--profile`),
/// so buffers are shared server-side. Best-effort: a spawn failure is logged, not surfaced (the user
/// simply gets no new window).
fn spawn_target(target: &WindowTarget) {
    let Some(exe) = window_spawn_exe(
        std::env::current_exe().ok(),
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
    ) else {
        tracing::warn!("cannot open a new window: current exe path is unavailable");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    // `--profile` (global) before the implicit `edit` so the sibling joins the same daemon.
    cmd.arg("--profile")
        .arg(crate::active_profile())
        .arg("--gui");
    if let Some(ws) = &target.workspace {
        cmd.arg("--workspace").arg(ws);
    }
    match &target.open {
        // `PATH:LINE:COL` — 1-based on the CLI (the editor convention `ae src/main.rs:42:10`), which
        // `split_path_and_jump` parses back. No `:L:C` when there's no jump.
        WindowOpen::Path { path, at } => {
            cmd.arg(match at {
                Some((line, col)) => format!("{path}:{}:{}", line + 1, col + 1),
                None => path.clone(),
            });
        }
        WindowOpen::Buffer(id) => {
            cmd.arg("--buffer").arg(id.to_string());
        }
        WindowOpen::Workspace => {}
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut cmd);
    if let Err(e) = cmd.spawn() {
        tracing::warn!("failed to spawn new window: {e}");
    }
}

/// The executable to spawn a sibling window from. Three cases:
/// - **AppImage** (`current` inside `$APPDIR`): the image itself, not the transient FUSE mount
///   that belongs to *this* launch — the sibling gets its own mount and lifetime (mirrors the
///   `ae` binary's `server_spawn_exe`; the `starts_with` guard keeps an APPIMAGE var merely
///   inherited from some other AppImage'd parent from hijacking the spawn).
/// - **Linux**: `/proc/self/exe` — the binary we are *running*, not its (possibly stale) path.
///   `current_exe()` reads that link textually, and once a `cargo build` has replaced the
///   on-disk file it yields `…/ae (deleted)`, which ENOENTs on spawn — the "new window
///   silently does nothing after a rebuild" bug. Exec'ing the link itself resolves to the
///   live inode, so the sibling is the *same build* as this window.
/// - **Elsewhere**: `current_exe()`, unchanged.
fn window_spawn_exe(
    current: Option<std::path::PathBuf>,
    appimage: Option<std::ffi::OsString>,
    appdir: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    if let (Some(image), Some(dir)) = (appimage, appdir) {
        if current.as_ref().is_some_and(|c| c.starts_with(&dir)) {
            return Some(image.into());
        }
    }
    if cfg!(target_os = "linux") {
        return Some("/proc/self/exe".into());
    }
    current
}

/// Put a spawned window in its own process group, so a signal sent to *our* foreground group (a
/// terminal Ctrl-C) doesn't reach it — the GUI sibling lives or dies on its own. std's
/// `process_group` keeps this libc-free; on non-Unix the null stdio already decouples the child.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn detach(_cmd: &mut std::process::Command) {}

/// The hover popover's scrollable id, for programmatic `scroll_by` (keyboard panning).
fn hover_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("hover-scroll")
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

/// Scroll the picker's jumplist so the highlighted row is in view. `Minimal` moves the
/// least distance; `Top` aligns the row to the top unless it's already fully visible.
fn reveal_picker_selection(
    p: &PickerState,
    scroll_y: &mut f32,
    reveal: Reveal,
    ui: theme::Ui,
) -> Task<Message> {
    let Some(y) = reveal_target(p, *scroll_y, reveal, ui) else {
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
fn reveal_target(p: &PickerState, scroll_y: f32, reveal: Reveal, ui: theme::Ui) -> Option<f32> {
    let sd = p.selected_display_row()?;
    // Row-index × ROW_H, plus the inter-group gap pixels above the row (gaps sit outside the
    // display-row unit — same compensation as the overlay's spacers).
    let gaps =
        p.gaps_above_window() + p.gaps_before_display_rel(sd.saturating_sub(p.window_base()));
    let top = sd as f32 * ui.row_h() + gaps as f32 * crate::picker::GROUP_GAP;
    let bottom = top + ui.row_h();
    // Kinds that pin a sticky group header over the top row (grep's file path, Keybindings'
    // group label) need a revealed row to clear one row's height or it slides under the header
    // (the bug grep hit first, then Keybindings). Same predicate as the pin itself.
    let clearance = if crate::picker::pins_group_header(p.kind) {
        ui.row_h()
    } else {
        0.0
    };
    let m_top = (top - clearance).max(0.0);
    let h = crate::picker::list_height(p, ui);
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

/// Box geometry for the modal prompt overlay.
struct PromptBox {
    width: f32,
    /// Gap above the box — also the bottom margin [`PromptBox::max_h`] reserves.
    top: f32,
    /// Height cap; taller content scrolls inside the box instead of running off the window.
    max_h: f32,
}

/// Size and place the prompt box. Two shapes, because there are two kinds of prompt:
///
/// * A **question** (confirm / save-as / open-path) is short and wants to sit near eye level, so it
///   keeps the narrower box and the lower offset.
/// * The **info dialog** is a reference screen: it's wider (absolute paths would otherwise wrap
///   three times), and it sits at the picker's offset so the app's two "read something" overlays
///   line up rather than appearing at different heights.
///
/// Either way the height is capped to leave `top` clear at *both* ends, so the box is never taller
/// than the window — the cut-off the fixed-height version suffered in a small window. The floor
/// keeps it usable on a very short window, where scrolling inside a small box beats a box drawn
/// past the bottom edge.
fn prompt_box(info_dialog: bool, view_height: f32, ui: theme::Ui) -> PromptBox {
    let top = if info_dialog { 56.0 } else { 120.0 };
    PromptBox {
        width: ui.at(if info_dialog { 560.0 } else { 420.0 }),
        top,
        max_h: (view_height - top * 2.0).max(120.0),
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
        ConfirmKind::RemoveProject { path } => format!("Stop pinning project \"{path}\""),
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
fn spawn_connect_delayed(args: ConnectingBootstrap, attempt: u32) -> Task<Message> {
    Task::perform(
        async move {
            // Boot pacing, not the reconnect curve: we're usually racing the daemon this very
            // process just spawned, so poll fast at first (see `boot_backoff`).
            tokio::time::sleep(boot_backoff(attempt)).await;
            connect_and_bootstrap(args).await
        },
        Message::Booted,
    )
}

/// One boot-connect attempt: dial the fixed address, then (with a CLI workspace) activate it and
/// open the file / MRU buffer, or (without one) hand back a bare connection for the chooser.
/// Returns the connected [`Bootstrap`] to install, or a [`BootError`] to retry / surface. Bootstrap
/// RPC failures are `String` and fold into [`BootError::Retry`] via `?`; only a version-mismatch
/// dial is `Fatal`.
async fn connect_and_bootstrap(args: ConnectingBootstrap) -> Result<Bootstrap, BootError> {
    let base_url = args.server_url.clone();
    let (handle, rx) = crate::connection::connect(&base_url, &args.client_version)
        .await
        .map_err(BootError::from)?;
    let notifications = std::sync::Arc::new(tokio::sync::Mutex::new(rx));

    // No workspace on the CLI. A file outside any configured workspace (`ae /etc/hosts`) opens
    // directly in an ephemeral "(no workspace)" context — a missing path counts as a file to
    // create (`create_if_missing`: empty buffer, written at the first save). Otherwise (no file,
    // or a directory) hand back the bare connection so the chooser browses on it.
    let Some(workspace) = args.workspace.clone() else {
        let resolved = match &args.file {
            Some(f) => Some(resolve_cli_path(f)?),
            None => None,
        };
        if let Some(abs) = resolved.filter(|p| !p.is_dir()) {
            let opened = handle
                .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                    path: abs.display().to_string(),
                    transient: None,
                    create_if_missing: true,
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
                buffer: buffer_info(open, &workspace_paths),
                workspace: opened.workspace,
                explorer_dir: None,
                tethered: args.tether,
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

    let open = if let Some(bid) = args.buffer_id {
        // The scratch-buffer "open in new window": re-attach to the buffer by id (buffers are
        // daemon-global, so a fresh client can reach it). The id is daemon-session scoped, so a stale
        // one — the daemon restarted since the picker row was built — falls back to the MRU/scratch.
        match handle
            .rpc::<BufferOpen>(BufferOpenParams {
                buffer_id: Some(bid),
                ..Default::default()
            })
            .await
        {
            Ok(open) => open,
            Err(_) => handle
                .rpc::<BufferOpen>(BufferOpenParams {
                    buffer_id: activated.last_buffer_id,
                    transient: activated.last_buffer_id.is_none().then_some(true),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())?,
        }
    } else {
        match &resolved {
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
                    // Inside a workspace root: ordinary workspace-relative open (creating a
                    // missing file, like the terminal client). A `path:line:col` launch (or a
                    // grep-hit "open in new window") jumps to `jump_to` here.
                    Some((path_index, relative_path)) => handle
                        .rpc::<BufferOpen>(BufferOpenParams {
                            path_index: Some(path_index),
                            relative_path: Some(relative_path),
                            create_if_missing: true,
                            jump_to: args.jump_to,
                            ..Default::default()
                        })
                        .await
                        .map_err(|e| e.to_string())?,
                    // Outside the named workspace's roots: open as an external (guest) buffer in it.
                    // `workspace/open_path` carries no jump, so a `path:line:col` on an external file
                    // opens at the top.
                    None => handle
                        .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                            path: abs_str,
                            transient: None,
                            create_if_missing: true,
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
        }
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
        buffer: buffer_info(open, &workspace_paths),
        workspace: activated.workspace,
        explorer_dir,
        // Quick-edit launch (`ae file`, workspace inferred — `tether` is never set alongside an
        // explicit `--workspace`, and window-spawns always pass one): tether to the opened file.
        // A missing path is a file to create and tethers too; directory args (explorer over a
        // scratch) and `--buffer` re-attaches have no file.
        tethered: args.tether
            && args.buffer_id.is_none()
            && resolved.as_ref().is_some_and(|p| !p.is_dir()),
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
    match abs.canonicalize() {
        Ok(p) => Ok(p),
        // A not-yet-existing file (`ae path/to/new-file`) still resolves — deepest existing
        // ancestor canonicalized, missing tail kept — so the boot opens (which run with
        // `create_if_missing`) bind an empty create-on-save buffer to it. A trailing `/`
        // declares directory intent, which create-on-open can't satisfy: keep the error.
        Err(_) if !input.ends_with('/') => {
            canonicalize_partial(&abs).map_err(|e| format!("resolving {}: {e}", abs.display()))
        }
        Err(e) => Err(format!("resolving {}: {e}", abs.display())),
    }
}

/// Canonicalize a path that may not fully exist: walk up to the deepest existing ancestor,
/// canonicalize that, then re-attach the not-yet-created tail verbatim (mirrors the server's
/// helper of the same name).
fn canonicalize_partial(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&cursor) {
            Ok(canon) => {
                let mut out = canon;
                // suffix was accumulated tail-first; reverse on attach.
                for component in suffix.iter().rev() {
                    out.push(component);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                let Some(parent) = cursor.parent().map(|p| p.to_path_buf()) else {
                    return Err(e);
                };
                suffix.push(name);
                cursor = parent;
            }
            Err(e) => return Err(e),
        }
    }
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
            // Bundle JetBrains Mono for the editor (chrome stays on the default monospace). Registered
            // here so `Font::with_name("JetBrains Mono")` resolves; the editor toggles its ligatures
            // via shaping mode (see `editor::EDITOR_FONT`). All four faces (Regular/Bold/Italic/
            // Bold-Italic) are bundled so each weight/slant request resolves within the family —
            // without the matching face, cosmic-text falls back to a non-monospace family for
            // `text.strong` / `text.emphasis` runs.
            fonts: vec![
                include_bytes!("../fonts/JetBrainsMono-Regular.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/JetBrainsMono-Bold.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/JetBrainsMono-Italic.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/JetBrainsMono-BoldItalic.ttf")
                    .as_slice()
                    .into(),
                // Source Serif 4 (OFL, see fonts/OFL-SourceSerif4.txt): the reading view's body
                // face (docs/markdown-view.md §2.8). Regular/Italic for prose, Semibold+Bold so
                // heading and strong runs resolve inside the family rather than falling back.
                include_bytes!("../fonts/SourceSerif4-Regular.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/SourceSerif4-It.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/SourceSerif4-Semibold.ttf")
                    .as_slice()
                    .into(),
                include_bytes!("../fonts/SourceSerif4-Bold.ttf")
                    .as_slice()
                    .into(),
            ],
            default_font: iced::Font::MONOSPACE,
            default_text_size: iced::Pixels(14.0),
            antialiasing: true,
            ..iced::Settings::default()
        })
        .window(window_settings())
        .run()
}

/// Initial window settings: size, and on Linux the application id ("uk.joef.Aether") that becomes
/// the Wayland `app_id` / X11 `WM_CLASS`. Desktop environments match a running window to its
/// installed `uk.joef.Aether.desktop` entry (and thus its icon) by this exact string; iced's
/// default is an empty id, which matches nothing. Reverse-DNS per the freedesktop convention —
/// the same id names the desktop file and icon shipped in the AppImage, and is what a future
/// Flatpak would have to be called.
fn window_settings() -> iced::window::Settings {
    iced::window::Settings {
        size: Size::new(1100.0, 750.0),
        #[cfg(target_os = "linux")]
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: "uk.joef.Aether".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::GROUP_GAP;

    /// The chrome scale at its default `ui_font_size` — the geometry these tests were written
    /// against (row height, prompt widths). `Ui::row_h()` is the display-row unit the picker's
    /// virtual scroll and the reveal math both count in.
    fn ui() -> theme::Ui {
        theme::Ui::new(aether_protocol::settings::default_ui_font_size())
    }

    fn row_h() -> f32 {
        ui().row_h()
    }
    use aether_protocol::picker::{PickerItem, PickerUpdateParams};

    /// The info dialog is capped to leave its own top offset clear at both ends, so it can't run
    /// off a short window — the bug that made it clip before it was made scrollable.
    #[test]
    fn info_dialog_never_exceeds_the_window() {
        for view_h in [400.0, 700.0, 1080.0, 2160.0] {
            let b = prompt_box(true, view_h, ui());
            assert!(
                b.top + b.max_h + b.top <= view_h.max(b.top * 2.0 + 120.0),
                "box at {view_h}px window: top {} + max_h {} overflows",
                b.top,
                b.max_h
            );
        }
    }

    /// On a very short window the cap bottoms out rather than collapsing to nothing — a small
    /// scrolling box still beats an unusable one.
    #[test]
    fn info_dialog_height_has_a_floor() {
        assert_eq!(prompt_box(true, 100.0, ui()).max_h, 120.0);
        assert_eq!(prompt_box(true, 0.0, ui()).max_h, 120.0);
    }

    /// The info dialog is the wide, picker-aligned shape; the question prompts keep the narrow,
    /// lower one.
    #[test]
    fn prompt_shapes_differ_by_kind() {
        let info = prompt_box(true, 1000.0, ui());
        let question = prompt_box(false, 1000.0, ui());
        assert!(info.width > question.width);
        assert!(info.top < question.top, "info sits at the picker's offset");
    }

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
        let file_span = |start: u32, rel: &str| aether_protocol::picker::GroupSpan {
            start,
            header: aether_protocol::picker::GroupHeader::File {
                path_index: 0,
                relative_path: rel.into(),
            },
        };
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Grep,
            generation: 0,
            offset: 0,
            items: Some(items),
            total_matches: 23,
            total_candidates: 23,
            ticking: false,
            groups: vec![file_span(0, "a.rs"), file_span(3, "b.rs")],
            display_offset: Some(0),
            total_display_rows: Some(25),
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
        let scroll = 6.0 * row_h();
        s.selected = 4; // display row 6 — the first visible row, pinned header over it
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Minimal, ui()),
            Some(5.0 * row_h() + GROUP_GAP),
            "selection on the pinned-over first row needs a one-row scroll (plus the group gap above it)"
        );
        // One row below the top edge is genuinely visible — no scroll.
        s.selected = 5; // display row 7
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal, ui()), None);
        // Top-aligned reveals (grep file jumps) leave the same clearance.
        s.selected = 22; // display row 24 — below the 18-row viewport (rows 6..24)
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Top, ui()),
            Some(23.0 * row_h() + GROUP_GAP),
            "the row aligns with its clearance row at the top"
        );
        // The first hit of the list reveals to 0 — its real header row is above it.
        s.selected = 0; // display row 1
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal, ui()), Some(0.0));
    }

    /// The Keybindings picker pins its group header the same way grep pins its file header, so
    /// an upward reveal (Alt-k) must leave the same one-row clearance — without it the
    /// selection slides under the pinned header (the bug grep hit first).
    #[test]
    fn keybindings_reveal_clears_the_sticky_group_header() {
        let kb = |group: &str, n: u32| PickerItem::Keybinding {
            group: group.into(),
            desc: format!("binding {n}"),
            mode: "Normal".into(),
            keys: "x".into(),
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::Keybindings);
        // Rows [0]=hdr Motion, [1..=3]=items, [4]=hdr Edit, [5..=24]=items — the grep fixture's
        // shape with group labels instead of file paths.
        let mut items: Vec<_> = (1..=3).map(|n| kb("Motion", n)).collect();
        items.extend((1..=20).map(|n| kb("Edit", n)));
        let label_span = |start: u32, label: &str| aether_protocol::picker::GroupSpan {
            start,
            header: aether_protocol::picker::GroupHeader::Label {
                label: label.into(),
            },
        };
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Keybindings,
            generation: 0,
            offset: 0,
            items: Some(items),
            total_matches: 23,
            total_candidates: 23,
            ticking: false,
            groups: vec![label_span(0, "Motion"), label_span(3, "Edit")],
            display_offset: Some(0),
            total_display_rows: Some(25),
            center_on: None,
            explorer_peek_missing: false,
        }));
        // Scrolled so display row 6 (an Edit row) is first visible, pinned "Edit" header over it.
        let scroll = 6.0 * row_h();
        s.selected = 4; // display row 6 — the first visible row, pinned header over it
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Minimal, ui()),
            Some(5.0 * row_h() + GROUP_GAP),
            "selection on the pinned-over first row needs a one-row scroll (plus the group gap above it)"
        );
        // One row below the top edge is genuinely visible — no scroll.
        s.selected = 5; // display row 7
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal, ui()), None);
        // The list's first item reveals to 0 — its real header row is above it.
        s.selected = 0; // display row 1
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal, ui()), Some(0.0));
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        let scroll = 6.0 * row_h();
        s.selected = 6; // first visible row — visible as-is
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal, ui()), None);
        s.selected = 5;
        assert_eq!(
            reveal_target(&s, scroll, Reveal::Minimal, ui()),
            Some(5.0 * row_h())
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

    /// The new-window spawn must survive the running binary being rebuilt: on Linux it execs
    /// `/proc/self/exe` (the live inode), never the textual `current_exe()` path — which reads
    /// `…/ae (deleted)` after a `cargo build` and ENOENTs. AppImage launches spawn the image.
    #[test]
    fn window_spawn_exe_survives_rebuilds_and_prefers_the_appimage() {
        let current = Some(std::path::PathBuf::from("/tmp/.mount_ae123/usr/bin/ae"));
        // Inside the AppImage mount: spawn the image (its own mount + lifetime).
        assert_eq!(
            window_spawn_exe(
                current.clone(),
                Some("/home/u/Applications/aether.AppImage".into()),
                Some("/tmp/.mount_ae123".into()),
            ),
            Some("/home/u/Applications/aether.AppImage".into())
        );
        // An inherited APPIMAGE var from some other AppImage'd parent doesn't hijack the spawn.
        let plain = Some(std::path::PathBuf::from("/home/u/.cargo/bin/ae"));
        let via_proc = window_spawn_exe(
            plain.clone(),
            Some("/somewhere/other.AppImage".into()),
            Some("/tmp/.mount_other".into()),
        );
        if cfg!(target_os = "linux") {
            assert_eq!(via_proc, Some("/proc/self/exe".into()));
            // The stale-path trap: even a "(deleted)" current_exe is irrelevant on Linux.
            assert_eq!(
                window_spawn_exe(
                    Some("/home/u/proj/target/debug/ae (deleted)".into()),
                    None,
                    None
                ),
                Some("/proc/self/exe".into())
            );
        } else {
            assert_eq!(via_proc, plain);
        }
    }
}
