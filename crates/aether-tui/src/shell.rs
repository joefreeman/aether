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
use aether_client::effect::{Effect, Effects, ToastKind};
use aether_client::keymap::{
    hover_action, Action, HoverAction, KeyCode, Mods, ScrollDir, ScrollUnit,
};
use aether_client::session::{
    buffer_info, reconnect_backoff, ConnState, HoverText, Mode, Pending, Prompt, Session,
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
    /// Re-enabling the sticky diff view after a fresh subscribe.
    DiffViewSet(Result<aether_protocol::viewport::ViewportWindowResult, String>),
    /// A floating toast's time-to-live elapsed; removes the toast with this id from the stack.
    ToastExpired(u64),
}

/// How long a floating toast (bottom-right) stays before it auto-dismisses — matching the web/native
/// clients' transient toasts.
const TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(4);

struct Reestablished {
    handle: Handle,
    notifications: mpsc::UnboundedReceiver<Notification>,
    project: aether_protocol::project::ProjectInfo,
    open: aether_protocol::buffer::BufferOpenResult,
    restarted: bool,
}

enum ReconnectError {
    /// Server not up yet — retry after backoff.
    NotUp,
    /// Connected but restoring state failed — give up with a message.
    Fatal(String),
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
    reveal_after_fetch: bool,
    /// Like `reveal_after_fetch`, but centres the cursor once its (out-of-window) line lands — for
    /// `-` (center-cursor) when the cursor has been scrolled out of the loaded window.
    center_after_fetch: bool,
    /// Picker results scroll (first visible item index) — the shell half of picker
    /// geometry, reset by `Effect::PickerScrollReset`.
    picker_scroll: usize,
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
    /// Terminal size (cols, rows); the text viewport is `rows - 1` (status row).
    term: (u16, u16),
    /// Monotonic id source for floating toasts — each toast's expiry timer carries its id so the
    /// right one is removed from the stack when it elapses.
    next_toast_id: u64,
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    handle: Handle,
    notifications: mpsc::UnboundedReceiver<Notification>,
    session: Session,
    state: AppState,
    startup: Effects,
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
    let mut shell = Shell {
        handle,
        notifications,
        session,
        state,
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
        reveal_after_fetch: false,
        center_after_fetch: false,
        picker_scroll: 0,
        subscribe_epoch: 0,
        last_click: None,
        click_streak: 0,
        should_quit: false,
        term,
        next_toast_id: 0,
    };

    // First subscribe: the session was bootstrapped with a buffer; show it.
    shell.sent_grid = Some(shell.grid());
    shell.subscribe();
    // Fire any bootstrap effects (e.g. the no-args Projects chooser's `picker/view`).
    shell.run_effects(startup);
    shell.sync();
    crate::app::apply_cursor_style(&shell.state);
    terminal.draw(|f| ui::draw(f, &shell.state))?;
    crate::app::refresh_terminal_title(&mut shell.state);

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
        shell.maybe_blame();
        shell.maybe_fetch();
        shell.sync();
        crate::app::apply_cursor_style(&shell.state);
        terminal.draw(|f| ui::draw(f, &shell.state))?;
        crate::app::refresh_terminal_title(&mut shell.state);
    }
    Ok(())
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
        self.push_toast(msg);
    }

    /// Push a transient toast onto the bottom-right stack and arm its expiry timer. Empty messages
    /// are dropped. The stack is capped so a burst can't grow it without bound (oldest fall off).
    fn push_toast(&mut self, msg: StatusMessage) {
        if msg.is_empty() {
            return;
        }
        const MAX_TOASTS: usize = 5;
        let id = self.next_toast_id;
        self.next_toast_id = self.next_toast_id.wrapping_add(1);
        self.state.toasts.push(crate::app::Toast {
            id,
            text: msg.text,
            kind: msg.kind,
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
                Effect::Toast(text, kind) => self.status(StatusMessage {
                    text,
                    kind: match kind {
                        ToastKind::Info => StatusKind::Info,
                        ToastKind::Success => StatusKind::Success,
                        ToastKind::Warning => StatusKind::Warning,
                        ToastKind::Error => StatusKind::Error,
                    },
                }),
                Effect::WriteClipboard(text) => {
                    let len = text.len();
                    match clipboard::copy(&mut self.state.clipboard, text) {
                        Ok(()) => {
                            self.status(StatusMessage::success(format!("copied {len} bytes")))
                        }
                        Err(e) => self.status(StatusMessage::error(format!("copy failed: {e}"))),
                    }
                }
                Effect::ReadClipboard(kind) => {
                    let text = clipboard::paste(&mut self.state.clipboard).ok();
                    self.dispatch(CoreEvent::ClipboardRead(kind, text));
                }
                Effect::RevealCursor => self.ensure_cursor_visible(),
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
                        self.picker_scroll = 0;
                    }
                }
                Effect::Reconnect { attempt } => self.spawn_reconnect(attempt),
                Effect::Exit => self.should_quit = true,
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
                // Diff view is sticky across switches; a fresh viewport starts with it off.
                if self.session.diff_view {
                    let h = self.handle.clone();
                    let viewport_id = self.session.viewport_id.unwrap_or(0);
                    let fut = async move {
                        h.rpc::<aether_protocol::git::GitSetDiffView>(
                            aether_protocol::git::GitSetDiffViewParams {
                                viewport_id,
                                enabled: true,
                            },
                        )
                        .await
                    };
                    self.pending.push(Box::pin(async move {
                        // Route through the core's DiffViewSet handling via a parked token?
                        // No token here — feed the event directly.
                        Done::DiffViewSet(fut.await.map_err(|e| e.to_string()))
                    }));
                }
            }
            Done::Subscribed(_, Err(e)) => {
                self.status(StatusMessage::error(format!("subscribe failed: {e}")))
            }
            Done::Window(Ok(res)) => {
                self.fetch_in_flight = false;
                self.session.adopt_window(res);
                self.clamp_scroll();
                if self.reveal_after_fetch {
                    self.reveal_after_fetch = false;
                    self.reveal_cursor();
                }
                if self.center_after_fetch {
                    self.center_after_fetch = false;
                    self.center_cursor_in_window();
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
                    self.status(StatusMessage::error(format!("viewport update failed: {e}")));
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
            Done::DiffViewSet(result) => self.dispatch(CoreEvent::DiffViewSet {
                enabled: true,
                result,
            }),
            Done::ToastExpired(id) => self.state.toasts.retain(|t| t.id != id),
            Done::Reconnected(result) => match *result {
                Ok(r) => {
                    self.handle = r.handle;
                    self.notifications = r.notifications;
                    let restarted = r.restarted;
                    self.dispatch(CoreEvent::Reestablished {
                        project: r.project,
                        open: r.open,
                        restarted,
                    });
                }
                Err(ReconnectError::NotUp) => self.dispatch(CoreEvent::ReconnectRetry),
                Err(ReconnectError::Fatal(e)) => self.dispatch(CoreEvent::ReconnectFatal(e)),
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
        // `keymap::hover_action`, so the chords never drift) — Ctrl-y copies its content, the scroll
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
                                    .status(StatusMessage::success("copied popover".to_string())),
                                Err(e) => {
                                    self.status(StatusMessage::error(format!("copy failed: {e}")))
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
        // Shell-local overlays own the keyboard while open.
        if self.state.help.open {
            let _ = crate::app::handle_help_key(&mut self.state, k);
            return;
        }
        if self.state.project_settings.is_some() {
            // Fully shell-local: the overlay drives its own RPCs on the live handle.
            let handle = self.handle.clone();
            if let Err(e) =
                crate::app::handle_project_settings_key(&handle, &mut self.state, k).await
            {
                self.status(StatusMessage::error(format!("project settings: {e}")));
            }
            // The handler reports success by writing `state.status` directly (it has no shell
            // access to push a toast); drain that into the toast stack here.
            let drained = std::mem::take(&mut self.state.status);
            self.push_toast(drained);
            // Root edits change the project paths server-side; mirror into the session.
            self.session.project_paths = self.state.project_paths.clone();
            // A removal that closed the active buffer routes through the same
            // `buffer/closed` path another client's close would push.
            if let Some(p) = self.state.pending_external_close.take() {
                let n = Notification {
                    jsonrpc: aether_protocol::envelope::JsonRpc,
                    method: <aether_protocol::buffer::BufferClosed as
                        aether_protocol::envelope::NotificationMethod>::NAME
                        .into(),
                    params: serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
                };
                self.dispatch(CoreEvent::ServerPush(n));
            }
            return;
        }
        let Some((code, mods, text)) = translate_key(&k) else {
            return;
        };
        // The no-project chooser: Esc dismisses it, which — with nothing behind it to fall back to —
        // exits the app, matching the native client. (Selecting a project instead lands a buffer and
        // proceeds; that path goes through `on_key` below.) Handled here, before the core closes the
        // picker, so it's distinguishable from a project pick (which also closes the picker).
        if code == KeyCode::Esc && self.session.is_placeholder() && self.session.picker.is_some() {
            self.should_quit = true;
            return;
        }
        let visible_rows = self.visible_rows();
        let fx = self.session.on_key(code, mods, text, visible_rows);
        self.run_effects(fx);
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if self.state.help.open {
            crate::app::handle_help_mouse(&mut self.state, m);
            return;
        }
        // While a picker is open its overlay owns the screen: the wheel moves the highlight,
        // and clicks fall through to the picker rather than the buffer underneath.
        if self.session.picker.is_some() {
            let delta = match m.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return,
            };
            let fx = self.session.picker_wheel(delta);
            self.run_effects(fx);
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

    fn run_shell_action(&mut self, action: Action) {
        match action {
            Action::Scroll { dir, unit } => {
                let rows = self.visible_rows() as i64;
                let delta = match unit {
                    ScrollUnit::Line => 1,
                    ScrollUnit::Half => (rows / 2).max(1),
                    ScrollUnit::Page => rows.max(1),
                };
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
            Action::CenterCursor => self.center_cursor(),
            Action::ToggleWrap => {
                self.session.wrap = match self.session.wrap {
                    WrapMode::Soft => WrapMode::None,
                    WrapMode::None => WrapMode::Soft,
                };
                self.sent_grid = Some(self.grid());
                self.subscribe();
            }
            Action::OpenHelp => {
                self.state.help.open = true;
                self.state.help.scroll = Default::default();
            }
            Action::OpenProjectSettings => self.open_project_settings(),
            _ => {}
        }
    }

    fn open_project_settings(&mut self) {
        // The view model is synced, so the old opener reads the right state.
        crate::app::open_project_settings(&mut self.state);
    }

    // ---- viewport geometry (iced's px math, in rows) -------------------------------------

    fn subscribe(&mut self) {
        if self.session.is_placeholder() {
            return; // no buffer to show until a project is picked (the no-project view)
        }
        let Some((cols, rows)) = self.sent_grid else {
            return;
        };
        // A fresh subscribe invalidates any in-flight fetch (new viewport identity); the core
        // no longer resets these on switch/reconnect — they live here now.
        self.fetch_in_flight = false;
        self.refetch_queued = false;
        self.reveal_after_fetch = false;
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
                logical_line: self
                    .session
                    .buffer
                    .cursor
                    .position
                    .line
                    .saturating_sub(rows / 2),
                sub_row: 0.0,
            })
        };
        self.subscribe_scroll = scroll;
        self.subscribe_epoch += 1;
        let epoch = self.subscribe_epoch;
        let h = self.handle.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let wrap = self.session.wrap;
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
    fn ensure_cursor_visible(&mut self) {
        let Some(window) = &self.session.window else {
            return;
        };
        let line = self.session.buffer.cursor.position.line;
        if line < window.first_logical_line || line >= window.last_logical_line_exclusive {
            let Some(viewport_id) = self.session.viewport_id else {
                return;
            };
            self.reveal_after_fetch = true;
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
        self.reveal_cursor();
        self.maybe_fetch();
    }

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

    fn center_cursor(&mut self) {
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
        // centre once it lands. Mirrors `ensure_cursor_visible`.
        if line < first || line >= last {
            let Some(viewport_id) = self.session.viewport_id else {
                return;
            };
            self.center_after_fetch = true;
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
        self.center_cursor_in_window();
    }

    /// Centre the cursor's line in the viewport. Assumes its line is in the loaded window (the
    /// caller pulls it in first otherwise); a no-op if its visual row isn't resolvable.
    fn center_cursor_in_window(&mut self) {
        self.reveal_cursor_col();
        let Some(window) = &self.session.window else {
            return;
        };
        let Some(row) = cursor_visual_row(window, self.session.buffer.cursor.position) else {
            return;
        };
        let half = self.visible_rows() / 2;
        self.scroll_to_row(row.saturating_sub(half));
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

    fn spawn_reconnect(&mut self, attempt: u32) {
        let project = self.session.project.clone();
        let path = self.session.buffer.path.clone();
        let buffer_id = self.session.buffer.buffer_id;
        let transient = self.session.buffer.transient;
        let cursor = self.session.buffer.cursor.position;
        let version = env!("CARGO_PKG_VERSION").to_string();
        self.pending.push(Box::pin(async move {
            Done::Reconnected(Box::new(
                dial(
                    attempt, project, path, buffer_id, transient, cursor, version,
                )
                .await,
            ))
        }));
    }

    // ---- view sync (Session → the render model ui::draw reads) ---------------------------

    fn sync(&mut self) {
        // No editor until a project is picked: the placeholder session renders the no-project
        // view, not a buffer behind the chooser.
        let editor = (!self.session.is_placeholder()).then(|| self.editor_view());
        let s = &self.session;
        let st = &mut self.state;
        st.project_name = s.project.clone();
        if st.project_paths != s.project_paths {
            st.project_paths = s.project_paths.clone();
            st.root_labels = labels::root_labels(&st.project_paths);
        }
        let (cols, rows) = self.term;
        st.viewport_cols = cols as u32;
        st.viewport_rows = (rows as u32).saturating_sub(1);
        st.conn = s.conn;
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
        self.sync_picker();
        self.sync_prompts();
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
            last_repeat: None,
            search: self.search_view(),
            blame: BlameState::default(),
            transient: s.buffer.transient,
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
        query.cursor = s.cursor;
        TuiSearchState {
            query,
            active: s.active,
            summary: s.summary.clone(),
            snapshot: None,
            history: s.history.clone(),
            history_cursor: s.history_cursor,
            history_draft: s.history_draft.clone(),
            extend_to_cursor: s.extend_to_cursor,
        }
    }

    fn sync_picker(&mut self) {
        let pane_rows =
            crate::ui::picker_result_rows(self.state.viewport_cols, self.state.viewport_rows);
        let p = &mut self.state.picker;
        let Some(core) = &self.session.picker else {
            p.open = false;
            return;
        };
        p.open = true;
        p.pane_rows = pane_rows;
        p.kind = Some(core.kind);
        p.query.set(core.query.clone());
        p.query.cursor = core.cursor;
        p.generation = core.generation;
        p.offset = core.offset;
        p.items = core.items.clone();
        p.total_matches = core.total_matches;
        p.total_candidates = core.total_candidates;
        p.ticking = core.ticking;
        p.spinner = core.spinner_glyph();
        p.total_display_rows = Some(core.total_display_rows);
        p.selected = (core.selected.saturating_sub(core.offset)) as usize;
        // The Explorer's synthetic "+ Create …" affordance — the core owns the decision
        // (`pending_create`); the shell appends it as a trailing row (italicised via
        // `synthetic_create_idx`) once the fetched window reaches the list's end, mirroring the
        // core's `display_rows`. Purely visual: Enter routes through the core's `picker_accept`,
        // which sees the create row on the *core* selection and creates the file/dir.
        p.synthetic_create_idx = None;
        if let Some(pc) = core.pending_create() {
            if core.offset + core.items.len() as u32 >= core.total_matches {
                let label = if pc.is_dir {
                    format!("+ Create directory {}/", pc.name)
                } else {
                    format!("+ Create file {}", pc.name)
                };
                p.items.push(aether_protocol::picker::PickerItem::DirEntry {
                    name: label,
                    is_dir: false,
                    match_indices: Vec::new(),
                    git_status: None,
                });
                p.synthetic_create_idx = Some(p.items.len() - 1);
            }
        }
        p.chips = core.chips.iter().map(chip_value_view).collect();
        p.chip_selected = core.chip_selected;
        p.chip_editor = core.chip_editor.as_ref().map(chip_editor_view);
        p.explorer_dir = core.directory.clone();
        p.explorer_parent = core.directory_parent.clone();
        // Keep the highlight on-screen within the fetched slice (the shell half of
        // RevealPickerSelection). Grep groups each file under a header row, so the visible
        // window holds fewer items than `pane_rows` — `picker_scroll_for_selected` walks the
        // real layout instead of assuming one row per item.
        self.picker_scroll = crate::ui::picker_scroll_for_selected(
            &p.items,
            p.selected,
            self.picker_scroll,
            p.pane_rows.max(1) as usize,
            p.kind,
        );
        p.visible_start = self.picker_scroll;
    }

    fn sync_prompts(&mut self) {
        let st = &mut self.state;
        st.confirm_prompt = None;
        st.save_prompt = None;
        st.picker.lsp_detail = None;
        match &self.session.prompt {
            Some(Prompt::Confirm { message, .. }) => {
                st.confirm_prompt = Some(crate::app::ConfirmPrompt {
                    message: message.clone(),
                    // The action runs in the core; the render only needs the message. The
                    // variant here is a placeholder the new shell never executes.
                    action: crate::app::ConfirmAction::OverwriteSaveAs,
                });
            }
            Some(Prompt::SaveAs {
                path_index,
                input,
                cursor,
            }) => {
                let mut text = crate::text_input::TextInput::default();
                text.set(input.clone());
                text.cursor = *cursor;
                st.save_prompt = Some(crate::save_prompt::SavePromptState {
                    mode: crate::save_prompt::PromptMode::Editing(
                        crate::save_prompt::EditingState {
                            path_index: *path_index,
                            listing: Vec::new(),
                            listing_dir_abs: String::new(),
                            suggestion_idx: 0,
                        },
                    ),
                    input: text,
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

// ---- chip view conversion (core chips -> the render model's types) --------------------------

fn chip_value_view(v: &aether_client::chips::ChipValue) -> crate::picker::ChipValue {
    use crate::picker::ChipValue as T;
    use aether_client::chips::ChipValue as C;
    match v {
        C::Dir(d) => T::Dir(d.clone()),
        C::Glob(g) => T::Glob(g.clone()),
        C::Case(m) => T::Case(*m),
        C::Word => T::Word,
        C::Lit => T::Lit,
        C::Ignored { hide } => T::Ignored { hide: *hide },
        C::Hidden { hide } => T::Hidden { hide: *hide },
        C::Changed => T::Changed,
    }
}

fn chip_editor_view(e: &aether_client::chips::ChipEditor) -> crate::picker::ChipEditor {
    use crate::picker as t;
    use aether_client::chips as c;
    let input = |i: &c::Input| {
        let mut x = crate::text_input::TextInput::default();
        x.set(i.text.clone());
        x.cursor = i.cursor;
        x
    };
    t::ChipEditor {
        kind: match e.kind {
            c::ChipEditorKind::Glob { edit } => t::ChipEditorKind::Glob { edit },
            c::ChipEditorKind::Dir { edit } => t::ChipEditorKind::Dir { edit },
        },
        field: match e.field {
            c::ChipEditorField::Root => t::ChipEditorField::Root,
            c::ChipEditorField::Path => t::ChipEditorField::Path,
        },
        input: input(&e.input),
        root_filter: input(&e.root_filter),
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
        CK::BackTab => KeyCode::Tab,
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

/// One paced reconnect attempt: back off, re-read discovery, dial, restore.
#[allow(clippy::too_many_arguments)]
async fn dial(
    attempt: u32,
    project: String,
    path: Option<String>,
    buffer_id: u64,
    transient: bool,
    cursor: aether_protocol::LogicalPosition,
    version: String,
) -> Result<Reestablished, ReconnectError> {
    use aether_protocol::buffer::{BufferOpen, BufferOpenParams};
    use aether_protocol::project::{ProjectActivate, ProjectActivateParams};

    tokio::time::sleep(reconnect_backoff(attempt)).await;
    let info = crate::discovery::read().map_err(|_| ReconnectError::NotUp)?;
    let server_url = format!("ws://127.0.0.1:{}", info.port);
    let (handle, notifications) = crate::connection::connect(&server_url, &version)
        .await
        .map_err(|_| ReconnectError::NotUp)?;
    let activated = handle
        .rpc::<ProjectActivate>(ProjectActivateParams {
            name: project,
            open_last: false,
        })
        .await
        .map_err(|e| ReconnectError::Fatal(e.to_string()))?;
    let params = match &path {
        Some(p) => aether_client::session::strip_longest_root(p, &activated.project.paths).map(
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
        project: activated.project,
        open,
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
/// buffer or a named file) and build the core `Session` + the render model.
pub async fn bootstrap(
    handle: &Handle,
    project: Option<&str>,
    file: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<(Session, AppState, Effects)> {
    use aether_protocol::buffer::{BufferOpen, BufferOpenParams};
    use aether_protocol::picker::PickerKind;
    use aether_protocol::project::{ProjectActivate, ProjectActivateParams};

    // Project selection is explicit. When none is named on the command line we DON'T activate
    // one — we start with a placeholder session (no project, no buffer) and raise the Projects
    // chooser. Nothing is rendered behind it; picking a project activates it and lands the first
    // buffer (`PickerSelected` → `ProjectActivated` → `adopt_switch`), which is when the editor
    // first appears. Its `picker/view` request rides the returned effects, run once the shell is up.
    let (session, project_name, project_paths, startup) = match project {
        None => {
            let mut session = Session::placeholder();
            let startup = session.open_picker(PickerKind::Projects, None, None);
            (session, String::new(), Vec::new(), startup)
        }
        Some(project) => {
            let activated = handle
                .rpc::<ProjectActivate>(ProjectActivateParams {
                    name: project.to_string(),
                    open_last: file.is_none(),
                })
                .await?;
            let project_paths = activated.project.paths.clone();

            let open = match file {
                Some(f) => {
                    let abs = crate::app::resolve_cli_path(f)?.display().to_string();
                    let (path_index, relative_path) =
                        aether_client::session::strip_longest_root(&abs, &project_paths)
                            .ok_or_else(|| {
                                anyhow::anyhow!("{abs} is outside the project's roots")
                            })?;
                    handle
                        .rpc::<BufferOpen>(BufferOpenParams {
                            path_index: Some(path_index),
                            relative_path: Some(relative_path),
                            create_if_missing: true,
                            ..Default::default()
                        })
                        .await?
                }
                None => activated.opened.ok_or_else(|| {
                    anyhow::anyhow!("project/activate returned no landing buffer")
                })?,
            };

            let session = Session::new(
                activated.project.name.clone(),
                project_paths.clone(),
                buffer_info(open, &project_paths),
            );
            (
                session,
                activated.project.name,
                project_paths,
                Effects::none(),
            )
        }
    };

    let root_labels = labels::root_labels(&project_paths);
    let state = AppState {
        project_name,
        project_paths,
        root_labels,
        viewport_cols: cols as u32,
        viewport_rows: (rows as u32).saturating_sub(1),
        should_quit: false,
        status: StatusMessage::default(),
        toasts: Vec::new(),
        conn: ConnState::Connected,
        last_terminal_title: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: Default::default(),
        save_prompt: None,
        confirm_prompt: None,
        editor: None,
        project_settings: None,
        help: Default::default(),
        lsp_status: Default::default(),
        hover: None,
        diagnostic_counts: Default::default(),
        pending_external_close: None,
    };
    Ok((session, state, startup))
}
