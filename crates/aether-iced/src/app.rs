//! Application state and message loop.
//!
//! Mirrors the TUI's `app.rs` in miniature, restructured for iced's architecture: key events
//! resolve through `keymap` to `Action`s, actions become RPC `Task`s, and responses /
//! server notifications come back as `Message`s that update state. The scroll model is the web
//! client's: a pixel offset into the full document height, with window fetches when the view
//! nears the loaded range's edge.

pub use crate::core::session::*;
pub use crate::core::effect::{Effect, Effects, ToastKind};
use crate::core::update::Event as CoreEvent;
use crate::connection::NotifRx;
use crate::connection::Handle;
use crate::editor::{self, ClickKind, EditorEvent, GUTTER_COLS, PAD};
use crate::grid;
use crate::picker::{PickerMsg, PickerState, Reveal, FETCH_LIMIT};
use crate::keymap::{Action, KeyCode, Mods, ScrollDir, ScrollUnit};
use crate::theme;
use aether_protocol::buffer::{
    BufferOpen, BufferOpenParams,
    BufferOpenResult,
};
use aether_protocol::cursor::{
    CursorSet,
    CursorSetParams, Granularity,
};
use aether_protocol::envelope::{NotificationMethod, RpcMethod};
use aether_protocol::git::{
    GitBlameLine, GitBlameLineParams, GitSetDiffView, GitSetDiffViewParams,
};
use aether_protocol::lsp::LspStatus;
use aether_protocol::picker::{
    PickerItem, PickerKind, PickerQuery,
    PickerQueryParams, PickerUpdate,
    PickerUpdateParams, PickerView, PickerViewParams,
};
use aether_protocol::project::{ProjectActivate, ProjectActivateParams, ProjectInfo};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{
    ScrollPosition, ViewportResize,
    ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportScrollToRow,
    ViewportScrollToRowParams, ViewportSetWrap, ViewportSetWrapParams, ViewportSubscribe,
    ViewportSubscribeParams, ViewportSubscribeResult, ViewportWindowResult,
    Window, WrapMode,
};
use iced::widget::{column, container, row, text};
use iced::{keyboard, Element, Event, Length, Size, Subscription, Task};

const TAB_WIDTH: u32 = 4;

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

/// Everything a successful reconnect hands back to rebuild the session.
pub struct Reestablished {
    pub handle: Handle,
    pub notifications: NotifRx,
    pub project: ProjectInfo,
    pub open: BufferOpenResult,
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
    /// No daemon reachable (discovery/dial failed) — retry, silently.
    NotUp,
    /// A server answered but re-establishing failed — terminal.
    Fatal(String),
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

/// The hover popover's body: plain severity-coloured blocks (diagnostics, commit details) or
/// rendered markdown (LSP hover). The *content* comes from the core ([`HoverText`]); the
/// parsed widget items are this shell's cache of it.
enum HoverContent {
    Blocks(Vec<HoverBlock>),
    Markdown {
        items: Vec<iced::widget::markdown::Item>,
        /// Source line count, for the place-above-or-below estimate.
        est_lines: usize,
    },
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
    /// The project chooser (no-args start). While set, `session` is an inert placeholder and
    /// all messages route through `update_boot`; picking a project builds the real session
    /// over the boot connection and clears this.
    boot: Option<Boot>,
    /// The window's one editing context (one connection — the server's client).
    session: Session,
    /// The session's transport — shell-owned (native sockets don't exist on every shell;
    /// the core receives the handle per call rather than storing it).
    handle: Handle,
    notifications: NotifRx,
    client_version: String,
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
    /// The picker results list's scroll offset in px (boot chooser or session picker —
    /// never both). The core tracks rows, not pixels; resets arrive as
    /// `Effect::PickerScrollReset`.
    picker_scroll_y: f32,
    /// The hover popover (hover info / diagnostics-at-cursor / commit details), anchored at
    /// the cursor; holds *parsed* iced markdown. Dismissed by any key, click, or scroll.
    hover: Option<HoverContent>,

    // Transient messages are toasts; the status bar shows persistent state only (web client
    // convention).
    toasts: Vec<Toast>,
    next_toast: u64,
}

impl App {
    pub fn new(b: Bootstrap) -> (Self, Task<Message>) {
        let shell = |boot: Option<Boot>, session: Session, handle: Handle,
                     notifications: NotifRx, client_version: String,
                     server_started_at: u64| App {
            boot,
            session,
            handle,
            notifications,
            client_version,
            server_started_at,
            cell: None,
            view_size: Size::ZERO,
            scroll_px: 0.0,
            scroll_x_px: 0.0,
            scroll_anim: None,
            scroll_anchor: None,
            picker_scroll_y: 0.0,
            hover: None,
            toasts: Vec::new(),
            next_toast: 0,
        };
        match b {
            Bootstrap::Session(b) => {
                let pump = pump(b.notifications.clone());
                let session = Session::new(b.project, b.project_paths, b.buffer);
                (
                    shell(
                        None,
                        session,
                        b.handle,
                        b.notifications,
                        b.client_version,
                        b.server_started_at,
                    ),
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
                    |result| Message::Core(CoreEvent::PickerViewed {
                        initial: true,
                        result,
                    }),
                );
                let boot = Boot {
                    handle: b.handle.clone(),
                    notifications: b.notifications.clone(),
                    picker: PickerState::new(PickerKind::Projects),
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
                        b.server_started_at,
                    ),
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
            }) => crate::input::keycode(&key).map(|code| Message::Key {
                code,
                mods: crate::input::mods(modifiers),
                text: text.map(|t| t.to_string()),
            }),
            _ => None,
        });
        if self.boot.is_none() && self.scroll_anim.is_some() {
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
                self.picker_scroll_y = 0.0;
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
                    |result| Message::Core(CoreEvent::PickerViewed {
                        initial: true,
                        result,
                    }),
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
                self.handle = boot.handle;
                self.notifications = boot.notifications;
                self.session = Session::new(project.name, project.paths, buffer);
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
        let (query, generation) = (p.query.clone(), p.generation);
        self.picker_scroll_y = 0.0;
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
            |result| Message::Core(CoreEvent::PickerViewed {
                initial: false,
                result,
            }),
        )
    }

    fn boot_move(&mut self, delta: i64) -> Task<Message> {
        let Some(boot) = &mut self.boot else {
            return Task::none();
        };
        match boot.picker.move_selection(delta) {
            Some(offset) => self.boot_refetch(offset),
            None => reveal_picker_selection(&boot.picker, &mut self.picker_scroll_y, Reveal::Minimal),
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
                        self.scroll_px = (row as f32 + scroll.sub_row) * cell.height;
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
                        move |result| Message::Core(CoreEvent::DiffViewSet { enabled, result }),
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

            Message::Core(ev) => {
                let t = self.transport();
                let fx = self.session.on_event(ev, &t);
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
                        let t = self.transport();
                        let fx = self.session.picker_refetch(&t, offset);
                        self.run_core(fx)
                    }
                    None => Task::none(),
                }
            }

            Message::ToastExpired(id) => {
                self.toasts.retain(|t| t.id != id);
                Task::none()
            }
            Message::Noop => Task::none(),

            Message::AnimTick(now) => {
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
                let t = self.transport();
                let fx = self.session.on_event(CoreEvent::ServerPush(n), &t);
                Task::batch([self.run_core(fx), pump(self.notifications.clone())])
            }
            Message::Notified(None) => {
                let t = self.transport();
                let fx = self.session.on_event(CoreEvent::ConnectionLost, &t);
                self.run_core(fx)
            }

            // The transport swap is the shell's half of a reconnect (the new socket and
            // daemon identity live here); the session adoption is the core's.
            Message::Reconnected(Ok(r)) => {
                let restarted = r.server_started_at != self.server_started_at;
                tracing::info!(restarted, url = %r.server_url, "transport re-established");
                self.server_started_at = r.server_started_at;
                self.handle = r.handle;
                self.notifications = r.notifications.clone();
                let t = self.transport();
                let fx = self.session.on_event(
                    CoreEvent::Reestablished {
                        project: r.project,
                        open: r.open,
                        restarted,
                    },
                    &t,
                );
                Task::batch([pump(r.notifications), self.run_core(fx)])
            }
            Message::Reconnected(Err(ReconnectError::NotUp)) => {
                let t = self.transport();
                let fx = self.session.on_event(CoreEvent::ReconnectRetry, &t);
                self.run_core(fx)
            }
            Message::Reconnected(Err(ReconnectError::Fatal(e))) => {
                let t = self.transport();
                let fx = self.session.on_event(CoreEvent::ReconnectFatal(e), &t);
                self.run_core(fx)
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

    /// Execute a batch of core effects: futures spawn onto iced's executor with their events
    /// routed back through the bridge; presentation effects run against shell state.
    fn run_core(&mut self, fx: Effects<CoreEvent>) -> Task<Message> {
        let mut tasks = Vec::new();
        for e in fx.0 {
            match e {
                Effect::Spawn(fut) => tasks.push(Task::perform(fut, Message::Core)),
                Effect::Toast(message, kind) => tasks.push(self.toast(message, kind)),
                Effect::WriteClipboard(text) => tasks.push(iced::clipboard::write(text)),
                Effect::RevealCursor => tasks.push(self.ensure_cursor_visible()),
                Effect::Resubscribe => {
                    self.scroll_px = 0.0;
                    self.scroll_x_px = 0.0;
                    self.scroll_anim = None;
                    self.hover = None;
                    // Reconnects zero the grid (new viewport identity); re-derive it from
                    // the current metrics so subscribe_task has something to send.
                    if self.session.sent_grid.is_none() {
                        self.session.sent_grid = self.current_grid();
                    }
                    tasks.push(self.subscribe_task());
                }
                Effect::SaveScrollAnchor => self.scroll_anchor = Some(self.scroll_px),
                Effect::ShowHover(content) => {
                    self.hover = Some(match content {
                        crate::core::session::HoverText::Blocks(blocks) => {
                            HoverContent::Blocks(blocks)
                        }
                        crate::core::session::HoverText::Markdown(text) => {
                            let est_lines = text.lines().count().max(1);
                            HoverContent::Markdown {
                                items: iced::widget::markdown::parse(&text).collect(),
                                est_lines,
                            }
                        }
                    });
                }
                Effect::DismissHover => self.hover = None,
                Effect::WindowAdopted => {
                    self.clamp_scroll();
                    self.reveal_cursor();
                }
                Effect::RevealPickerSelection(reveal) => {
                    tasks.push(self.picker_reveal_selected_with(reveal));
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

    /// The session's transport as the core sees it (a fresh `Arc` per dispatch is fine —
    /// `Handle` is a channel sender).
    fn transport(&self) -> crate::core::transport::SharedTransport {
        std::sync::Arc::new(self.handle.clone())
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
                self.hover = None;
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
                self.hover = None;
                // A click outside the dialog/picker cancels it (the web's backdrop-click
                // behaviour); the click doesn't also move the cursor.
                if self.session.prompt.is_some() {
                    self.session.decline_prompt();
                    return Task::none();
                }
                if self.session.picker.is_some() {
                    let t = self.transport();
                    let fx = self.session.close_picker(&t);
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
                let anchor = if shift { self.session.buffer.cursor.anchor } else { pos };
                self.session.drag = Some((anchor, granularity));
                self.rpc::<CursorSet>(
                    CursorSetParams {
                        buffer_id: self.session.buffer.buffer_id,
                        position: pos,
                        anchor,
                        granularity,
                    },
                    |r| Message::Core(CoreEvent::CursorMsg(r)),
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
                    |r| Message::Core(CoreEvent::CursorMsg(r)),
                )
            }
            EditorEvent::Released => {
                self.session.drag = None;
                Task::none()
            }
        }
    }

    // ---- keyboard --------------------------------------------------------------------------

    /// Key events: the shell's edge — dismiss the hover popover (its parse cache lives
    /// here), then hand the key to the core with the viewport height it may need.
    fn on_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Task<Message> {
        // Any keystroke dismisses an open hover popover; Esc is consumed by the dismissal
        // (matching the web client), everything else still acts.
        if self.hover.is_some() {
            self.hover = None;
            if code == KeyCode::Esc {
                return Task::none();
            }
        }
        let visible_rows = self.visible_rows();
        let t = self.transport();
        let fx = self.session.on_key(&t, code, mods, text, visible_rows);
        self.run_core(fx)
    }

    /// Actions whose execution is irreducibly shell-side (`Effect::ShellAction`).
    fn run_shell_action(&mut self, action: Action) -> Task<Message> {
        use Action as A;
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
            A::CenterCursor => {
                self.center_cursor();
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
                self.scroll_x_px = 0.0;
                let wrap = self.session.wrap;
                self.rpc::<ViewportSetWrap>(
                    ViewportSetWrapParams { viewport_id, wrap },
                    Message::WindowUpdate,
                )
            }
            _ => Task::none(),
        }
    }

    // ---- actions ----------------------------------------------------------------------------

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
        iced::clipboard::read()
            .map(move |t| Message::Core(CoreEvent::ClipboardRead(kind, t)))
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
                scroll_px: self.scroll_px,
                scroll_x_px: self.scroll_x_px,
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
        if self.hover.is_some() {
            layers.push(self.hover_overlay());
        }
        if let Some(p) = &self.session.picker {
            layers.push(
                Element::from(crate::picker::overlay(p, &self.session.project_paths, self.picker_scroll_y)).map(
                    |m| match m {
                        PickerMsg::Click(abs) => Message::Core(CoreEvent::PickerClicked(abs)),
                        PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
                        PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
                        PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
                        PickerMsg::ChipClicked(i) => Message::Core(CoreEvent::PickerChipClicked(i)),
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
        let picker = Element::from(crate::picker::overlay(&boot.picker, &[], self.picker_scroll_y)).map(|m| match m {
            PickerMsg::Click(abs) => Message::Core(CoreEvent::PickerClicked(abs)),
            PickerMsg::Scrolled(y) => Message::PickerScrolled(y),
            PickerMsg::Hovered(abs) => Message::PickerHovered(Some(abs)),
            PickerMsg::Unhovered(abs) => Message::PickerUnhovered(abs),
            PickerMsg::ChipClicked(i) => Message::Core(CoreEvent::PickerChipClicked(i)),
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
            PromptMsg::Accept => Message::Core(CoreEvent::PromptAccept),
            PromptMsg::Cancel => Message::Core(CoreEvent::PromptCancel),
        })
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
                let row_top = PAD + row as f32 * cell.height - self.scroll_px;
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
    let clearance = if p.kind == PickerKind::Grep {
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

/// `"47"` or `"10000+"` when the server hit its match cap.
fn format_total(s: &SearchSummary) -> String {
    if s.truncated {
        format!("{}+", s.total)
    } else {
        s.total.to_string()
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
            items,
            total_matches: 23,
            total_candidates: 23,
            ticking: false,
            grep_display_offset: Some(0),
            grep_total_display_rows: Some(25),
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
        let mut s = PickerState::new(PickerKind::Projects);
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Projects,
            generation: 0,
            offset: 0,
            items: (0..30)
                .map(|i| PickerItem::Project {
                    name: format!("p{i}"),
                    match_indices: vec![],
                })
                .collect(),
            total_matches: 30,
            total_candidates: 30,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }));
        let scroll = 6.0 * ROW_H;
        s.selected = 6; // first visible row — visible as-is
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal), None);
        s.selected = 5;
        assert_eq!(reveal_target(&s, scroll, Reveal::Minimal), Some(5.0 * ROW_H));
    }
}
