//! The core-driven shell: the TUI's event loop over `aether_client::Session`
//! (docs/tui-port.md). Keys translate to the core's `KeyCode`/`Mods` and feed
//! `Session::on_key`; server pushes feed `on_event(ServerPush)`; RPC outcomes feed
//! `on_rpc_result`. The shell executes effects in terminal terms (row scrolling, the
//! status row, the clipboard) and syncs a render view (`AppState`) from `Session` before
//! every draw, so `ui::draw` renders unchanged.
//!
//! Geometry mirrors the iced shell with rows for pixels: the scroll position is a visual
//! row (`top_visual_row`), reveals overscroll by one row, and window fetches go through
//! `viewport/scroll_to_row` when the view nears the loaded range's edge.

use crate::app::{
    AppState, BlameState, EditorMode, EditorState, HoverBlock, HoverBody, HoverPopup,
    PendingLeader, SearchState as TuiSearchState, StatusKind, StatusMessage,
};
use crate::connection::Handle;
use crate::connection::RpcError;
use crate::{clipboard, labels, ui};
use aether_client::effect::{Effect, Effects, RevealStyle, ShellAction, ToastKind};
use aether_client::keymap::{
    hover_action, HoverAction, KeyCode, Mods, ScrollDir, ScrollUnit, ViewportPlace,
    CURSOR_REST_FRACTION,
};
use aether_client::session::{
    boot_backoff, buffer_info, reconnect_backoff, ConfirmKind, ConnState, HoverText, Mode, Pending,
    Prompt, Session,
};
use aether_client::update::Event as CoreEvent;
use aether_protocol::envelope::Notification;
use aether_protocol::git::{GitBlameLine, GitBlameLineParams};
use aether_protocol::viewport::{
    ScrollPosition, ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams,
    ViewportScrollToRow, ViewportScrollToRowParams, ViewportSubscribe, ViewportSubscribeParams,
    ViewportSubscribeResult, ViewportWindowResult, Window, WrapMode,
};
use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use std::pin::Pin;
use tokio::sync::mpsc;

/// Tab rendering width — matches the other clients.
const TAB_WIDTH: u32 = 4;

/// How long after a left press a follow-up press on the same cell still counts as part of the
/// same multi-click chain (double → word, triple → line). Terminals report plain presses with
/// no click count, so the shell synthesises the streak from press timing.
const MULTI_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// A completed async operation, drained from the shell's `FuturesUnordered`.
enum Done {
    /// An `Effect::Request` outcome — token routes it to the session's parked mapping.
    Core(u64, Result<serde_json::Value, RpcError>),
    /// `viewport/subscribe` result (shell-initiated; geometry). The epoch identifies which
    /// subscribe this answers, so a response superseded by a newer subscribe (e.g. a burst of
    /// `<`/`>` grep jumps) can be dropped instead of installing a since-deleted viewport_id.
    Subscribed(u64, Result<ViewportSubscribeResult, RpcError>),
    /// A window-returning viewport call (`scroll_to_row` / `scroll` / `resize`).
    Window(Result<ViewportWindowResult, RpcError>),
    /// Cursor-line blame, formatted shell-side ("author · 3w ago" needs a clock).
    Blame {
        buffer_id: u64,
        line: u32,
        text: Option<String>,
    },
    /// A reconnect dial attempt (see `Effect::Reconnect`).
    Reconnected(Box<Result<Reestablished, ReconnectError>>),
    /// The initial boot dial: connect + bootstrap from the `Connecting` launch state. `NotUp`
    /// retries (the daemon may still be coming up); `Fatal` (e.g. a bad CLI workspace) ends the run.
    Booted(Box<Result<Booted, ReconnectError>>),
    /// A floating toast's time-to-live elapsed; removes the toast with this id from the stack.
    ToastExpired(u64),
}

/// How long a floating toast (bottom-right) stays before it auto-dismisses — matching the web/native
/// clients' transient toasts.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);

struct Reestablished {
    handle: Handle,
    notifications: mpsc::UnboundedReceiver<Notification>,
    /// The restored workspace + landing buffer, or `None` when the workspace is gone — renamed or
    /// removed by another client while we were disconnected. The connection itself is fine, so the
    /// shell recovers into the workspace chooser rather than failing.
    restore: Option<(
        aether_protocol::workspace::WorkspaceInfo,
        aether_protocol::buffer::BufferOpenResult,
    )>,
    restarted: bool,
}

enum ReconnectError {
    /// Server not up yet — retry after backoff.
    NotUp,
    /// Connected but restoring state failed — give up with a message.
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

/// A successful initial boot: the live connection plus the bootstrapped session/state and any
/// startup effects (e.g. the no-args Workspaces chooser's `picker/view`), ready to install in place
/// of the connecting placeholder.
struct Booted {
    handle: Handle,
    notifications: mpsc::UnboundedReceiver<Notification>,
    session: Session,
    state: AppState,
    startup: Effects,
}

/// The CLI args the boot dial needs, retained so a `NotUp` failure can re-dial. Held while the
/// shell is in the `Connecting` state.
#[derive(Clone)]
struct BootSpec {
    workspace: Option<String>,
    file: Option<String>,
    /// A 0-based `(line, col)` to jump to once `file` opens (`ae src/main.rs:42`). `None` for a bare
    /// open. Initial-boot only — reconnects restore the live cursor, not this.
    jump: Option<(u32, u32)>,
    /// Tether the client to the buffer `file` opens (docs/tether.md): the quick-edit invocation
    /// — a file positional without an explicit `--workspace` — where closing that buffer exits.
    tether: bool,
    version: String,
}

type DoneFuture = Pin<Box<dyn std::future::Future<Output = Done> + Send>>;

pub struct Shell {
    pub handle: Handle,
    notifications: mpsc::UnboundedReceiver<Notification>,
    pub session: Session,
    pub state: AppState,
    pending: FuturesUnordered<DoneFuture>,
    /// The view's scroll position as a visual row into the buffer's full height —
    /// the iced shell's `scroll_px` with rows for pixels.
    top_visual_row: u32,
    /// The search prompt's Esc-restore anchor (`SaveScrollAnchor` effect).
    scroll_anchor: Option<u32>,
    /// Horizontal scroll: the first visible display column when soft-wrap is off (always 0 under
    /// soft wrap, which never overflows right). The renderer drops this many columns from each
    /// row; cursor moves keep it in `[scroll_col, scroll_col + viewport_cols)`.
    scroll_col: u32,
    // Viewport/fetch geometry — shell-owned (the core reasons about `window`/`viewport_id`,
    // never these). The grid last sent to the server, the scroll a subscribe asked for, and the
    // fetch-coordination flags that gate `maybe_fetch`.
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
    /// Picker results scroll continuity — the first-visible *view row* (`top`, an index into
    /// [`crate::ui::picker_window_rows`]: header / gap / item rows, so grep / keybindings scroll one
    /// screen row at a time), the selection's on-screen pane row, and the `core.offset` they were
    /// computed against. Threaded through `crate::ui::picker_scroll_step` each sync so the scroll
    /// survives a recentering refetch (and its empty-items frames). Reset by
    /// `Effect::PickerScrollReset`.
    picker_scroll: crate::ui::PickerScroll,
    /// Monotonic id for the latest `viewport/subscribe`. Stale `Done::Subscribed` responses
    /// (whose epoch != this) are dropped so a superseded subscribe can't reinstate a viewport
    /// the server has already replaced.
    subscribe_epoch: u64,
    /// The last left-press `(when, row, col)`, for synthesising double/triple clicks: the
    /// terminal reports plain presses with no click count, so a same-cell press within
    /// [`MULTI_CLICK_WINDOW`] extends the streak (1→Char, 2→Word, 3+→Line).
    last_click: Option<(std::time::Instant, u16, u16)>,
    click_streak: u32,
    should_quit: bool,
    /// Shell-owned text editor for the focused overlay input (save-as, etc.). The terminal has no
    /// native input widget, so the shell drives the caret + edits here and syncs the whole value
    /// into the core; command keys still route through `session.on_key`. `None` when no overlay
    /// text field is focused. See `crate::overlay_input`.
    overlay_edit: Option<crate::overlay_input::OverlayEdit>,
    /// Terminal size (cols, rows); the text viewport is `rows - 1` (status row).
    term: (u16, u16),
    /// Monotonic id source for floating toasts — each toast's expiry timer carries its id so the
    /// right one is removed from the stack when it elapses.
    next_toast_id: u64,
    /// Set while in the boot `Connecting` state: the CLI args to (re)dial with. Cleared once a
    /// connection lands and the real session is installed.
    boot: Option<BootSpec>,
    /// Boot dial attempt count, for the retry backoff while `boot` is set.
    boot_attempt: u32,
    /// A fatal boot error (e.g. the named workspace doesn't exist) — surfaced as the run's `Err`
    /// after the loop unwinds and the terminal is restored.
    fatal: Option<String>,
    /// The (profile-resolved) WebSocket address every boot dial and reconnect dials.
    server_url: String,
    /// Reading-view scroll: first visible row of the laid-out document — the reading sibling of
    /// `top_visual_row` (docs/markdown-view.md: read-mode scroll is shell-owned).
    read_scroll: u16,
    /// The focus last revealed, so the view scrolls only when the focus *changes* (manual
    /// scrolling doesn't fight the reveal).
    read_last_focus: Option<usize>,
    /// Reading-view layout cache, keyed by `(buffer, revision, hl_gen, content_cols)` —
    /// `hl_gen` moves when fence highlights land, which revision alone doesn't capture.
    #[allow(clippy::type_complexity)]
    read_cache: Option<(
        u64,
        u64,
        u64,
        u16,
        std::sync::Arc<Vec<aether_client::read_layout::ReadRow>>,
    )>,
    /// Per-code-block horizontal scroll (docs/markdown-view.md §2.8), keyed by the block's
    /// element index: code rows are laid out unchunked and the painter clips them to this
    /// offset. Cleared when the parse changes (element indices shift with the content).
    read_hscroll: std::collections::HashMap<usize, u16>,
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    workspace: Option<String>,
    file: Option<String>,
    jump: Option<(u32, u32)>,
    tether: bool,
    version: String,
    server_url: String,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<std::io::Result<Event>>();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(ev) = events.next().await {
            if event_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let term = crossterm::terminal::size()?;
    // Launch connectionless: a placeholder session flagged `Connecting` and dummy transport that's
    // never exercised (the core drops RPCs while not `Connected`). The editor chrome renders from
    // the start — empty buffer area, status row showing "Connecting…" — and client-side keys work,
    // exactly the feel of a mid-session reconnect. The boot dial runs in the background and
    // installs the real session once the socket lands.
    let mut session = Session::placeholder();
    session.conn = ConnState::Connecting;
    let mut shell = Shell {
        handle: crate::connection::dummy_handle(),
        notifications: crate::connection::dummy_notifications(),
        session,
        state: connecting_state(term.0, term.1),
        pending: FuturesUnordered::new(),
        top_visual_row: 0,
        scroll_anchor: None,
        scroll_col: 0,
        sent_grid: None,
        subscribe_scroll: ScrollPosition {
            logical_line: 0,
            sub_row: 0.0,
        },
        fetch_in_flight: false,
        refetch_queued: false,
        reveal_after_fetch: None,
        place_after_fetch: None,
        picker_scroll: crate::ui::PickerScroll::default(),
        subscribe_epoch: 0,
        last_click: None,
        click_streak: 0,
        should_quit: false,
        overlay_edit: None,
        term,
        next_toast_id: 0,
        boot: Some(BootSpec {
            workspace,
            file,
            jump,
            tether,
            version,
        }),
        boot_attempt: 0,
        fatal: None,
        server_url,
        read_scroll: 0,
        read_last_focus: None,
        read_cache: None,
        read_hscroll: std::collections::HashMap::new(),
    };

    // Kick the boot dial; no subscribe yet (no buffer until it lands).
    shell.spawn_boot_dial();
    shell.sync();
    crate::app::apply_cursor_style(&shell.state);
    terminal.draw(|f| ui::draw(f, &shell.state))?;
    crate::app::refresh_terminal_title(&mut shell.state);

    // The hint engine's clock (docs/hints.md): a slow tick whose arm is gated below on
    // "connected + hints on". The engine's own idle gate covers unattended terminals (crossterm
    // focus events aren't universal, so the TUI ticks whenever the arm is enabled).
    let mut hint_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    hint_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !shell.should_quit {
        tokio::select! {
            ev = event_rx.recv() => {
                let Some(ev) = ev else { break };
                shell.on_terminal_event(ev?).await;
                while !shell.should_quit {
                    match event_rx.try_recv() {
                        Ok(ev) => shell.on_terminal_event(ev?).await,
                        Err(_) => break,
                    }
                }
            }
            _ = hint_tick.tick(), if shell.session.conn == ConnState::Connected
                && shell.session.hints_enabled =>
            {
                let fx = shell.session.on_hint_tick(now_unix_ms());
                shell.run_effects(fx);
            }
            // Only poll the notifications channel while connected. Once the socket dies the channel
            // is closed, so `recv()` returns `None` *immediately* — without this guard the `select!`
            // would spin on that arm (re-dispatching `ConnectionLost` + redrawing) and peg a core
            // during the whole reconnect backoff. The first `None` (handled while still Connected)
            // flips us to Reconnecting, disabling the arm until `Reconnected` installs a fresh one.
            n = shell.notifications.recv(), if shell.session.conn == ConnState::Connected => {
                match n {
                    Some(n) => shell.dispatch(CoreEvent::ServerPush(n)),
                    None => shell.dispatch(CoreEvent::ConnectionLost),
                }
            }
            Some(done) = shell.pending.next() => shell.on_done(done),
        }
        // Coalesce a burst of already-arrived server pushes into a single redraw — a streaming grep
        // emits a `picker/update` per batch, and the broad intermediate queries (`as`…) flood them.
        // Without this we'd sync+draw once per push and fall behind the stream (the native client
        // coalesces the same way by rendering once per frame).
        while let Ok(n) = shell.notifications.try_recv() {
            shell.dispatch(CoreEvent::ServerPush(n));
        }
        // No buffer/window exists until a connection lands; these are no-ops while connecting but
        // gated explicitly so a dummy-handle call can never slip out.
        if shell.session.conn == ConnState::Connected {
            shell.maybe_blame();
            shell.maybe_fetch();
        }
        shell.sync();
        crate::app::apply_cursor_style(&shell.state);
        terminal.draw(|f| ui::draw(f, &shell.state))?;
        crate::app::refresh_terminal_title(&mut shell.state);
    }
    // A fatal boot failure (bad CLI workspace, etc.) surfaces as the run's error once the terminal
    // is restored by the caller.
    match shell.fatal.take() {
        Some(e) => Err(anyhow::anyhow!(e)),
        None => Ok(()),
    }
}

/// Wall clock in Unix ms — the hint engine's injected clock (the core never reads time).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Shell {
    /// The text viewport's grid: full width, terminal rows minus the status row.
    fn grid(&self) -> (u32, u32) {
        (self.term.0 as u32, (self.term.1 as u32).saturating_sub(1))
    }

    fn visible_rows(&self) -> u32 {
        self.grid().1
    }

    fn status(&mut self, msg: StatusMessage) {
        self.push_toast(msg, None);
    }

    /// Push a transient toast onto the bottom-right stack and arm its expiry timer. Empty messages
    /// are dropped. A `group` replaces any existing toast with the same key (so an evolving status
    /// updates one toast in place); otherwise the stack is capped so a burst can't grow it without
    /// bound (oldest fall off).
    fn push_toast(&mut self, msg: StatusMessage, group: Option<String>) {
        if msg.is_empty() {
            return;
        }
        const MAX_TOASTS: usize = 5;
        if let Some(g) = &group {
            self.state
                .toasts
                .retain(|t| t.group.as_deref() != Some(g.as_str()));
        }
        let id = self.next_toast_id;
        self.next_toast_id = self.next_toast_id.wrapping_add(1);
        self.state.toasts.push(crate::app::Toast {
            id,
            text: msg.text,
            kind: msg.kind,
            group,
        });
        if self.state.toasts.len() > MAX_TOASTS {
            self.state.toasts.remove(0);
        }
        self.pending.push(Box::pin(async move {
            tokio::time::sleep(TOAST_TTL).await;
            Done::ToastExpired(id)
        }));
    }

    // ---- core dispatch -----------------------------------------------------------------

    fn dispatch(&mut self, event: CoreEvent) {
        let fx = self.session.on_event(event);
        self.run_effects(fx);
    }

    fn run_effects(&mut self, fx: Effects) {
        for effect in fx.0 {
            match effect {
                Effect::Request {
                    token,
                    method,
                    params,
                } => {
                    // Enqueue NOW (`call` sends synchronously) so requests hit the wire in
                    // effect-emission order — the core's sequencing contract.
                    let fut = self.handle.call(method, params);
                    self.pending
                        .push(Box::pin(async move { Done::Core(token, fut.await) }));
                }
                Effect::Toast {
                    message,
                    kind,
                    group,
                } => self.push_toast(
                    StatusMessage {
                        text: message,
                        kind: match kind {
                            ToastKind::Info => StatusKind::Info,
                            ToastKind::Success => StatusKind::Success,
                            ToastKind::Warning => StatusKind::Warning,
                            ToastKind::Error => StatusKind::Error,
                        },
                    },
                    group,
                ),
                Effect::WriteClipboard(text) => {
                    // The core already emits a "copied N bytes" success toast alongside
                    // this effect (see update.rs CopyDone handler), so only report failures
                    // here to avoid a duplicate success message.
                    if let Err(e) = clipboard::copy(&mut self.state.clipboard, text) {
                        self.status(StatusMessage::error(format!("Copy failed: {e}")));
                    }
                }
                Effect::ReadClipboard(kind) => {
                    let text = clipboard::paste(&mut self.state.clipboard).ok();
                    self.dispatch(CoreEvent::ClipboardRead(kind, text));
                }
                Effect::RevealCursor(style) => self.ensure_cursor_visible(style),
                Effect::Resubscribe => {
                    // No scroll reset here: `Subscribed` positions `top_visual_row` from
                    // the subscribe's scroll once the window arrives. Until then the view
                    // sync keeps showing the previous window — a blank-then-jump frame
                    // between buffers reads as a flash (most visibly on same-file grep
                    // `<`/`>` jumps).
                    self.state.hover = None;
                    self.scroll_col = 0; // a fresh buffer starts flush-left
                    self.sent_grid = Some(self.grid());
                    self.subscribe();
                }
                Effect::SaveScrollAnchor => self.scroll_anchor = Some(self.top_visual_row),
                Effect::RestoreScrollAnchor => {
                    if let Some(row) = self.scroll_anchor.take() {
                        self.scroll_to_row(row);
                    }
                }
                Effect::SaveContentAnchor => self
                    .session
                    .capture_scroll_anchor(self.top_visual_row, self.visible_rows()),
                Effect::ShowHover(text) => {
                    let body = match text {
                        HoverText::Blocks(blocks) => HoverBody::Blocks(
                            blocks
                                .into_iter()
                                .map(|b| HoverBlock {
                                    text: b.text,
                                    severity: b.severity,
                                })
                                .collect(),
                        ),
                        // The shared Markdown AST — the UI renders it to styled, wrapped lines.
                        HoverText::Markdown(blocks) => HoverBody::Markdown(blocks),
                    };
                    self.state.hover = Some(HoverPopup::new(body));
                }
                Effect::DismissHover => self.state.hover = None,
                Effect::WindowAdopted => {
                    // Diff toggle re-layout: if a content anchor is pending, restore the view to
                    // it (keep the same content on screen); otherwise clamp + reveal as before.
                    if let Some(row) = self.session.resolve_scroll_anchor() {
                        self.top_visual_row = row;
                        self.clamp_scroll();
                    } else {
                        self.clamp_scroll();
                        self.reveal_cursor();
                    }
                }
                Effect::RevealPickerSelection(_) | Effect::PickerScrollReset => {
                    // Selection reveals are handled by the sync (`visible_start` follows
                    // `selected`); a reset just snaps the window to the top.
                    if matches!(effect, Effect::PickerScrollReset) {
                        self.picker_scroll = crate::ui::PickerScroll::default();
                    }
                }
                Effect::Reconnect { attempt } => self.spawn_reconnect(attempt),
                Effect::HintTickNow => {
                    let fx = self.session.on_hint_tick(now_unix_ms());
                    self.run_effects(fx);
                }
                Effect::Exit => self.should_quit = true,
                Effect::ToChooser => self.enter_chooser(),
                Effect::ShellAction(action) => self.run_shell_action(action),
            }
        }
    }

    fn on_done(&mut self, done: Done) {
        match done {
            Done::Core(token, result) => {
                let fx = self.session.on_rpc_result(token, result);
                self.run_effects(fx);
            }
            Done::Subscribed(epoch, _) if epoch != self.subscribe_epoch => {
                // A newer subscribe has been issued; this one's viewport was already replaced
                // (and likely deleted) server-side. Adopting it would reinstate a stale
                // viewport_id and fire fetches that fail with "unknown viewport_id".
            }
            Done::Subscribed(_, Ok(res)) => {
                let scroll = self.subscribe_scroll;
                self.session.adopt_subscribe(res);
                // A wrap toggle left a content anchor pending: restore the view to it (keeping the
                // same content on screen across the reflow). Otherwise position the top at the
                // subscribe's scroll line and reveal the cursor as usual.
                if let Some(row) = self.session.resolve_scroll_anchor() {
                    self.top_visual_row = row;
                    self.clamp_scroll();
                } else {
                    if let Some(w) = self.session.window.as_ref() {
                        if let Some(rel) = rows_before_line(w, scroll.logical_line) {
                            self.top_visual_row = w.first_visual_row + rel;
                        }
                    }
                    self.clamp_scroll();
                    self.reveal_cursor();
                }
                // Diff view rides the subscribe params, so there's nothing to re-apply here.
            }
            Done::Subscribed(_, Err(e)) => {
                self.status(StatusMessage::error(format!("Subscribe failed: {e}")))
            }
            Done::Window(Ok(res)) => {
                self.fetch_in_flight = false;
                self.session.adopt_window(res);
                self.clamp_scroll();
                if let Some(style) = self.reveal_after_fetch.take() {
                    self.reveal_cursor_styled(style);
                }
                if let Some(place) = self.place_after_fetch.take() {
                    self.place_cursor_in_window(place);
                }
                if self.refetch_queued {
                    self.refetch_queued = false;
                    self.maybe_fetch();
                }
            }
            Done::Window(Err(e)) => {
                self.fetch_in_flight = false;
                self.refetch_queued = false;
                // A fetch can race a resubscribe: the viewport it targeted was deleted before
                // the call landed. The pending newer subscribe reveals the cursor afresh, so
                // this is expected churn, not a failure worth surfacing.
                if e.code != aether_protocol::error::ErrorCode::VIEWPORT_NOT_FOUND.code() {
                    self.status(StatusMessage::error(format!("Viewport update failed: {e}")));
                }
            }
            Done::Blame {
                buffer_id,
                line,
                text,
            } => self.dispatch(CoreEvent::BlameLine {
                buffer_id,
                line,
                text,
            }),
            Done::ToastExpired(id) => self.state.toasts.retain(|t| t.id != id),
            Done::Reconnected(result) => match *result {
                Ok(r) => {
                    self.handle = r.handle;
                    self.notifications = r.notifications;
                    match r.restore {
                        Some((workspace, open)) => {
                            let restarted = r.restarted;
                            self.dispatch(CoreEvent::Reestablished {
                                workspace,
                                open,
                                restarted,
                            });
                        }
                        // The workspace is gone — drop to the chooser over the fresh connection.
                        None => self.reconnect_to_chooser(),
                    }
                }
                Err(ReconnectError::NotUp) => self.dispatch(CoreEvent::ReconnectRetry),
                Err(ReconnectError::Fatal(e)) => self.dispatch(CoreEvent::ReconnectFatal(e)),
            },
            Done::Booted(result) => match *result {
                Ok(b) => {
                    // The dial landed: swap the dummy transport for the real one and install the
                    // bootstrapped session over the connecting placeholder, then subscribe + fire
                    // startup effects — the same setup the old synchronous boot did inline.
                    let jumped = self.boot.as_ref().is_some_and(|s| s.jump.is_some());
                    self.boot = None;
                    self.boot_attempt = 0;
                    self.handle = b.handle;
                    self.notifications = b.notifications;
                    self.session = b.session;
                    self.state = b.state;
                    // Boot installs the session directly (no `adopt_switch`), so the markdown
                    // reading-view default is applied here (docs/markdown-view.md §1.6); an
                    // `ae file:line` launch is jump-shaped and lands in the editor.
                    let read_fx = self.session.boot_read_presentation(jumped);
                    self.sent_grid = Some(self.grid());
                    self.subscribe();
                    self.run_effects(b.startup);
                    self.run_effects(read_fx);
                }
                // Daemon not up yet — keep dialing (the whole point of launching first).
                Err(ReconnectError::NotUp) => {
                    self.boot_attempt = self.boot_attempt.saturating_add(1);
                    self.spawn_boot_dial();
                }
                // Terminal: either a live server whose bootstrap refused (e.g. unknown CLI
                // workspace) or a version mismatch (a stale daemon holding the port). Retrying
                // can't help — end the run, printing the message once the terminal is restored.
                Err(ReconnectError::Fatal(e)) => {
                    self.fatal = Some(e);
                    self.should_quit = true;
                }
            },
        }
    }

    // ---- terminal events ---------------------------------------------------------------

    async fn on_terminal_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k).await,
            Event::Mouse(m) => self.on_mouse(m),
            Event::Resize(cols, rows) => self.on_resize(cols, rows),
            _ => {}
        }
    }

    async fn on_key(&mut self, k: KeyEvent) {
        // Hover: the popover reuses the editor's own Copy / Scroll bindings (resolved by
        // `keymap::hover_action`, so the chords never drift) — Ctrl-c copies its content, the scroll
        // keys pan it and keep it open; any other key dismisses it.
        if self.state.hover.is_some() {
            if let Some((code, mods, _)) = translate_key(&k) {
                match hover_action(code, mods) {
                    // The terminal can't drag-select a cell-grid overlay, so copy-all is the
                    // affordance. Leaves the popover open.
                    Some(HoverAction::Copy) => {
                        if let Some(h) = self.state.hover.as_ref() {
                            let text = h.body.to_plain_text();
                            match clipboard::copy(&mut self.state.clipboard, text) {
                                Ok(()) => self
                                    .status(StatusMessage::success("Copied popover".to_string())),
                                Err(e) => {
                                    self.status(StatusMessage::error(format!("Copy failed: {e}")))
                                }
                            }
                        }
                        return;
                    }
                    Some(HoverAction::Scroll { dir, unit }) => {
                        if let Some(h) = self.state.hover.as_mut() {
                            let down = matches!(dir, ScrollDir::Down);
                            match unit {
                                ScrollUnit::Line => h.scroll.scroll_by(if down { 1 } else { -1 }),
                                ScrollUnit::Half => h.scroll.half(down),
                                ScrollUnit::Page => h.scroll.page(down),
                            }
                        }
                        return;
                    }
                    None => {}
                }
            }
            self.state.hover = None;
            return;
        }
        // App-info dialog: the scroll keys pan it and keep it open, exactly as they do the hover
        // popover (same `hover_action` resolution, so the chords can't drift from the editor's).
        // Everything else — including `y` (copy) — falls through to the core, which owns the
        // dialog's "copy or close" rule. Without this the terminal would have no keyboard way to
        // reach the bottom of the dialog in a short window; the GUI and web get it from their
        // native scrollables.
        if self.state.app_info.is_some() {
            if let Some((code, mods, _)) = translate_key(&k) {
                if let Some(HoverAction::Scroll { dir, unit }) = hover_action(code, mods) {
                    if let Some(v) = self.state.app_info.as_mut() {
                        let down = matches!(dir, ScrollDir::Down);
                        match unit {
                            ScrollUnit::Line => v.scroll.scroll_by(if down { 1 } else { -1 }),
                            ScrollUnit::Half => v.scroll.half(down),
                            ScrollUnit::Page => v.scroll.page(down),
                        }
                    }
                    return;
                }
            }
        }
        let Some((code, mods, text)) = translate_key(&k) else {
            return;
        };
        let visible_rows = self.visible_rows();
        // A focused overlay input (save-as, etc.) is edited shell-side: the shell owns the caret
        // and text mechanics, syncing the whole value into the core. Command keys (commit / cancel
        // / nav / chord) still route through the core's keycode dispatch.
        self.sync_overlay_edit();
        if let Some(edit) = self.overlay_edit.as_mut() {
            use crate::overlay_input::{classify, is_command_override, KeyClass};
            let is_text = matches!(classify(code, mods), KeyClass::Text)
                && !is_command_override(edit.field, code, edit.input.cursor);
            if is_text {
                if crate::overlay_input::apply_text_key(&mut edit.input, code, text) {
                    let (field, value) = (edit.field, edit.input.text.clone());
                    let fx = self.set_overlay_field(field, value);
                    self.run_effects(fx);
                }
                return;
            }
        }
        let fx = self.session.on_key(code, mods, text, visible_rows);
        self.run_effects(fx);
    }

    /// Keep the shell-owned overlay editor in step with the focused field. Mirrors iced's
    /// `desired_focus` + reseed and its post-key caret resync:
    ///
    /// - focus moved to a new field → seed the editor from the core's value (caret at end);
    /// - focus left every field → drop the editor;
    /// - same field, but the core rewrote the value out-of-band (search history recall,
    ///   chip-editor tab-complete) → adopt the new value (caret at end). A normal text edit already
    ///   kept the two equal, so this only fires for core-driven changes, never clobbering a live
    ///   in-field caret.
    ///
    /// Cheap enough to call before every key and every render `sync`.
    fn sync_overlay_edit(&mut self) {
        let desired = self.desired_overlay_field();
        let current = self.overlay_edit.as_ref().map(|e| e.field);
        match (desired, current) {
            (Some(f), Some(c)) if f == c => {
                let value = self.overlay_field_value(f);
                if let Some(edit) = self.overlay_edit.as_mut() {
                    if edit.input.text != value {
                        edit.input.set(value); // adopt the core-driven rewrite, caret to end
                    }
                }
            }
            (Some(f), _) => {
                let value = self.overlay_field_value(f);
                self.overlay_edit = Some(crate::overlay_input::OverlayEdit {
                    field: f,
                    input: crate::text_input::TextInput::new(value),
                });
            }
            (None, Some(_)) => self.overlay_edit = None,
            (None, None) => {}
        }
    }

    /// Which overlay text field is focused, if any — the TUI counterpart of iced's `desired_focus`.
    fn desired_overlay_field(&self) -> Option<crate::overlay_input::OverlayField> {
        use crate::overlay_input::OverlayField;
        // A modal prompt owns the keyboard. Save-as has a text field; confirm / lsp-info don't
        // (their keys are commands), so they suppress any overlay editor behind them.
        if let Some(prompt) = &self.session.prompt {
            return match prompt {
                // Multi-root save-as has a leading root-typeahead segment; focus follows the core
                // editor's `field`. Single-root workspaces only ever have the path segment.
                Prompt::SaveAs(ed) => Some(
                    if self.session.workspace_paths.len() > 1
                        && ed.field == aether_client::chips::ChipEditorField::Root
                    {
                        OverlayField::SaveAsRoot
                    } else {
                        OverlayField::SaveAs
                    },
                ),
                Prompt::OpenPath(_) => Some(OverlayField::OpenPath),
                _ => None,
            };
        }
        if self.session.mode == Mode::Search {
            // A selected option chip makes every key a command (chip-row nav / remove / cycle), so
            // drop the editor and let the core's `on_search_key` own the keyboard.
            return self
                .session
                .search
                .chip_selected
                .is_none()
                .then_some(OverlayField::Search);
        }
        // The picker query owns the keyboard while open — unless the chip editor is open (its own
        // root/path segments take focus) or a chip is selected (all keys are commands then).
        if let Some(p) = &self.session.picker {
            if let Some(ed) = &p.chip_editor {
                return Some(match ed.field {
                    aether_client::chips::ChipEditorField::Root => OverlayField::ChipRoot,
                    aether_client::chips::ChipEditorField::Path => OverlayField::ChipPath,
                });
            }
            if p.chip_selected.is_none() {
                return Some(OverlayField::PickerQuery);
            }
            return None;
        }
        if let Some(ps) = &self.session.workspace_settings {
            use aether_client::chips::ChipEditorField;
            use aether_client::session::SettingsRow;
            return match ps.row() {
                SettingsRow::Name => Some(OverlayField::WorkspaceName),
                SettingsRow::AddRoot => Some(OverlayField::WorkspaceAddRoot),
                // The add-project row is a two-segment path editor; focus follows whichever
                // segment it has active (the root typeahead only exists in multi-root workspaces).
                SettingsRow::AddProject => Some(if ps.on_add_project_language {
                    OverlayField::WorkspaceAddProjectLanguage
                } else if self.session.workspace_paths.len() > 1
                    && ps.add_project.field == ChipEditorField::Root
                {
                    OverlayField::WorkspaceAddProjectRoot
                } else {
                    OverlayField::WorkspaceAddProject
                }),
                // A root/project row is a selection, not a text field.
                SettingsRow::Root(_) | SettingsRow::Project(_) => None,
            };
        }
        None
    }

    /// The core's current value for an overlay field — the seed when (re)focusing it.
    fn overlay_field_value(&self, field: crate::overlay_input::OverlayField) -> String {
        use crate::overlay_input::OverlayField;
        match field {
            OverlayField::SaveAs => match &self.session.prompt {
                Some(Prompt::SaveAs(ed)) => ed.input.text.clone(),
                _ => String::new(),
            },
            OverlayField::SaveAsRoot => match &self.session.prompt {
                Some(Prompt::SaveAs(ed)) => ed.root_filter.text.clone(),
                _ => String::new(),
            },
            OverlayField::OpenPath => match &self.session.prompt {
                Some(Prompt::OpenPath(field)) => field.text.clone(),
                _ => String::new(),
            },
            OverlayField::Search => self.session.search.query.clone(),
            OverlayField::WorkspaceName => self
                .session
                .workspace_settings
                .as_ref()
                .map(|s| s.name.text.clone())
                .unwrap_or_default(),
            OverlayField::WorkspaceAddProject => self
                .session
                .workspace_settings
                .as_ref()
                .map(|s| s.add_project.input.text.clone())
                .unwrap_or_default(),
            OverlayField::WorkspaceAddProjectRoot => self
                .session
                .workspace_settings
                .as_ref()
                .map(|s| s.add_project.root_filter.text.clone())
                .unwrap_or_default(),
            OverlayField::WorkspaceAddProjectLanguage => self
                .session
                .workspace_settings
                .as_ref()
                .map(|s| s.add_project_language.text.clone())
                .unwrap_or_default(),
            OverlayField::WorkspaceAddRoot => self
                .session
                .workspace_settings
                .as_ref()
                .map(|s| s.add.text.clone())
                .unwrap_or_default(),
            OverlayField::PickerQuery => self
                .session
                .picker
                .as_ref()
                .map(|p| p.query.clone())
                .unwrap_or_default(),
            OverlayField::ChipRoot => self
                .session
                .picker
                .as_ref()
                .and_then(|p| p.chip_editor.as_ref())
                .map(|ed| ed.root_filter.text.clone())
                .unwrap_or_default(),
            OverlayField::ChipPath => self
                .session
                .picker
                .as_ref()
                .and_then(|p| p.chip_editor.as_ref())
                .map(|ed| ed.input.text.clone())
                .unwrap_or_default(),
        }
    }

    /// Sync an overlay field's new value into the core — the sink for the shell-owned editor,
    /// mirroring iced's `overlay_set`.
    fn set_overlay_field(
        &mut self,
        field: crate::overlay_input::OverlayField,
        value: String,
    ) -> Effects {
        use crate::overlay_input::OverlayField;
        match field {
            OverlayField::SaveAs => self.session.save_as_set_input(value),
            OverlayField::SaveAsRoot => self.session.save_as_set_root_filter(value),
            OverlayField::OpenPath => self.session.open_path_set_input(value),
            OverlayField::Search => self.session.search_set_query(value),
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
            OverlayField::PickerQuery => self.session.picker_set_query(value),
            OverlayField::ChipRoot => self.session.chip_editor_set_root_filter(value),
            OverlayField::ChipPath => self.session.chip_editor_set_input(value),
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        // The app-info dialog is modal: like the picker, it owns the screen while up, so neither a
        // click nor the wheel may reach the buffer behind it. The wheel scrolls the dialog (the
        // Paths section runs past a short terminal); a click inside is swallowed, and a click on
        // the backdrop dismisses it — the same "click the editor behind to cancel" rule the other
        // prompts document.
        if self.state.app_info.is_some() {
            match m.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                        -3
                    } else {
                        3
                    };
                    if let Some(v) = self.state.app_info.as_mut() {
                        v.scroll.scroll_by(delta);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    let inside = ui::app_info_rect(&self.state)
                        .map(|r| {
                            m.column >= r.x
                                && m.column < r.x + r.width
                                && m.row >= r.y
                                && m.row < r.y + r.height
                        })
                        .unwrap_or(false);
                    if !inside {
                        let fx = self.session.decline_prompt();
                        self.run_effects(fx);
                    }
                }
                _ => {}
            }
            return;
        }
        // While a picker is open its overlay owns the screen: the wheel moves the highlight, a
        // press lands on the picker (see `on_picker_press`) — never on the buffer underneath.
        if self.session.picker.is_some() {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    let fx = self.session.picker_wheel(-1);
                    self.run_effects(fx);
                }
                MouseEventKind::ScrollDown => {
                    let fx = self.session.picker_wheel(1);
                    self.run_effects(fx);
                }
                MouseEventKind::Down(MouseButton::Left) => self.on_picker_press(m),
                _ => {}
            }
            return;
        }
        // The hover popover owns the wheel while the cursor is over it (scrolls the popover, not the
        // buffer behind), and any click dismisses it — a click *inside* is consumed (so it doesn't
        // also move the editor cursor), a click *outside* falls through to the editor below.
        if self.state.hover.is_some() {
            let over = ui::hover_rect(&self.state)
                .map(|r| {
                    m.column >= r.x
                        && m.column < r.x + r.width
                        && m.row >= r.y
                        && m.row < r.y + r.height
                })
                .unwrap_or(false);
            match m.kind {
                MouseEventKind::ScrollUp if over => {
                    if let Some(h) = self.state.hover.as_mut() {
                        h.scroll.scroll_by(-3);
                    }
                    return;
                }
                MouseEventKind::ScrollDown if over => {
                    if let Some(h) = self.state.hover.as_mut() {
                        h.scroll.scroll_by(3);
                    }
                    return;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.state.hover = None;
                    if over {
                        return;
                    }
                    // Click outside the popover: fall through and treat it as an editor click.
                }
                _ => {}
            }
        }
        // The reading view owns the mouse while active: the wheel scrolls the document,
        // horizontal wheel (or shift+wheel) pans the code block under the pointer, a left
        // press focuses the element under it (docs/markdown-view.md §2.3).
        if self.session.read.is_some() {
            match m.kind {
                MouseEventKind::ScrollLeft => {
                    let e = self.read_element_at_row(m.row);
                    self.read_hscroll_by(e, -6);
                }
                MouseEventKind::ScrollRight => {
                    let e = self.read_element_at_row(m.row);
                    self.read_hscroll_by(e, 6);
                }
                MouseEventKind::ScrollUp if m.modifiers.contains(KeyModifiers::SHIFT) => {
                    let e = self.read_element_at_row(m.row);
                    self.read_hscroll_by(e, -6);
                }
                MouseEventKind::ScrollDown if m.modifiers.contains(KeyModifiers::SHIFT) => {
                    let e = self.read_element_at_row(m.row);
                    self.read_hscroll_by(e, 6);
                }
                MouseEventKind::ScrollUp => self.read_scroll_by(-3),
                MouseEventKind::ScrollDown => self.read_scroll_by(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((byte, interactive)) = self.read_hit(m.row, m.column) {
                        // A click ON a link/footnote-ref span follows it like Enter (the
                        // core keeps images arm-only); elsewhere it just focuses.
                        let fx = if interactive {
                            self.session.read_click_activate(byte)
                        } else {
                            self.session.read_click(byte)
                        };
                        self.run_effects(fx);
                    }
                }
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::Down(MouseButton::Left) => self.on_left_press(m),
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(pos) = ui::screen_to_logical(&self.state, m.row, m.column) {
                    let fx = self.session.pointer_drag(pos);
                    self.run_effects(fx);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.session.pointer_release(),
            _ => {}
        }
    }

    /// A left press while the picker overlay is up, hit-tested against what was drawn:
    ///
    /// - a result row selects *and* accepts it — `PickerClicked` is the same core path Enter takes,
    ///   shared with the native and web clients (row click opens the file, drills into a directory,
    ///   creates from the synthetic "+ Create …" row, …);
    /// - the dim backdrop dismisses the picker (the other clients' click-away gesture), except for
    ///   the mandatory chooser, which no dismissal gesture may close;
    /// - the box's own chrome (input row, group headers, borders) is swallowed.
    fn on_picker_press(&mut self, m: MouseEvent) {
        // Shift-click is the terminal's own text selection, as in the buffer — leave it alone.
        if m.modifiers.contains(KeyModifiers::SHIFT) {
            return;
        }
        // A confirm prompt raised from the picker (delete this file, discard that buffer) owns
        // input while it's up, as it does for the keyboard: a press declines it rather than
        // slipping an accept past it — the native client's click-away-declines rule.
        if self.session.prompt.is_some() {
            let fx = self.session.decline_prompt();
            self.run_effects(fx);
            return;
        }
        let (cols, rows) = self.term;
        match ui::picker_hit(&self.state, cols, rows, m.row, m.column) {
            ui::PickerHit::Item(i) => {
                // The hit is a window-relative index; the core selects by absolute index.
                let abs = self.state.picker.offset + i as u32;
                self.dispatch(CoreEvent::PickerClicked(abs));
            }
            ui::PickerHit::Backdrop if !self.session.picker_is_mandatory() => {
                let fx = self.session.close_picker();
                self.run_effects(fx);
            }
            ui::PickerHit::Backdrop | ui::PickerHit::Chrome => {}
        }
    }

    fn on_left_press(&mut self, m: MouseEvent) {
        // Shift-click is reserved for the terminal's own text selection (copy/paste), so we
        // don't hijack it for an editor selection.
        if m.modifiers.contains(KeyModifiers::SHIFT) {
            return;
        }
        let Some(pos) = ui::screen_to_logical(&self.state, m.row, m.column) else {
            return;
        };
        // Synthesize the click streak from a same-cell press chain (terminals report no count).
        let now = std::time::Instant::now();
        let streak = match self.last_click {
            Some((at, row, col))
                if (row, col) == (m.row, m.column)
                    && now.duration_since(at) <= MULTI_CLICK_WINDOW =>
            {
                self.click_streak + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, m.row, m.column));
        self.click_streak = streak;
        let granularity = match streak {
            1 => aether_protocol::cursor::Granularity::Char,
            2 => aether_protocol::cursor::Granularity::Word,
            _ => aether_protocol::cursor::Granularity::Line,
        };
        let fx = self.session.pointer_press(pos, granularity, false);
        self.run_effects(fx);
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.term = (cols, rows);
        self.sent_grid = Some(self.grid());
        let Some(viewport_id) = self.session.viewport_id else {
            return;
        };
        let (cols, rows) = self.grid();
        let h = self.handle.clone();
        let fut = async move {
            h.rpc::<ViewportResize>(ViewportResizeParams {
                viewport_id,
                cols,
                rows,
            })
            .await
        };
        self.pending
            .push(Box::pin(async move { Done::Window(fut.await) }));
    }

    // ---- shell actions (geometry + local overlays) --------------------------------------

    fn run_shell_action(&mut self, action: ShellAction) {
        match action {
            // A local image beside the buffer: the TUI opens the absolute path (the web half
            // of this action rides the asset route instead).
            ShellAction::OpenBufferFile { absolute, .. } => {
                self.run_shell_action(ShellAction::OpenUrl(absolute));
            }
            // Reading-view Enter on an external link or image. The TUI is a local process, so
            // the same allow-listed system opener the GUI uses works here; bare absolute paths
            // ride the `file:` scheme. Best-effort — a spawn failure is silent.
            ShellAction::OpenUrl(url) => {
                let target = match url.strip_prefix('/') {
                    Some(p) => format!("file:///{p}"),
                    None => url,
                };
                if ["http://", "https://", "mailto:", "file:"]
                    .iter()
                    .any(|s| target.starts_with(s))
                {
                    let program = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    let _ = std::process::Command::new(program)
                        .arg(&target)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
            ShellAction::Scroll { dir, unit } => {
                let rows = self.visible_rows() as i64;
                let delta = match unit {
                    ScrollUnit::Line => 1,
                    ScrollUnit::Half => (rows / 2).max(1),
                    ScrollUnit::Page => rows.max(1),
                };
                // In the reading view the vertical scroll is the document's; Left/Right pan
                // the *focused* block's horizontal window (only code rows can overflow, so
                // this is a clamped no-op anywhere else).
                if self.session.read.is_some() {
                    match dir {
                        ScrollDir::Up => self.read_scroll_by(-(delta as i32)),
                        ScrollDir::Down => self.read_scroll_by(delta as i32),
                        ScrollDir::Left | ScrollDir::Right => {
                            let (content_cols, _) =
                                aether_client::read_layout::measure(self.term.0.max(10));
                            let window = content_cols.saturating_sub(2).max(1) as i32;
                            let step = match unit {
                                ScrollUnit::Line => 6,
                                ScrollUnit::Half => (window / 2).max(1),
                                ScrollUnit::Page => window,
                            };
                            let focused =
                                self.session.read.as_ref().and_then(|r| {
                                    r.block_focus(self.session.buffer.cursor.position)
                                });
                            let signed = if matches!(dir, ScrollDir::Left) {
                                -step
                            } else {
                                step
                            };
                            self.read_hscroll_by(focused, signed);
                        }
                    }
                    return;
                }
                match dir {
                    ScrollDir::Up => self.scroll_by(-delta),
                    ScrollDir::Down => self.scroll_by(delta),
                    ScrollDir::Left | ScrollDir::Right => {
                        // Horizontal scroll only bites when soft wrap is off (wrapped text never
                        // overflows right). A `Half` unit pans half a screen; a line pans one col.
                        if self.session.wrap == WrapMode::None {
                            let cols = self.state.viewport_cols as i64;
                            let mag = match unit {
                                ScrollUnit::Half => (cols / 2).max(1),
                                _ => 1,
                            };
                            let signed = if matches!(dir, ScrollDir::Left) {
                                -mag
                            } else {
                                mag
                            };
                            self.scroll_col = (self.scroll_col as i64 + signed).max(0) as u32;
                        }
                    }
                }
            }
            ShellAction::PlaceCursor(place) => self.place_cursor(place),
            ShellAction::ToggleWrap => {
                self.session.wrap = match self.session.wrap {
                    WrapMode::Soft => WrapMode::None,
                    WrapMode::None => WrapMode::Soft,
                };
                self.sent_grid = Some(self.grid());
                self.subscribe();
            }
            // "Open another window" (both the `Space z` duplicate and "open picker item in a
            // new window") is GUI-only — a new OS window makes no sense for the terminal client,
            // which owns the one terminal it was launched in. Ignore it here.
            ShellAction::NewWindow(_) => {}
        }
    }

    // ---- viewport geometry (iced's px math, in rows) -------------------------------------

    fn subscribe(&mut self) {
        if self.session.is_placeholder() {
            return; // no buffer to show until a workspace is picked (the no-workspace view)
        }
        let Some((cols, rows)) = self.sent_grid else {
            return;
        };
        // A fresh subscribe invalidates any in-flight fetch (new viewport identity); the core
        // no longer resets these on switch/reconnect — they live here now.
        self.fetch_in_flight = false;
        self.refetch_queued = false;
        self.reveal_after_fetch = None;
        // A pending relayout anchor (wrap toggle) wins: load a window around its reference line so
        // the anchor can be resolved precisely once it arrives. Otherwise restore the buffer's
        // saved scroll, else center on the cursor.
        let scroll = if let Some(line) = self.session.relayout_anchor_line() {
            ScrollPosition {
                logical_line: line,
                sub_row: 0.0,
            }
        } else {
            self.session.buffer.scroll.unwrap_or(ScrollPosition {
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
            })
        };
        self.subscribe_scroll = scroll;
        self.subscribe_epoch += 1;
        let epoch = self.subscribe_epoch;
        let h = self.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let wrap = self.session.wrap;
        let diff_view = self.session.diff_view;
        let fut = async move {
            h.rpc::<ViewportSubscribe>(ViewportSubscribeParams {
                buffer_id,
                cols,
                rows,
                overscan_rows: rows,
                scroll,
                wrap,
                continuation_marker_width: 2,
                tab_width: TAB_WIDTH,
                diff_view,
            })
            .await
        };
        self.pending
            .push(Box::pin(async move { Done::Subscribed(epoch, fut.await) }));
    }

    fn max_scroll_row(&self) -> u32 {
        match &self.session.window {
            Some(w) => w
                .total_visual_rows
                .saturating_sub(self.visible_rows())
                .min(w.total_visual_rows),
            None => 0,
        }
    }

    fn clamp_scroll(&mut self) {
        self.top_visual_row = self.top_visual_row.min(self.max_scroll_row());
    }

    fn scroll_to_row(&mut self, row: u32) {
        self.top_visual_row = row.min(self.max_scroll_row());
        self.maybe_fetch();
    }

    fn scroll_by(&mut self, delta: i64) {
        let target = (self.top_visual_row as i64 + delta).max(0) as u32;
        self.scroll_to_row(target);
    }

    /// Fetch a new window when the view nears the loaded range's edge (iced's
    /// `maybe_fetch`, row units).
    fn maybe_fetch(&mut self) {
        let (Some(window), Some(viewport_id)) = (&self.session.window, self.session.viewport_id)
        else {
            return;
        };
        let loaded_start = window.first_visual_row;
        let loaded_end = loaded_start + loaded_rows(window);
        let margin = self.visible_rows();
        let visible = self.visible_rows();
        let top_row = self.top_visual_row;
        let need_above = loaded_start > 0 && top_row < loaded_start.saturating_add(margin);
        let need_below = loaded_end < window.total_visual_rows
            && top_row + visible > loaded_end.saturating_sub(margin);
        if !(need_above || need_below) {
            return;
        }
        if self.fetch_in_flight {
            self.refetch_queued = true;
            return;
        }
        self.fetch_in_flight = true;
        let h = self.handle.clone();
        let fut = async move {
            h.rpc::<ViewportScrollToRow>(ViewportScrollToRowParams {
                viewport_id,
                top_visual_row: top_row,
            })
            .await
        };
        self.pending
            .push(Box::pin(async move { Done::Window(fut.await) }));
    }

    /// After a cursor move: fetch around the cursor when it left the loaded window,
    /// otherwise scroll the minimum to reveal it.
    fn ensure_cursor_visible(&mut self, style: RevealStyle) {
        let Some(window) = &self.session.window else {
            return;
        };
        let line = self.session.buffer.cursor.position.line;
        if line < window.first_logical_line || line >= window.last_logical_line_exclusive {
            let Some(viewport_id) = self.session.viewport_id else {
                return;
            };
            self.reveal_after_fetch = Some(style);
            self.fetch_in_flight = true;
            let h = self.handle.clone();
            let fut = async move {
                h.rpc::<ViewportScroll>(ViewportScrollParams {
                    viewport_id,
                    scroll: ScrollPosition {
                        logical_line: line,
                        sub_row: 0.0,
                    },
                })
                .await
            };
            self.pending
                .push(Box::pin(async move { Done::Window(fut.await) }));
            return;
        }
        self.reveal_cursor_styled(style);
        self.maybe_fetch();
    }

    fn reveal_cursor_styled(&mut self, style: RevealStyle) {
        match style {
            RevealStyle::Follow => self.reveal_cursor(),
            RevealStyle::Jump => self.reveal_cursor_jump(),
        }
    }

    /// Minimal vertical reveal: scroll just enough to bring the cursor on-screen, leaving it where
    /// it already is when visible. The follow behaviour for ordinary motions.
    fn reveal_cursor(&mut self) {
        self.reveal_cursor_col();
        let Some(window) = &self.session.window else {
            return;
        };
        let Some(row) = cursor_visual_row(window, self.session.buffer.cursor.position) else {
            return;
        };
        let visible = self.visible_rows();
        if row < self.top_visual_row {
            self.top_visual_row = row;
        } else if row + 1 > self.top_visual_row + visible {
            self.top_visual_row = row + 1 - visible;
        }
        self.maybe_fetch();
    }

    /// Jump reveal: if the cursor is already visible, leave the view; otherwise rest it near the top
    /// of the viewport (more context below). The TUI snaps — only the GUI/web animate.
    fn reveal_cursor_jump(&mut self) {
        self.reveal_cursor_col();
        let Some(window) = &self.session.window else {
            return;
        };
        let Some(row) = cursor_visual_row(window, self.session.buffer.cursor.position) else {
            return;
        };
        let visible = self.visible_rows();
        if row >= self.top_visual_row && row < self.top_visual_row + visible {
            self.maybe_fetch(); // already visible — don't disturb the view
            return;
        }
        let above = (visible as f32 * CURSOR_REST_FRACTION) as u32;
        self.scroll_to_row(row.saturating_sub(above));
    }

    /// Keep the cursor's column inside the horizontal window — no-wrap only, since soft wrap
    /// never overflows to the right. Pure client-side, mirroring the renderer's `scroll_col`
    /// drop. Shared by every cursor-reveal path.
    fn reveal_cursor_col(&mut self) {
        if self.session.wrap != WrapMode::None {
            self.scroll_col = 0;
            return;
        }
        let cols = self.state.viewport_cols;
        if cols == 0 {
            return;
        }
        let col = self.session.buffer.cursor.position.col;
        if col < self.scroll_col {
            self.scroll_col = col;
        } else if col >= self.scroll_col.saturating_add(cols) {
            self.scroll_col = col.saturating_sub(cols.saturating_sub(1));
        }
    }

    fn place_cursor(&mut self, place: ViewportPlace) {
        // Reading view: edge-matched placement of the focused *block* (the bar'd subtree —
        // docs/markdown-view.md §2.3): `;` leaves READ_GAP between the view top and the
        // block's top, `Alt-;` between the view bottom and the block's bottom. Read scroll is
        // shell-owned rows (§3.1); the document-end clamp happens in `read_view`.
        if let Some(read) = self.session.read.as_ref() {
            let cursor = self.session.buffer.cursor.position;
            let Some(focus) = read.block_focus(cursor) else {
                return;
            };
            let Some(rows) = self.read_cache.as_ref().map(|c| c.4.clone()) else {
                return;
            };
            let span = read.elements[focus].span();
            let mut first = None;
            let mut last = 0usize;
            for (i, row) in rows.iter().enumerate() {
                let inside = row.element.is_some_and(|e| {
                    let s = read.elements[e].span();
                    s.start >= span.start && s.end <= span.end
                });
                if inside {
                    if first.is_none() {
                        first = Some(i);
                    }
                    last = i;
                }
            }
            let Some(first) = first else { return };
            let pad = f32::from(crate::ui::READ_PAD_TOP);
            let visible = self.visible_rows().max(1) as f32;
            let gap = visible * ViewportPlace::READ_GAP;
            let target = match place {
                ViewportPlace::Upper => first as f32 + pad - gap,
                ViewportPlace::Lower => (last + 1) as f32 + pad - (visible - gap),
            };
            self.read_scroll = target.max(0.0) as u16;
            return;
        }
        let line = self.session.buffer.cursor.position.line;
        let loaded = self
            .session
            .window
            .as_ref()
            .map(|w| (w.first_logical_line, w.last_logical_line_exclusive));
        let Some((first, last)) = loaded else {
            return;
        };
        // When the cursor's line has been scrolled out of the loaded window, its visual row is
        // unknown — pull that region from the server (scrolling the viewport to the line), then
        // place once it lands. Mirrors `ensure_cursor_visible`.
        if line < first || line >= last {
            let Some(viewport_id) = self.session.viewport_id else {
                return;
            };
            self.place_after_fetch = Some(place);
            self.fetch_in_flight = true;
            let h = self.handle.clone();
            let fut = async move {
                h.rpc::<ViewportScroll>(ViewportScrollParams {
                    viewport_id,
                    scroll: ScrollPosition {
                        logical_line: line,
                        sub_row: 0.0,
                    },
                })
                .await
            };
            self.pending
                .push(Box::pin(async move { Done::Window(fut.await) }));
            return;
        }
        self.place_cursor_in_window(place);
    }

    /// Scroll so the cursor's line sits a fixed fraction down the viewport. Assumes its line is in
    /// the loaded window (the caller pulls it in first otherwise); a no-op if its row isn't known.
    fn place_cursor_in_window(&mut self, place: ViewportPlace) {
        self.reveal_cursor_col();
        let Some(window) = &self.session.window else {
            return;
        };
        let Some(row) = cursor_visual_row(window, self.session.buffer.cursor.position) else {
            return;
        };
        let above = (self.visible_rows() as f32 * place.fraction()) as u32;
        self.scroll_to_row(row.saturating_sub(above));
    }

    // ---- blame (formatted shell-side: "3w ago" needs a clock) ----------------------------

    fn maybe_blame(&mut self) {
        if self.session.mode != Mode::Normal {
            return;
        }
        let line = self.session.buffer.cursor.position.line;
        let key = (line, self.session.buffer.revision);
        if self.session.buffer.path.is_none() {
            self.session.blame = None;
            return;
        }
        if self.session.blame_requested == Some(key) {
            return;
        }
        self.session.blame_requested = Some(key);
        if self.session.blame.as_ref().is_some_and(|(l, _)| *l != line) {
            self.session.blame = None;
        }
        let buffer_id = self.session.buffer.buffer_id;
        let h = self.handle.clone();
        let fut = async move {
            h.rpc::<GitBlameLine>(GitBlameLineParams {
                buffer_id,
                line,
                include_commit_info: false,
            })
            .await
        };
        self.pending.push(Box::pin(async move {
            let text = fut.await.ok().and_then(|r| r.blame).map(|b| {
                if b.is_uncommitted {
                    "uncommitted".into()
                } else {
                    format!("{} · {}", b.author, time_ago(b.timestamp))
                }
            });
            Done::Blame {
                buffer_id,
                line,
                text,
            }
        }));
    }

    // ---- reconnect (the dial loop; policy lives in the core) ------------------------------

    /// Recovery when a reconnect succeeds but there's no workspace to restore: either none was
    /// active (the chooser was up when the connection dropped — land back in it, quietly) or the
    /// one we had is gone (renamed/removed while away). Reset to a placeholder session over the
    /// fresh connection and raise the Workspaces chooser, mirroring a no-args start. Picking a
    /// workspace (a renamed one shows under its new name) lands a buffer the usual way.
    fn reconnect_to_chooser(&mut self) {
        let was_choosing = self.session.is_placeholder();
        self.enter_chooser();
        self.status(if was_choosing {
            StatusMessage::success("Reconnected".to_string())
        } else {
            StatusMessage::error("workspace no longer exists — pick another".to_string())
        });
    }

    /// Drop to the workspace chooser over the live connection: swap in a fresh placeholder session
    /// (no buffer, so nothing stale renders behind the picker) and raise the Workspaces picker.
    /// Driven by [`Effect::ToChooser`] when an ephemeral context we navigated into loses its last
    /// buffer, and reused by [`Self::reconnect_to_chooser`] for workspace-gone recovery.
    fn enter_chooser(&mut self) {
        use aether_protocol::picker::PickerKind;
        self.session = Session::placeholder(); // conn = Connected, so notifications resume
        let startup = self
            .session
            .open_picker(PickerKind::Workspaces, None, None, false, None);
        self.run_effects(startup);
    }

    /// Push the boot dial (connect + bootstrap) onto the pending set; its `Done::Booted` result
    /// installs the session or schedules a retry. No-op once boot has completed (`boot` is `None`).
    fn spawn_boot_dial(&mut self) {
        let Some(spec) = self.boot.clone() else {
            return;
        };
        let attempt = self.boot_attempt;
        let (cols, rows) = self.term;
        let server_url = self.server_url.clone();
        self.pending.push(Box::pin(async move {
            Done::Booted(Box::new(
                boot_dial(attempt, spec, cols, rows, server_url).await,
            ))
        }));
    }

    fn spawn_reconnect(&mut self, attempt: u32) {
        let workspace = self.session.workspace.clone();
        let path = self.session.buffer.path.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let transient = self.session.buffer.transient;
        let cursor = self.session.buffer.cursor.position;
        let version = env!("CARGO_PKG_VERSION").to_string();
        let server_url = self.server_url.clone();
        self.pending.push(Box::pin(async move {
            Done::Reconnected(Box::new(
                dial(
                    attempt, workspace, path, buffer_id, transient, cursor, version, server_url,
                )
                .await,
            ))
        }));
    }

    // ---- view sync (Session → the render model ui::draw reads) ---------------------------

    fn sync(&mut self) {
        // Report the current on-screen line range to the core (it owns no pixel scroll), so sneak
        // scopes its labels to what's actually visible rather than the overscan-padded window.
        self.session
            .set_visible_lines(self.top_visual_row, self.visible_rows());
        // Keep the shell-owned overlay editor in step with the focused field before projecting any
        // overlay state into the view model below.
        self.sync_overlay_edit();
        // No editor until a workspace is picked: the placeholder session renders the no-workspace
        // view, not a buffer behind the chooser.
        let editor = (!self.session.is_placeholder()).then(|| self.editor_view());
        let read = self.read_view();
        let s = &self.session;
        let st = &mut self.state;
        st.workspace_name = s.workspace.clone();
        st.tether = s.tether;
        if st.workspace_paths != s.workspace_paths {
            st.workspace_paths = s.workspace_paths.clone();
            st.root_labels = labels::root_labels(&st.workspace_paths);
        }
        let (cols, rows) = self.term;
        st.viewport_cols = cols as u32;
        st.viewport_rows = (rows as u32).saturating_sub(1);
        st.conn = s.conn;
        st.hint = s.hint_view().map(|h| {
            let (before, keys, after) = h.parts();
            (before.to_string(), keys.to_string(), after.to_string())
        });
        st.pending_leader = match s.pending {
            Pending::Leader => Some(PendingLeader::Space),
            _ => None,
        };
        st.lsp_status = match (&s.lsp, &s.buffer.lsp_server) {
            (Some(status), Some(server)) => {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    (server.language.clone(), server.workspace_root.clone()),
                    status.clone(),
                );
                m
            }
            _ => Default::default(),
        };
        st.diagnostic_counts = {
            let mut m = std::collections::HashMap::new();
            m.insert(s.buffer.buffer_id, s.diagnostics);
            m
        };

        st.editor = editor;
        st.read = read;
        self.sync_picker();
        self.sync_prompts();
        self.sync_workspace_settings();
        self.sync_app_settings();
    }

    /// Mirror the core's app-settings overlay (`session.app_settings`) into the view model. A pure
    /// projection — the core owns the state, key handling, and the grouped settings
    /// (`app_setting_groups`).
    fn sync_app_settings(&mut self) {
        self.state.app_settings =
            self.session
                .app_settings
                .as_ref()
                .map(|core| crate::app::AppSettingsState {
                    groups: self.session.app_setting_groups(),
                    selected: core.selected,
                });
    }

    /// Mirror the core's workspace-settings overlay (`session.workspace_settings`) into the view
    /// model the renderer reads. The core owns the state and key handling now; this is a pure
    /// projection into the shell's `TextInput`-based struct (`WorkspaceSettingsState`).
    fn sync_workspace_settings(&mut self) {
        let Some(core) = &self.session.workspace_settings else {
            self.state.workspace_settings = None;
            return;
        };
        // The caret for the focused field lives in the shell-owned overlay editor (text mechanics
        // are shell-side now); unfocused fields render with the caret parked at end.
        use crate::overlay_input::OverlayField;
        let field_cursor = |want: OverlayField, len: usize| {
            self.overlay_edit
                .as_ref()
                .filter(|e| e.field == want)
                .map(|e| e.input.cursor)
                .unwrap_or(len)
        };
        let mut name_input = crate::text_input::TextInput::default();
        name_input.set(core.name.text.clone());
        name_input.cursor = field_cursor(OverlayField::WorkspaceName, core.name.text.len());
        let mut add_input = crate::text_input::TextInput::default();
        add_input.set(core.add.text.clone());
        add_input.cursor = field_cursor(OverlayField::WorkspaceAddRoot, core.add.text.len());
        // The add-project row is a path editor: project its two segments (plus the ghosts and
        // validity the core computes) the way the save prompt does.
        let labels = aether_client::labels::root_labels(&self.session.workspace_paths);
        let multi_root = self.session.workspace_paths.len() > 1;
        let ed = &core.add_project;
        let mut add_project_input = crate::text_input::TextInput::default();
        add_project_input.set(ed.input.text.clone());
        add_project_input.cursor =
            field_cursor(OverlayField::WorkspaceAddProject, ed.input.text.len());
        let mut add_project_root_input = crate::text_input::TextInput::default();
        add_project_root_input.set(ed.root_filter.text.clone());
        add_project_root_input.cursor = field_cursor(
            OverlayField::WorkspaceAddProjectRoot,
            ed.root_filter.text.len(),
        );
        let add_project = crate::app::ProjectEditorState {
            root_input: add_project_root_input,
            root_display: labels
                .get(ed.chosen_root(&labels) as usize)
                .cloned()
                .unwrap_or_default(),
            root_ghost: multi_root
                .then(|| ed.root_ghost(&labels).map(|(_, s)| s))
                .flatten(),
            root_invalid: multi_root && ed.root_invalid(&labels),
            on_root: multi_root && ed.field == aether_client::chips::ChipEditorField::Root,
            focused: core.row() == SettingsRow::AddProject,
            multi_root,
            path_input: add_project_input,
            path_ghost: ed.path_ghost(),
            path_invalid: ed.path_invalid(),
            language_input: {
                let mut t = crate::text_input::TextInput::default();
                t.set(core.add_project_language.text.clone());
                t.cursor = field_cursor(
                    OverlayField::WorkspaceAddProjectLanguage,
                    core.add_project_language.text.len(),
                );
                t
            },
            language_ghost: core.language_ghost(),
            language_invalid: core.language_invalid(),
            on_language: core.on_add_project_language,
        };

        // Flatten the core's row model into display order, carrying each row's selection index so
        // the renderer never re-derives it. Index 0 (the name field) lives in the header, not here.
        use crate::app::{SettingsListRow, SettingsRowView};
        use aether_client::session::SettingsRow;
        let mut rows: Vec<SettingsListRow> = Vec::new();
        for i in 1..core.row_count() {
            let view = match core.row_at(i) {
                SettingsRow::Name => continue,
                SettingsRow::Root(r) => match core.roots.get(r) {
                    Some(path) => SettingsRowView::Root(path.clone()),
                    None => continue,
                },
                SettingsRow::AddRoot => SettingsRowView::AddRoot,
                SettingsRow::Project(p) => match core.projects.get(p) {
                    Some(project) => SettingsRowView::Project {
                        // The canonical `[root]: [path]` buffer-location format, so a project row
                        // reads like every other file reference in the editor.
                        path: aether_client::labels::root_relative_display(
                            &self.session.workspace_paths,
                            project.path_index,
                            &project.relative_path,
                        ),
                        language: project.language.clone(),
                        error: project.error.clone(),
                    },
                    None => continue,
                },
                SettingsRow::AddProject => SettingsRowView::AddProject,
            };
            // The heading goes above the projects list — before the first project, or before the
            // add-project row when nothing is declared yet.
            let starts_projects =
                matches!(
                    core.row_at(i),
                    SettingsRow::Project(0) | SettingsRow::AddProject
                ) && !matches!(core.row_at(i.saturating_sub(1)), SettingsRow::Project(_));
            if starts_projects {
                // A blank line so the two sections read as separate groups, then the heading —
                // styled like the header's `Workspace roots:` label.
                rows.push(SettingsListRow {
                    select: None,
                    view: SettingsRowView::Blank,
                });
                rows.push(SettingsListRow {
                    select: None,
                    view: SettingsRowView::Section("Projects:"),
                });
            }
            rows.push(SettingsListRow {
                select: Some(i),
                view,
            });
        }

        self.state.workspace_settings = Some(crate::app::WorkspaceSettingsState {
            name_input,
            rows,
            selected: core.selected,
            add_input,
            add_project,
            error: core.error.clone(),
        });
    }

    /// Build the reading-view render model (docs/markdown-view.md §2.8): lay the document out at
    /// the current width (cached by `(buffer, revision, cols)`), derive the focused element from
    /// the server cursor, reveal it when the focus changed, and clamp the scroll.
    fn read_view(&mut self) -> Option<crate::app::ReadViewState> {
        let read = self.session.read.as_ref()?;
        // First fetch still in flight: show the loading page WITHOUT touching the layout cache —
        // the adopt often lands at the same revision (an unedited buffer stays at rev 0), so a
        // cached empty layout would otherwise mask the parsed document.
        if read.loading && read.blocks.is_empty() {
            return Some(crate::app::ReadViewState {
                rows: std::sync::Arc::new(Vec::new()),
                bar_rows: None,
                target_focus: None,
                scroll: 0,
                hscroll: std::collections::HashMap::new(),
                loading: true,
            });
        }
        let (area_cols, _) = self.term;
        let (content_cols, _margin) = aether_client::read_layout::measure(area_cols.max(10));
        let key = (read.buffer_id, read.revision, read.hl_gen, content_cols);
        let stale = self
            .read_cache
            .as_ref()
            .map(|c| (c.0, c.1, c.2, c.3) != key)
            .unwrap_or(true);
        if stale {
            // A buffer switch resets the scroll; a re-parse of the same buffer keeps it.
            if self.read_cache.as_ref().map(|c| c.0) != Some(read.buffer_id) {
                self.read_scroll = 0;
                self.read_last_focus = None;
            }
            // Horizontal offsets are keyed by element index, which shifts with the parse —
            // drop them on content change (highlight arrivals and width changes keep them).
            if self.read_cache.as_ref().map(|c| (c.0, c.1)) != Some((read.buffer_id, read.revision))
            {
                self.read_hscroll.clear();
            }
            let rows = aether_client::read_layout::layout(
                &read.blocks,
                &read.elements,
                content_cols,
                &read.code_highlights,
            );
            self.read_cache = Some((key.0, key.1, key.2, key.3, std::sync::Arc::new(rows)));
        }
        let rows = self.read_cache.as_ref().expect("just filled").4.clone();
        // Two projections of the one server cursor (docs/markdown-view.md §1.3): the block bar
        // always marks the reading position; the interactive target inverts on top of it.
        let cursor = self.session.buffer.cursor.position;
        let block_focus = read.block_focus(cursor);
        let target_focus = read.target_focus(cursor);
        // The bar spans the focused block's whole *subtree* — first..=last row whose element's
        // span nests inside the focused span, gaps (separator blanks) riding along — so a
        // parent item's bar covers its nested items, exactly like the GUI/web item wrappers.
        let bar_rows = block_focus.and_then(|f| {
            let fs = read.elements[f].span();
            let mut range: Option<(usize, usize)> = None;
            for (i, row) in rows.iter().enumerate() {
                let inside = row.element.is_some_and(|e| {
                    let s = read.elements[e].span();
                    s.start >= fs.start && s.end <= fs.end
                });
                if inside {
                    range = Some(match range {
                        Some((a, _)) => (a, i),
                        None => (i, i),
                    });
                }
            }
            range
        });
        let loading = read.loading;
        let visible = self.visible_rows().max(1) as u16;
        // Reveal on focus *changes* only, so manual scrolling isn't fought — keyed to the
        // target when the cursor is inside one (a Tab step must reveal the link, not just its
        // paragraph). Coordinates are padded rows (READ_PAD_TOP blanks precede the document).
        let reveal = target_focus.or(block_focus);
        if reveal != self.read_last_focus {
            self.read_last_focus = reveal;
            if let Some(row) =
                reveal.and_then(|f| aether_client::read_layout::first_row_of_element(&rows, f))
            {
                let padded = row as u16 + crate::ui::READ_PAD_TOP;
                if padded < self.read_scroll || padded >= self.read_scroll.saturating_add(visible) {
                    // Rest the element ~20% down, matching the editor's jump reveals.
                    self.read_scroll = padded.saturating_sub(visible / 5);
                }
            }
        }
        let total = rows.len() as u16 + crate::ui::READ_PAD_TOP + crate::ui::READ_PAD_BOTTOM;
        let max_scroll = total.saturating_sub(visible);
        self.read_scroll = self.read_scroll.min(max_scroll);
        Some(crate::app::ReadViewState {
            rows,
            bar_rows,
            target_focus,
            scroll: self.read_scroll,
            hscroll: self.read_hscroll.clone(),
            loading,
        })
    }

    /// Scroll the reading view by `delta` rows (keys and wheel; clamped in `read_view`).
    fn read_scroll_by(&mut self, delta: i32) {
        let cur = self.read_scroll as i32;
        self.read_scroll = cur.saturating_add(delta).max(0) as u16;
    }

    /// Pan `element`'s horizontal scroll by `delta` columns (docs/markdown-view.md §2.8):
    /// clamped so the block's widest row just reaches the window's right edge, and a no-op on
    /// anything without overflowing rows — only unchunked code rows can overflow, so panning a
    /// paragraph clamps straight to zero.
    fn read_hscroll_by(&mut self, element: Option<usize>, delta: i32) {
        let Some(e) = element else { return };
        let Some(rows) = self.read_cache.as_ref().map(|c| c.4.clone()) else {
            return;
        };
        let (content_cols, _) = aether_client::read_layout::measure(self.term.0.max(10));
        // The painter's code window: the measure minus the 2-column gutter and the two
        // pad/indicator columns (rows with extra prefixes — quoted code — over-clamp a bit,
        // which only allows a few surplus columns of scroll).
        let window = content_cols.saturating_sub(4) as usize;
        let widest = aether_client::read_layout::hscroll_content_width(&rows, e);
        let max_off = widest.saturating_sub(window) as i32;
        let cur = self.read_hscroll.get(&e).copied().unwrap_or(0) as i32;
        let next = cur.saturating_add(delta).clamp(0, max_off.max(0)) as u16;
        if next == 0 {
            self.read_hscroll.remove(&e);
        } else {
            self.read_hscroll.insert(e, next);
        }
    }

    /// The block-grain element on the layout row under screen row `row`, for wheel-driven
    /// panning of the code block under the pointer.
    fn read_element_at_row(&self, row: u16) -> Option<usize> {
        if u32::from(row) >= self.visible_rows() {
            return None; // the status row (or below)
        }
        let rows = &self.read_cache.as_ref()?.4;
        (self.read_scroll as usize + row as usize)
            .checked_sub(crate::ui::READ_PAD_TOP as usize)
            .and_then(|i| rows.get(i))
            .and_then(|r| r.element)
    }

    /// Hit-test a reading-view click to a source byte: the layout row under the pointer gives
    /// the block-grain element; a span carrying its own (interactive) element — a link — wins
    /// when the column lands inside it. Mirrors the geometry `draw_read_view` paints: the
    /// centered measure plus the 2-column focus gutter.
    /// Hit-test a click to `(span start byte, landed on an interactive span)` — the flag
    /// selects click-follow (`read_click_activate`) over plain focusing.
    fn read_hit(&self, row: u16, col: u16) -> Option<(u32, bool)> {
        use unicode_width::UnicodeWidthStr;
        let read = self.session.read.as_ref()?;
        let rows = &self.read_cache.as_ref()?.4;
        if u32::from(row) >= self.visible_rows() {
            return None; // the status row (or below)
        }
        let r = (self.read_scroll as usize + row as usize)
            .checked_sub(crate::ui::READ_PAD_TOP as usize)
            .and_then(|i| rows.get(i))?;
        let (_, margin) = aether_client::read_layout::measure(self.term.0.max(10));
        let mut element = r.element;
        let mut interactive = false;
        let col_in_content = i32::from(col) - i32::from(margin) - 2;
        if col_in_content >= 0 {
            let mut acc = 0i32;
            for sp in &r.spans {
                let w = sp.text.width() as i32;
                if col_in_content < acc + w {
                    if sp.element.is_some() {
                        element = sp.element;
                        interactive = true;
                    }
                    break;
                }
                acc += w;
            }
        }
        Some((read.elements.get(element?)?.span().start, interactive))
    }

    fn editor_view(&self) -> EditorState {
        let s = &self.session;
        let window = s.window.as_ref();
        // A buffer switch clears `session.window` until the fresh subscribe lands; keep
        // showing the previous window meanwhile (web parity) — a blank frame between
        // buffers reads as a flash, worst on same-file grep `<`/`>` jumps where it's
        // conceptually just a cursor move.
        let prev = self.state.editor.as_ref().filter(|_| window.is_none());
        let (scroll_logical_line, scroll_skip_rows) = match (window, prev) {
            (Some(w), _) => line_at_row(w, self.top_visual_row),
            (None, Some(p)) => (p.scroll_logical_line, p.scroll_skip_rows),
            (None, None) => (0, 0),
        };
        EditorState {
            mode: match s.mode {
                Mode::Normal => EditorMode::Normal,
                Mode::Insert => EditorMode::Insert,
                Mode::Search => EditorMode::Search,
                // Interim until the reading-view renderer lands: Read draws like Normal (block
                // cursor over the editor window).
                Mode::Read => EditorMode::Normal,
            },
            buffer_id: s.buffer.buffer_id,
            viewport_id: s.viewport_id.unwrap_or(0),
            cursor: s.buffer.cursor,
            scroll_logical_line,
            scroll_skip_rows,
            window_first_logical_line: window
                .map(|w| w.first_logical_line)
                .or(prev.map(|p| p.window_first_logical_line))
                .unwrap_or(0),
            lines: window
                .map(|w| w.lines.clone())
                .or_else(|| prev.map(|p| p.lines.clone()))
                .unwrap_or_default(),
            line_count: window
                .map(|w| w.line_count)
                .or(prev.map(|p| p.line_count))
                .unwrap_or(0),
            git_status: window
                .and_then(|w| w.git_status.clone())
                .or_else(|| prev.and_then(|p| p.git_status.clone())),
            max_scroll_logical_line: window
                .map(|w| w.max_scroll_logical_line)
                .or(prev.map(|p| p.max_scroll_logical_line))
                .unwrap_or(0),
            total_visual_rows: window
                .map(|w| w.total_visual_rows)
                .or(prev.map(|p| p.total_visual_rows))
                .unwrap_or(0),
            // `top_visual_row` is absolute (whole-buffer) already; while a switch is in flight
            // (no window) keep the previous frame's value so the thumb doesn't jump.
            top_visual_row: window
                .map(|_| self.top_visual_row)
                .or(prev.map(|p| p.top_visual_row))
                .unwrap_or(0),
            wrap: s.wrap,
            diff_view: s.diff_view,
            scroll_col: self.scroll_col,
            pending_scroll_lines: 0,
            drag_anchor: None,
            drag_granularity: aether_protocol::cursor::Granularity::Char,
            last_click: None,
            click_streak: 0,
            revision: s.buffer.revision,
            saved_revision: s.buffer.saved_revision,
            externally_modified: s.externally_modified,
            externally_deleted: s.externally_deleted,
            pending_count: s.count.unwrap_or(0),
            pending_find: match s.pending {
                Pending::Find {
                    dir,
                    till,
                    extend,
                    count,
                } => Some(crate::app::PendingFind {
                    direction: dir,
                    till,
                    extend,
                    count,
                }),
                _ => None,
            },
            pending_surround: match s.pending {
                Pending::Surround(t) => Some(t),
                _ => None,
            },
            sneak_active: s.sneak.is_some(),
            search: self.search_view(),
            blame: BlameState {
                line: s.blame.as_ref().map(|(l, _)| *l),
                text: s.blame.as_ref().map(|(_, t)| t.clone()),
            },
            transient: s.buffer.transient,
            tethered: s.tethered(),
            file_path: s.buffer.path.clone(),
            file_label: s.buffer.label.clone(),
            language: s.buffer.language.clone(),
            lsp_server: s.buffer.lsp_server.clone(),
        }
    }

    fn search_view(&self) -> TuiSearchState {
        let s = &self.session.search;
        let mut query = crate::text_input::TextInput::default();
        query.set(s.query.clone());
        // The caret lives in the shell-owned overlay editor (text mechanics are shell-side now);
        // fall back to end-of-text if it hasn't been seeded yet.
        query.cursor = self
            .overlay_edit
            .as_ref()
            .filter(|e| e.field == crate::overlay_input::OverlayField::Search)
            .map(|e| e.input.cursor)
            .unwrap_or(s.query.len());
        TuiSearchState {
            query,
            active: s.active,
            summary: s.summary.clone(),
            snapshot: None,
            extend_to_cursor: s.extend_to_cursor,
            option_chips: s
                .option_chips()
                .iter()
                .map(|c| {
                    (
                        c.label.clone(),
                        matches!(c.id, aether_client::chips::ChipId::Word),
                    )
                })
                .collect(),
            chip_selected: s.chip_selected,
        }
    }

    fn sync_picker(&mut self) {
        let pane_rows =
            crate::ui::picker_result_rows(self.state.viewport_cols, self.state.viewport_rows);
        // The query / chip-editor carets live in the shell-owned overlay editor (text mechanics are
        // shell-side now); fall back to end-of-text when the field isn't the focused one.
        use crate::overlay_input::OverlayField;
        let overlay_cursor = |want: OverlayField| {
            self.overlay_edit
                .as_ref()
                .filter(|e| e.field == want)
                .map(|e| e.input.cursor)
        };
        let query_cursor = overlay_cursor(OverlayField::PickerQuery);
        let chip_root_cursor = overlay_cursor(OverlayField::ChipRoot);
        let chip_path_cursor = overlay_cursor(OverlayField::ChipPath);
        let p = &mut self.state.picker;
        let Some(core) = &self.session.picker else {
            p.open = false;
            return;
        };
        p.open = true;
        p.pane_rows = pane_rows;
        p.kind = Some(core.kind);
        p.query.set(core.query.clone());
        p.query.cursor = query_cursor.unwrap_or(core.query.len());
        p.generation = core.generation;
        p.offset = core.offset;
        p.items = core.items.clone();
        p.groups = core.groups.clone();
        p.total_matches = core.total_matches;
        p.total_candidates = core.total_candidates;
        p.ticking = core.ticking;
        p.spinner = core.spinner_glyph();
        p.total_display_rows = Some(core.total_display_rows);
        p.empty_note = core.empty_note().map(str::to_string);
        p.selected = (core.selected.saturating_sub(core.offset)) as usize;
        // The Explorer's synthetic "+ Create …" affordance — the core owns the decision
        // (`pending_create`); the shell appends it as a trailing row (italicised via
        // `synthetic_create_idx`) once the fetched window reaches the list's end, mirroring the
        // core's `display_rows`. Purely visual: Enter routes through the core's `picker_accept`,
        // which sees the create row on the *core* selection and creates the file/dir.
        p.synthetic_create_idx = None;
        if let Some(pc) = core.pending_create() {
            if core.offset + core.items.len() as u32 >= core.total_matches {
                use aether_protocol::picker::PickerItem;
                let item = if core.kind == aether_protocol::picker::PickerKind::Workspaces {
                    // Workspaces rows carry no leading status-dot cell, so the create row mustn't
                    // either — render it as a Workspace, not a DirEntry (which reserves that column
                    // and would indent it past the real workspace rows).
                    PickerItem::Workspace {
                        name: format!("+ Create workspace {}", pc.name),
                        unsaved_buffers: 0,
                        match_indices: Vec::new(),
                    }
                } else {
                    let label = if pc.is_dir {
                        format!("+ Create directory {}/", pc.name)
                    } else {
                        format!("+ Create file {}", pc.name)
                    };
                    PickerItem::DirEntry {
                        name: label,
                        is_dir: false,
                        match_indices: Vec::new(),
                        git_status: None,
                    }
                };
                p.items.push(item);
                p.synthetic_create_idx = Some(p.items.len() - 1);
            }
        }
        p.chips = core.chips.iter().map(chip_value_view).collect();
        p.chip_selected = core.chip_selected;
        p.chip_editor = core
            .chip_editor
            .as_ref()
            .map(|e| chip_editor_view(e, chip_root_cursor, chip_path_cursor));
        p.explorer_dir = core.directory.clone();
        p.completion = core.explorer_completion();
        p.explorer_parent = core.directory_parent.clone();
        // Keep the highlight on-screen within the fetched slice (the shell half of
        // RevealPickerSelection). Scroll is a *view row* over the window's header / gap / item
        // rows, so grep / keybindings advance one screen row at a time even across a group
        // boundary — the item-index math jumped a header + gap there. `picker_scroll_step` also
        // carries the on-screen position across a recentering refetch (and preserves it through
        // the empty-items frames a fast scroll produces, rather than snapping to the top).
        let rows = crate::ui::picker_window_rows(p.items.len(), &p.groups);
        let pin = crate::ui::pins_group_header(core.kind) && !p.groups.is_empty();
        self.picker_scroll = crate::ui::picker_scroll_step(
            &rows,
            p.selected,
            p.pane_rows.max(1) as usize,
            pin,
            core.offset,
            self.picker_scroll,
        );
        p.visible_start = self.picker_scroll.top;
    }

    fn sync_prompts(&mut self) {
        // `sync` has already reconciled the overlay editor with the focused field; read the live
        // save-as caret from it (the value mechanics are shell-side now). Only one of the root /
        // path segments is focused at a time, so at most one of these is `Some`.
        let save_path_cursor = self
            .overlay_edit
            .as_ref()
            .filter(|e| e.field == crate::overlay_input::OverlayField::SaveAs)
            .map(|e| e.input.cursor);
        let save_root_cursor = self
            .overlay_edit
            .as_ref()
            .filter(|e| e.field == crate::overlay_input::OverlayField::SaveAsRoot)
            .map(|e| e.input.cursor);
        let open_path_cursor = self
            .overlay_edit
            .as_ref()
            .filter(|e| e.field == crate::overlay_input::OverlayField::OpenPath)
            .map(|e| e.input.cursor);
        let multi_root = self.session.workspace_paths.len() > 1;
        let st = &mut self.state;
        st.confirm_prompt = None;
        st.save_prompt = None;
        st.open_path_prompt = None;
        st.picker.lsp_detail = None;
        // Taken rather than cleared: the app-info dialog's scroll position is shell-owned and must
        // survive a re-sync, or a wheel scroll would snap back to the top on the next event.
        let prev_app_info = st.app_info.take();
        match &self.session.prompt {
            Some(Prompt::Confirm { kind, .. }) => {
                st.confirm_prompt = Some(crate::app::ConfirmPrompt {
                    // The core states the reason; the TUI composes the phrasing (the status row
                    // then appends `? [y/N]`). The action runs in the core, so the variant here is
                    // a placeholder the new shell never executes.
                    message: confirm_phrase(kind),
                    action: crate::app::ConfirmAction::OverwriteSaveAs,
                });
            }
            Some(Prompt::SaveAs(ed)) => {
                st.save_prompt = Some(save_as_view(
                    ed,
                    multi_root,
                    save_root_cursor,
                    save_path_cursor,
                ));
            }
            Some(Prompt::OpenPath(field)) => {
                let mut input = crate::text_input::TextInput::default();
                input.set(field.text.clone());
                input.cursor = open_path_cursor.unwrap_or(field.text.len());
                st.open_path_prompt = Some(input);
            }
            Some(Prompt::AppInfo(info)) => {
                // Rows come from the core so all three shells agree; the TUI adds only scroll.
                // Recomposed each sync — cheap — but the scroll carries over from the previous
                // sync so wheel position sticks while the dialog stays open.
                st.app_info = Some(crate::app::AppInfoView {
                    sections: aether_client::app_info::sections(info),
                    scroll: prev_app_info.map(|v| v.scroll).unwrap_or_default(),
                });
            }
            Some(Prompt::LspInfo(status)) => {
                // The dedicated detail pane the LSP-servers picker renders. Scroll is
                // re-derived each sync (a fresh ScrollState clamps to the top); panning
                // joins the follow-up pass with the hover popup's.
                st.picker.lsp_detail = Some(crate::picker::LspServerDetail {
                    name: status.name.clone(),
                    language: status.language.clone(),
                    workspace_root: status.workspace_root.clone(),
                    status: status.status.clone(),
                    progress: status.progress.clone(),
                    scroll: Default::default(),
                });
            }
            None => {}
        }
    }
}

/// Compose the status-row phrasing for a confirmation. The core supplies the structured reason
/// ([`ConfirmKind`]); this is the TUI's presentational choice. `draw_status` then appends `? [y/N]`.
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

// ---- chip view conversion (core chips -> the render model's types) --------------------------

fn chip_value_view(v: &aether_client::chips::ChipValue) -> crate::picker::ChipValue {
    use crate::picker::ChipValue as T;
    use aether_client::chips::ChipValue as C;
    match v {
        C::Dir(d) => T::Dir(d.clone()),
        C::Glob(g) => T::Glob(g.clone()),
        C::Case(m) => T::Case(*m),
        C::Word => T::Word,
        C::Regex => T::Regex,
        C::Ignored { hide } => T::Ignored { hide: *hide },
        C::Hidden { hide } => T::Hidden { hide: *hide },
        C::Changed => T::Changed,
        C::Untracked => T::Untracked,
    }
}

/// Workspace the core's chip editor into the TUI view model. `root_cursor` / `path_cursor` carry the
/// shell-owned caret for whichever segment is focused (`None` → render that field's caret at end,
/// the unfocused convention).
fn chip_editor_view(
    e: &aether_client::chips::ChipEditor,
    root_cursor: Option<usize>,
    path_cursor: Option<usize>,
) -> crate::picker::ChipEditor {
    use crate::picker as t;
    use aether_client::chips as c;
    let input = |i: &c::Input, cursor: Option<usize>| {
        let mut x = crate::text_input::TextInput::default();
        x.set(i.text.clone());
        x.cursor = cursor.unwrap_or(i.text.len());
        x
    };
    t::ChipEditor {
        kind: match e.kind {
            c::ChipEditorKind::Glob { edit } => t::ChipEditorKind::Glob { edit },
            c::ChipEditorKind::Dir { edit } => t::ChipEditorKind::Dir { edit },
        },
        tag: e.field_tag(),
        field: match e.field {
            c::ChipEditorField::Root => t::ChipEditorField::Root,
            c::ChipEditorField::Path => t::ChipEditorField::Path,
        },
        input: input(&e.input, path_cursor),
        root_filter: input(&e.root_filter, root_cursor),
        root_selected: e.root_selected,
        root_index: e.root_index,
        listing: e.listing.clone(),
        listing_dir_abs: e.listing_dir_abs.clone(),
        listing_state: match e.listing_state {
            c::DirListingState::Pending => t::DirListingState::Pending,
            c::DirListingState::Loaded => t::DirListingState::Loaded,
            c::DirListingState::Failed => t::DirListingState::Failed,
        },
        suggestion_idx: e.suggestion_idx,
    }
}

/// Workspace the core's save-as editor into the TUI view model — the save-as counterpart of
/// [`chip_editor_view`]. `root_cursor` / `path_cursor` carry the shell-owned caret for whichever
/// segment is focused (`None` → that field's caret at end, the unfocused convention).
fn save_as_view(
    e: &aether_client::path_editor::PathEditor,
    multi_root: bool,
    root_cursor: Option<usize>,
    path_cursor: Option<usize>,
) -> crate::save_prompt::SavePromptState {
    use crate::picker as t;
    use aether_client::chips as c;
    let input = |i: &c::Input, cursor: Option<usize>| {
        let mut x = crate::text_input::TextInput::default();
        x.set(i.text.clone());
        x.cursor = cursor.unwrap_or(i.text.len());
        x
    };
    crate::save_prompt::SavePromptState {
        field: match e.field {
            c::ChipEditorField::Root => t::ChipEditorField::Root,
            c::ChipEditorField::Path => t::ChipEditorField::Path,
        },
        input: input(&e.input, path_cursor),
        root_filter: input(&e.root_filter, root_cursor),
        root_selected: e.root_selected,
        root_index: e.root_index,
        multi_root,
        listing: e.listing.clone(),
        listing_dir_abs: e.listing_dir_abs.clone(),
        listing_state: match e.listing_state {
            c::DirListingState::Pending => t::DirListingState::Pending,
            c::DirListingState::Loaded => t::DirListingState::Loaded,
            c::DirListingState::Failed => t::DirListingState::Failed,
        },
        suggestion_idx: e.suggestion_idx,
    }
}

// ---- helpers -------------------------------------------------------------------------------

/// Visual rows of every loaded line — phantom deleted rows (inline diff view) included,
/// via the core's grid math (the same fns the iced shell scrolls with).
fn loaded_rows(window: &Window) -> u32 {
    window
        .lines
        .iter()
        .map(aether_client::grid::line_rows)
        .sum()
}

/// Cumulative visual rows before `line` within the loaded window — absolute row when added
/// to `first_visual_row`. Phantom rows included; `None` when the line isn't loaded.
fn rows_before_line(window: &Window, line: u32) -> Option<u32> {
    aether_client::grid::rows_before_line(window, line)
}

/// The buffer-absolute visual row of the cursor's cell — past any phantom rows, since the
/// cursor never lands on them.
fn cursor_visual_row(window: &Window, pos: aether_protocol::LogicalPosition) -> Option<u32> {
    aether_client::grid::position_cell(window, pos, TAB_WIDTH).map(|(row, _, _)| row)
}

/// Resolve a buffer-absolute visual row to `(logical_line, rows_hidden_above)` for the
/// renderer's `scroll_logical_line`/`scroll_skip_rows` pair. Line heights count the whole
/// block (phantom rows + wrapped content rows), matching how the renderer skips.
fn line_at_row(window: &Window, row: u32) -> (u32, u32) {
    let mut rel = row.saturating_sub(window.first_visual_row);
    for l in &window.lines {
        let h = aether_client::grid::line_rows(l);
        if rel < h {
            return (l.logical_line, rel);
        }
        rel -= h;
    }
    (window.last_logical_line_exclusive.saturating_sub(1), 0)
}

/// crossterm key event → the core's `(KeyCode, Mods, typed text)`.
fn translate_key(k: &KeyEvent) -> Option<(KeyCode, Mods, Option<String>)> {
    use crossterm::event::KeyCode as CK;
    use crossterm::event::KeyModifiers as KM;
    let mods = Mods {
        ctrl: k.modifiers.contains(KM::CONTROL),
        alt: k.modifiers.contains(KM::ALT),
        shift: k.modifiers.contains(KM::SHIFT),
    };
    let code = match k.code {
        CK::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        CK::Esc => KeyCode::Esc,
        CK::Enter => KeyCode::Enter,
        CK::Tab => KeyCode::Tab,
        CK::BackTab => KeyCode::BackTab,
        CK::Backspace => KeyCode::Backspace,
        CK::Delete => KeyCode::Delete,
        CK::Home => KeyCode::Home,
        CK::End => KeyCode::End,
        CK::PageUp => KeyCode::PageUp,
        CK::PageDown => KeyCode::PageDown,
        CK::Left => KeyCode::Left,
        CK::Right => KeyCode::Right,
        CK::Up => KeyCode::Up,
        CK::Down => KeyCode::Down,
        _ => return None,
    };
    let text = match k.code {
        CK::Char(c) if !mods.ctrl && !mods.alt => Some(c.to_string()),
        _ => None,
    };
    Some((code, mods, text))
}

/// One boot dial: after the first attempt back off, dial the fixed address, then run the same
/// `bootstrap` the synchronous boot used (activate the CLI workspace + open the file/MRU buffer, or
/// hand back a placeholder + chooser startup). `NotUp` (server down) retries; a bootstrap refusal
/// is `Fatal`.
async fn boot_dial(
    attempt: u32,
    spec: BootSpec,
    cols: u16,
    rows: u16,
    server_url: String,
) -> Result<Booted, ReconnectError> {
    if attempt > 0 {
        // Boot pacing, not the reconnect curve: we're usually racing the daemon this very
        // process just spawned, so poll fast at first (see `boot_backoff`).
        tokio::time::sleep(boot_backoff(attempt)).await;
    }
    let (handle, notifications) = crate::connection::connect(&server_url, &spec.version)
        .await
        .map_err(ReconnectError::from)?;
    let (session, state, startup) = bootstrap(
        &handle,
        spec.workspace.as_deref(),
        spec.file.as_deref(),
        spec.jump,
        spec.tether,
        cols,
        rows,
    )
    .await
    .map_err(|e| ReconnectError::Fatal(e.to_string()))?;
    Ok(Booted {
        handle,
        notifications,
        session,
        state,
        startup,
    })
}

/// One paced reconnect attempt: back off, dial the server, restore.
#[allow(clippy::too_many_arguments)]
async fn dial(
    attempt: u32,
    workspace: String,
    path: Option<String>,
    buffer_id: u64,
    transient: bool,
    cursor: aether_protocol::LogicalPosition,
    version: String,
    server_url: String,
) -> Result<Reestablished, ReconnectError> {
    use aether_protocol::buffer::{BufferOpen, BufferOpenParams};
    use aether_protocol::workspace::{WorkspaceActivate, WorkspaceActivateParams};

    tokio::time::sleep(reconnect_backoff(attempt)).await;
    let (handle, notifications) = crate::connection::connect(&server_url, &version)
        .await
        .map_err(ReconnectError::from)?;
    // The connection died while the chooser was up (a placeholder session — its workspace name
    // is empty, and no real one can be: create validates non-empty). Nothing to restore; hand
    // back a workspace-less reconnect and the shell re-raises the chooser.
    if workspace.is_empty() {
        return Ok(Reestablished {
            handle,
            notifications,
            restore: None,
            restarted: false,
        });
    }
    let activated = match handle
        .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
            name: workspace,
            open_last: false,
        })
        .await
    {
        Ok(a) => a,
        // Couldn't re-activate the workspace — it was renamed or removed while we were away. The
        // socket is up, so hand back a workspace-less reconnect and let the shell raise the chooser.
        Err(_) => {
            return Ok(Reestablished {
                handle,
                notifications,
                restore: None,
                restarted: false,
            });
        }
    };
    let params = match &path {
        Some(p) => aether_client::session::strip_longest_root(p, &activated.workspace.paths).map(
            |(path_index, relative_path)| BufferOpenParams {
                path_index: Some(path_index),
                relative_path: Some(relative_path),
                transient: transient.then_some(true),
                jump_to: Some(cursor),
                ..Default::default()
            },
        ),
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
        None => handle
            .rpc::<BufferOpen>(BufferOpenParams {
                transient: Some(true),
                ..Default::default()
            })
            .await
            .map_err(|e| ReconnectError::Fatal(e.to_string()))?,
    };
    Ok(Reestablished {
        handle,
        notifications,
        restore: Some((activated.workspace, open)),
        restarted: false,
    })
}

/// Coarse relative age for the blame line ("3w ago").
fn time_ago(unix_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - unix_secs).max(0);
    let (n, unit) = if secs < 60 {
        return "just now".into();
    } else if secs < 3600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3600, "h")
    } else if secs < 604_800 {
        (secs / 86_400, "d")
    } else if secs < 31_536_000 {
        (secs / 604_800, "w")
    } else {
        (secs / 31_536_000, "y")
    };
    format!("{n}{unit} ago")
}

/// Bootstrap a session over an established connection: activate (landing on the last
/// buffer or a named file) and build the core `Session` + the render model. `tether` marks the
/// quick-edit invocation (a file positional, no explicit `--workspace`): the buffer `file` opens
/// becomes the session's [tether](Session::tether), so closing it exits the client.
pub async fn bootstrap(
    handle: &Handle,
    workspace: Option<&str>,
    file: Option<&str>,
    jump: Option<(u32, u32)>,
    tether: bool,
    cols: u16,
    rows: u16,
) -> Result<(Session, AppState, Effects)> {
    use aether_protocol::buffer::{BufferOpen, BufferOpenParams};
    use aether_protocol::LogicalPosition;
    let jump_to = jump.map(|(line, col)| LogicalPosition { line, col });
    use aether_protocol::picker::PickerKind;
    use aether_protocol::workspace::{
        WorkspaceActivate, WorkspaceActivateParams, WorkspaceOpenPath, WorkspaceOpenPathParams,
    };

    // Workspace selection is explicit. When none is named on the command line and no file is given
    // we DON'T activate one — we start with a placeholder session (no workspace, no buffer) and
    // raise the Workspaces chooser. Nothing is rendered behind it; picking a workspace activates it
    // and lands the first buffer (`PickerSelected` → `WorkspaceActivated` → `adopt_switch`), which
    // is when the editor first appears. When a file *is* given but no workspace (a path outside any
    // configured workspace, e.g. `ae /etc/hosts`), we open it directly via `workspace/open_path`,
    // which lands it in a fresh ephemeral "(no workspace)" context.
    let (mut session, workspace_name, workspace_paths, startup) = match workspace {
        None => {
            let resolved = match file {
                Some(f) => Some(crate::app::resolve_cli_path(f)?),
                None => None,
            };
            match resolved {
                // An external file: open it in an ephemeral context. A missing path counts as a
                // file to create (`create_if_missing` binds an empty buffer, written at the first
                // save — explorer-create semantics). Only a directory falls through to the
                // chooser (nothing sensible to open).
                Some(abs) if !abs.is_dir() => {
                    let opened = handle
                        .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                            path: abs.display().to_string(),
                            transient: None,
                            create_if_missing: true,
                        })
                        .await?;
                    let workspace_paths = opened.workspace.paths.clone();
                    let open = opened
                        .opened
                        .ok_or_else(|| anyhow::anyhow!("workspace/open_path returned no buffer"))?;
                    let buffer = buffer_info(open, &workspace_paths);
                    let workspace_name = opened.workspace.name.clone();
                    let mut session = Session::new(opened.workspace, buffer);
                    // Launched to edit this file: tether the client to it, so closing it quits
                    // (docs/tether.md) rather than dropping to the chooser.
                    if tether {
                        session.tether = Some(session.buffer.buffer_id);
                    }
                    (session, workspace_name, workspace_paths, Effects::none())
                }
                _ => {
                    let mut session = Session::placeholder();
                    let startup =
                        session.open_picker(PickerKind::Workspaces, None, None, false, None);
                    (session, String::new(), Vec::new(), startup)
                }
            }
        }
        Some(workspace) => {
            let activated = handle
                .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
                    name: workspace.to_string(),
                    open_last: file.is_none(),
                })
                .await?;
            let workspace_paths = activated.workspace.paths.clone();

            // Resolve the CLI path once, then branch on file vs directory. A directory lands in a
            // transient scratch and opens the file explorer over it (the `startup` effects below);
            // a file opens normally.
            let resolved = match file {
                Some(f) => Some(crate::app::resolve_cli_path(f)?),
                None => None,
            };

            let open = match &resolved {
                Some(abs) if abs.is_dir() => {
                    handle
                        .rpc::<BufferOpen>(BufferOpenParams {
                            transient: Some(true),
                            ..Default::default()
                        })
                        .await?
                }
                Some(abs) => {
                    let abs = abs.display().to_string();
                    match aether_client::session::strip_longest_root(&abs, &workspace_paths) {
                        // Inside a workspace root: ordinary workspace-relative open. A `path:line:col`
                        // launch jumps to `jump_to` here.
                        Some((path_index, relative_path)) => {
                            handle
                                .rpc::<BufferOpen>(BufferOpenParams {
                                    path_index: Some(path_index),
                                    relative_path: Some(relative_path),
                                    create_if_missing: true,
                                    jump_to,
                                    ..Default::default()
                                })
                                .await?
                        }
                        // Outside the named workspace's roots: open it as an external (guest) buffer
                        // in that workspace rather than refusing the launch.
                        None => handle
                            .rpc::<WorkspaceOpenPath>(WorkspaceOpenPathParams {
                                path: abs,
                                transient: None,
                                create_if_missing: true,
                            })
                            .await?
                            .opened
                            .ok_or_else(|| {
                                anyhow::anyhow!("workspace/open_path returned no buffer")
                            })?,
                    }
                }
                None => activated.opened.ok_or_else(|| {
                    anyhow::anyhow!("workspace/activate returned no landing buffer")
                })?,
            };

            let buffer = buffer_info(open, &workspace_paths);
            let workspace_name = activated.workspace.name.clone();
            let mut session = Session::new(activated.workspace, buffer);
            // A quick-edit launch (`ae file`, workspace *inferred* from the path — `tether` is
            // never set alongside an explicit `--workspace`): tether the client to the opened
            // file, so closing it exits (docs/tether.md). A missing path is a file to create
            // and tethers too; directory args land in the explorer over a scratch — nothing to
            // tether to.
            if tether && resolved.as_ref().is_some_and(|p| !p.is_dir()) {
                session.tether = Some(session.buffer.buffer_id);
            }
            let startup = match &resolved {
                Some(abs) if abs.is_dir() => session.open_picker(
                    PickerKind::Explorer,
                    Some(abs.display().to_string()),
                    None,
                    false,
                    None,
                ),
                _ => Effects::none(),
            };
            (session, workspace_name, workspace_paths, startup)
        }
    };

    // Fetch the persisted app settings (e.g. the soft-wrap default) alongside the boot effects.
    let startup = startup.and(session.startup());
    let state = make_state(
        workspace_name,
        workspace_paths,
        cols,
        rows,
        ConnState::Connected,
    );
    Ok((session, state, startup))
}

/// Build the shell's view-model state. Shared by [`bootstrap`] (a live `Connected` session) and
/// the boot-time connecting screen ([`connecting_state`]), so the two can't drift on the long
/// field list.
fn make_state(
    workspace_name: String,
    workspace_paths: Vec<String>,
    cols: u16,
    rows: u16,
    conn: ConnState,
) -> AppState {
    let root_labels = labels::root_labels(&workspace_paths);
    AppState {
        workspace_name,
        workspace_paths,
        root_labels,
        tether: None,
        viewport_cols: cols as u32,
        viewport_rows: (rows as u32).saturating_sub(1),
        should_quit: false,
        status: StatusMessage::default(),
        toasts: Vec::new(),
        hint: None,
        conn,
        last_terminal_title: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: Default::default(),
        save_prompt: None,
        open_path_prompt: None,
        confirm_prompt: None,
        app_info: None,
        editor: None,
        read: None,
        workspace_settings: None,
        app_settings: None,
        lsp_status: Default::default(),
        hover: None,
        diagnostic_counts: Default::default(),
    }
}

/// The view-model for the boot-time "Connecting…" backdrop: no workspace, no editor, `Connecting`
/// conn state. `ui::draw` renders its no-workspace backdrop with the centered indicator.
pub fn connecting_state(cols: u16, rows: u16) -> AppState {
    make_state(String::new(), Vec::new(), cols, rows, ConnState::Connecting)
}
