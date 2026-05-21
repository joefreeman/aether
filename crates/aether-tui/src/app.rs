//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use crate::client::Client;
use crate::clipboard;
use crate::ui;
use aether_protocol::buffer::{
    BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen,
    BufferOpenParams, BufferOpenResult, BufferSave, BufferSaveParams, BufferState,
    BufferStateParams, CopyScope,
};
use aether_protocol::directory::{
    DirEntry, DirectoryCreate, DirectoryCreateParams, DirectoryList, DirectoryListParams,
    DirectoryListResult,
};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNext, SearchPrev, SearchSet,
    SearchSetParams, SearchStateChanged, SearchSummary,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo,
    CursorUndoParams, CursorUndoResult, Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::handshake::ClientHelloResult;
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDedent, InputDelete, InputDeleteParams, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputRedo, InputText, InputTextParams,
    InputUndo, UndoResult,
};
use aether_protocol::viewport::{
    LogicalLineRender, ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams,
    ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportSetWrap,
    ViewportSetWrapParams, ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult,
    WrapMode,
};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};
use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use tokio::sync::mpsc;
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Search,
    FileBrowser,
}

/// State for the directory listing UI. Populated when entering `Mode::FileBrowser` via `-`.
#[derive(Debug, Default)]
pub struct FileBrowserState {
    /// Canonical absolute path of the directory currently being listed.
    pub path: String,
    /// Canonical absolute path of the parent (allowed) directory, or `None` if we're at a
    /// project-root boundary.
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
    /// Highlight index into `entries`.
    pub selected: usize,
    /// Active prompt overlay (status bar takes over). `None` when navigating the listing.
    pub prompt: Option<FileBrowserPrompt>,
}

#[derive(Debug, Clone)]
pub struct FileBrowserPrompt {
    pub kind: FileBrowserPromptKind,
    /// Text the user has typed so far.
    pub input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileBrowserPromptKind {
    /// `Ctrl-n`: prompt for a filename, then open it as a new buffer (file created on save).
    NewFile,
    /// `Ctrl-Alt-n`: prompt for a directory name, then create it and step into it.
    NewDirectory,
}

/// Captured state for a pending `f`/`t` keystroke — the next char the user types becomes the
/// target of a `Motion::FindChar`.
#[derive(Debug, Clone, Copy)]
pub struct PendingFind {
    pub direction: Direction,
    pub till: bool,
    pub extend: bool,
    pub count: u32,
}

/// Client-side mirror of the server's search state. The server owns the match list (and pushes
/// per-line highlights via viewport line renders); the client just tracks the query, the latest
/// summary, the history list, and the snapshot used to revert from Mode::Search via Esc.
#[derive(Debug, Default)]
pub struct SearchState {
    /// The current query — live while in Mode::Search, the committed query otherwise.
    pub query: String,
    /// True when there is a committed search on the server (set via `search/set` with a non-empty
    /// query and not later cleared). Used to gate highlighting and the `n`/`Alt-n` bindings.
    pub active: bool,
    /// Server-pushed summary (total, truncated, current_index). `None` before any search runs.
    pub summary: Option<SearchSummary>,
    /// Snapshot of pre-search-mode state, used by Esc to revert.
    pub snapshot: Option<SearchSnapshot>,
    /// Committed queries, oldest first. Up/Down in Mode::Search browses this; `n`/`Alt-n` with
    /// no active search re-activates the most recent entry.
    pub history: Vec<String>,
    /// `None` while the user is typing a fresh query; `Some(i)` while they're browsing the entry
    /// at `history[i]`. Any edit (typing/backspace) snaps back to `None`.
    pub history_cursor: Option<usize>,
    /// The live-typed query, stashed when the user steps into history with Up so that Down can
    /// restore it on the way back out.
    pub history_draft: String,
}

#[derive(Debug)]
pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub scroll_logical_line: u32,
    pub query: String,
    pub active: bool,
}

pub struct AppState {
    pub project_name: String,
    pub file_label: String,
    pub buffer_id: BufferId,
    pub viewport_id: ViewportId,
    pub cursor: CursorState,
    pub scroll_logical_line: u32,
    pub window_first_logical_line: u32,
    pub lines: Vec<LogicalLineRender>,
    pub viewport_cols: u32,
    pub viewport_rows: u32,
    /// Total logical lines in the buffer, kept fresh from every viewport response /
    /// `viewport/lines_changed` notification.
    pub line_count: u32,
    /// Highest legal `scroll_logical_line` — server-computed so it accounts for wrap, putting
    /// the buffer's last visual row at the bottom of the viewport.
    pub max_scroll_logical_line: u32,
    pub wrap: WrapMode,
    /// Horizontal scroll, in bytes. Only meaningful when `wrap == WrapMode::None`; reset to 0 when
    /// soft wrap is on (wrapped content never overflows the viewport horizontally). Client-only —
    /// the server doesn't know about horizontal scroll.
    pub scroll_col: u32,
    /// Accumulated vertical-scroll delta from arrow-key / PageUp-PageDown bursts. The actual
    /// `viewport/scroll` RPC is deferred until just before the next draw (or until a motion
    /// triggers `ensure_cursor_in_window`), so a trackpad fling becomes one RPC instead of N.
    pub pending_scroll_lines: i64,
    /// Anchor position set by a left-mouse-button down. Subsequent drags use it as the selection
    /// anchor; cleared on mouse-up.
    pub drag_anchor: Option<LogicalPosition>,
    pub revision: u64,
    /// Revision at the most recent successful save. `dirty` is derived as
    /// `revision != saved_revision` — no separate flag to keep in sync.
    pub saved_revision: u64,
    pub should_quit: bool,
    pub status: String,
    pub mode: Mode,
    /// Digit-prefix count for the next motion. Reset after consumption.
    pub pending_count: u32,
    /// Set after `f`/`t`/`F`/`T` (and their Alt variants); the next keystroke is interpreted as
    /// the target character rather than a normal-mode binding.
    pub pending_find: Option<PendingFind>,
    /// The most recent repeatable motion, replayed by `r` (cursor move) or `Shift-r` (cursor
    /// move + extend selection). Absolute-position motions (line/buffer endpoints, goto) aren't
    /// stored because repeating them is a no-op.
    pub last_motion: Option<Motion>,
    /// System clipboard handle. Held for the app's lifetime so the X11 selection isn't
    /// abandoned every operation. `None` if the clipboard couldn't be initialised (e.g. headless).
    pub clipboard: Option<arboard::Clipboard>,
    pub search: SearchState,
    /// Canonical absolute path of the current buffer's file on disk, if any. Used by the
    /// file-browser entry point (`-`) to know which directory to open.
    pub file_path: Option<String>,
    /// Project paths declared at startup — used as the file-browser root when there's no
    /// current file (scratch buffer).
    pub project_paths: Vec<String>,
    pub file_browser: FileBrowserState,
}

impl AppState {
    pub fn dirty(&self) -> bool {
        self.revision != self.saved_revision
    }
}

pub async fn bootstrap(
    client: &mut Client,
    token: String,
    file: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<AppState> {
    let viewport_rows = rows.saturating_sub(1) as u32;
    let viewport_cols = cols as u32;

    let hello: ClientHelloResult = client
        .rpc::<aether_protocol::handshake::ClientHello>(
            aether_protocol::handshake::ClientHelloParams {
                token,
                client_version: env!("CARGO_PKG_VERSION").into(),
            },
        )
        .await?;

    let (buffer_open_params, file_label) = match file {
        Some(f) => (
            aether_protocol::buffer::BufferOpenParams {
                path_index: Some(0),
                relative_path: Some(f.into()),
                language: None,
                create_if_missing: false,
            },
            f.to_string(),
        ),
        None => (
            aether_protocol::buffer::BufferOpenParams {
                path_index: None,
                relative_path: None,
                language: None,
                create_if_missing: false,
            },
            "[scratch]".to_string(),
        ),
    };
    let open: BufferOpenResult = client
        .rpc::<aether_protocol::buffer::BufferOpen>(buffer_open_params)
        .await?;

    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: viewport_cols,
            rows: viewport_rows,
            overscan_rows: viewport_rows,
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::Soft,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
        })
        .await?;

    Ok(AppState {
        project_name: hello.project.name,
        file_label,
        buffer_id: open.buffer_id,
        viewport_id: sub.viewport_id,
        cursor: CursorState::default(),
        scroll_logical_line: 0,
        window_first_logical_line: sub.window.first_logical_line,
        lines: sub.window.lines,
        viewport_cols,
        viewport_rows,
        line_count: sub.window.line_count,
        max_scroll_logical_line: sub.window.max_scroll_logical_line,
        wrap: WrapMode::Soft,
        scroll_col: 0,
        pending_scroll_lines: 0,
        drag_anchor: None,
        revision: open.revision,
        saved_revision: open.saved_revision,
        should_quit: false,
        status: String::new(),
        mode: Mode::Normal,
        pending_count: 0,
        pending_find: None,
        last_motion: None,
        clipboard: clipboard::new_handle(),
        search: SearchState::default(),
        file_path: open.path.clone(),
        project_paths: hello.project.paths.clone(),
        file_browser: FileBrowserState::default(),
    })
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &mut Client,
    state: &mut AppState,
) -> Result<()> {
    // Background task forwards events into a channel. Doing it this way (rather than awaiting
    // `EventStream::next` directly in the main `select!`) means we can use `try_recv` to drain
    // backlogged events between draws — `tokio::sync::mpsc` supports non-blocking recv natively,
    // whereas `now_or_never` on the EventStream future leaves the stream in a state where later
    // events don't wake the task. A trackpad scroll burst now coalesces into one redraw.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<std::io::Result<Event>>();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(ev) = events.next().await {
            if event_tx.send(ev).is_err() {
                break; // main loop dropped the receiver; we're shutting down
            }
        }
    });

    apply_cursor_style(state.mode);
    terminal.draw(|f| ui::draw(f, state))?;
    while !state.should_quit {
        tokio::select! {
            ev = event_rx.recv() => {
                let Some(ev) = ev else { break };
                let ev = ev?;
                dispatch_terminal_event(client, state, ev).await?;
                // Drain any other events that piled up while we were dispatching. mpsc's
                // try_recv is non-blocking and safe to call repeatedly.
                while !state.should_quit {
                    match event_rx.try_recv() {
                        Ok(ev) => dispatch_terminal_event(client, state, ev?).await?,
                        Err(_) => break, // empty or disconnected — either way, we're done draining
                    }
                }
            }
            inbound = client.recv() => {
                let Some(inbound) = inbound? else { break };
                if let ClientInbound::Notification(n) = inbound {
                    apply_notification(state, n);
                }
            }
        }
        apply_pending_notifications(state, client);
        flush_pending_scroll(client, state).await?;
        terminal.draw(|f| ui::draw(f, state))?;
    }
    Ok(())
}

async fn dispatch_terminal_event(
    client: &mut Client,
    state: &mut AppState,
    ev: Event,
) -> Result<()> {
    // Each user-driven event clears the ephemeral status line before being processed. Anything
    // the event itself sets (save/copy feedback, search truncation, etc.) stays visible until
    // the *next* event.
    state.status.clear();
    if let Event::Resize(cols, rows) = &ev {
        handle_resize(client, state, *cols, *rows).await
    } else {
        handle_event(client, state, ev).await
    }
}

fn apply_cursor_style(mode: Mode) {
    let style = match mode {
        Mode::Normal => SetCursorStyle::SteadyBlock,
        Mode::Insert | Mode::Search => SetCursorStyle::SteadyBar,
        Mode::FileBrowser => SetCursorStyle::SteadyBlock,
    };
    let _ = execute!(stdout(), style);
}

fn apply_pending_notifications(state: &mut AppState, client: &mut Client) {
    for n in client.drain_notifications() {
        apply_notification(state, n);
    }
}

fn apply_notification(state: &mut AppState, n: aether_protocol::envelope::Notification) {
    if n.method == ViewportLinesChanged::NAME {
        match serde_json::from_value::<ViewportLinesChangedParams>(n.params) {
            Ok(p) if p.viewport_id == state.viewport_id => {
                splice_lines(state, p);
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad notif params: {e}"),
        }
    } else if n.method == BufferState::NAME {
        match serde_json::from_value::<BufferStateParams>(n.params) {
            Ok(p) if p.buffer_id == state.buffer_id => {
                state.saved_revision = p.saved_revision;
                if state.revision == state.saved_revision {
                    state.status = format!("saved (rev {})", state.saved_revision);
                }
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad buffer/state params: {e}"),
        }
    } else if n.method == SearchStateChanged::NAME {
        match serde_json::from_value::<SearchSummary>(n.params) {
            Ok(s) if s.buffer_id == state.buffer_id => {
                state.search.summary = Some(s);
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad search/state_changed params: {e}"),
        }
    }
}

fn splice_lines(state: &mut AppState, p: ViewportLinesChangedParams) {
    state.revision = p.revision;
    state.line_count = p.line_count;
    state.max_scroll_logical_line = p.max_scroll_logical_line;
    let local_start = (p.range.start_logical_line as i64) - (state.window_first_logical_line as i64);
    let local_end = (p.range.end_logical_line_exclusive as i64) - (state.window_first_logical_line as i64);
    if local_end < 0 || local_start > state.lines.len() as i64 {
        return;
    }
    let lo = local_start.max(0) as usize;
    let hi = (local_end as usize).min(state.lines.len());
    state.lines.splice(lo..hi, p.replacement_lines);
}

async fn handle_event(client: &mut Client, state: &mut AppState, ev: Event) -> Result<()> {
    // Track whether the cursor moved during this event. Pure-scroll bindings leave it alone, so
    // the viewport stays where the user scrolled; any binding that actually moves the cursor
    // triggers `ensure_cursor_in_window` to snap the view back to it.
    let cursor_before = state.cursor.position;
    match ev {
        Event::Key(k) => {
            if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
                return Ok(());
            }
            match state.mode {
                Mode::Normal => handle_normal_key(client, state, k).await?,
                Mode::Insert => handle_insert_key(client, state, k).await?,
                Mode::Search => handle_search_key(client, state, k).await?,
                Mode::FileBrowser => handle_file_browser_key(client, state, k).await?,
            }
        }
        Event::Mouse(m) => handle_mouse_event(client, state, m).await?,
        _ => return Ok(()),
    }
    if state.cursor.position != cursor_before {
        ensure_cursor_in_window(client, state).await?;
    }
    Ok(())
}

async fn handle_mouse_event(
    client: &mut Client,
    state: &mut AppState,
    m: MouseEvent,
) -> Result<()> {
    match m.kind {
        MouseEventKind::ScrollUp => scroll_lines(state, -3),
        MouseEventKind::ScrollDown => scroll_lines(state, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            // Shift-click is left for the terminal's native text selection (copy-paste).
            if m.modifiers.contains(KeyModifiers::SHIFT) {
                return Ok(());
            }
            if let Some(pos) = ui::screen_to_logical(state, m.row, m.column) {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.buffer_id,
                        position: pos,
                        anchor: None,
                    })
                    .await?;
                state.cursor = new;
                state.drag_anchor = Some(new.position);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(anchor) = state.drag_anchor {
                if let Some(pos) = ui::screen_to_logical(state, m.row, m.column) {
                    let new = client
                        .rpc::<CursorSet>(CursorSetParams {
                            buffer_id: state.buffer_id,
                            position: pos,
                            anchor: Some(anchor),
                        })
                        .await?;
                    state.cursor = new;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.drag_anchor = None;
        }
        _ => {}
    }
    Ok(())
}

/// Normalize a `KeyEvent`'s `(code, modifiers)` so a `Shift-x` key reports as `('x', SHIFT)`
/// regardless of whether the terminal sent it as uppercase + no-shift or lowercase + shift.
fn normalize_key(k: KeyEvent) -> (KeyCode, KeyModifiers) {
    let mut mods = k.modifiers;
    let code = match k.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Char(c.to_ascii_lowercase())
        }
        other => other,
    };
    (code, mods)
}

const SHIFT_ONLY: KeyModifiers = KeyModifiers::SHIFT;
const ALT_ONLY: KeyModifiers = KeyModifiers::ALT;
const CTRL_ONLY: KeyModifiers = KeyModifiers::CONTROL;

async fn handle_normal_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Pending `f`/`t`: the next keystroke names the target character. Use the raw key (skipping
    // `normalize_key`) so `f X` is case-sensitive. Any non-`Char` key (Esc, arrow, etc.) cancels.
    if let Some(pending) = state.pending_find.take() {
        if let KeyCode::Char(ch) = k.code {
            move_motion(
                client,
                state,
                Motion::FindChar {
                    ch,
                    direction: pending.direction,
                    count: pending.count,
                    till: pending.till,
                },
                pending.extend,
            )
            .await?;
        }
        return Ok(());
    }

    let (code, mods) = normalize_key(k);

    // Digit accumulation for counts. `0` is the line-start motion unless we're already mid-count.
    if let KeyCode::Char(c @ '1'..='9') = code {
        if mods == KeyModifiers::NONE {
            state.pending_count = state
                .pending_count
                .saturating_mul(10)
                .saturating_add(c.to_digit(10).unwrap_or(0));
            return Ok(());
        }
    }
    if let KeyCode::Char('0') = code {
        if mods == KeyModifiers::NONE && state.pending_count > 0 {
            state.pending_count = state.pending_count.saturating_mul(10);
            return Ok(());
        }
    }

    // Whatever this command consumes for `count`, reset after.
    let count = if state.pending_count == 0 { 1 } else { state.pending_count };
    state.pending_count = 0;

    let extend = mods.contains(KeyModifiers::SHIFT);

    match (code, mods) {
        // ---- meta ----
        (KeyCode::Char('q'), CTRL_ONLY) => {
            state.should_quit = true;
        }
        (KeyCode::Esc, _) => {
            // Drop the active search (clears highlights, disables n/Alt-n). Use `d` to drop the
            // current selection instead.
            if state.search.active || state.search.summary.is_some() {
                let _ = client
                    .rpc::<SearchClear>(SearchClearParams { buffer_id: state.buffer_id })
                    .await;
            }
            state.search.active = false;
            state.search.summary = None;
        }
        (KeyCode::Char('d'), m) if m == KeyModifiers::NONE => {
            if state.cursor.anchor.is_some() {
                clear_selection(client, state).await?;
            }
        }

        // ---- non-letter motions and scroll ----
        // Home/End map to logical-line start/end; arrows scroll the viewport without moving the
        // cursor (so the cursor can drift off-screen until a motion snaps it back). Alt-arrow
        // scrolls by a half-viewport. PageUp/Down are full-viewport scrolls.
        (KeyCode::Home, _) => move_motion(client, state, Motion::LineStart, extend).await?,
        (KeyCode::End, _) => move_motion(client, state, Motion::LineEnd, extend).await?,
        (KeyCode::PageDown, _) => scroll_lines(state, state.viewport_rows as i64),
        (KeyCode::PageUp, _) => scroll_lines(state, -(state.viewport_rows as i64)),
        (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) =>
            scroll_lines(state, -((state.viewport_rows / 2) as i64)),
        (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) =>
            scroll_lines(state, (state.viewport_rows / 2) as i64),
        (KeyCode::Up, _) => scroll_lines(state, -1),
        (KeyCode::Down, _) => scroll_lines(state, 1),
        (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) =>
            scroll_cols(state, -((state.viewport_cols / 2) as i64)),
        (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) =>
            scroll_cols(state, (state.viewport_cols / 2) as i64),
        (KeyCode::Left, _) => scroll_cols(state, -1),
        (KeyCode::Right, _) => scroll_cols(state, 1),

        // ---- motions: hjkl (logical) and Alt-hjkl (line jumps + visual rows) ----
        // `h/l` move by char; `Alt-h/l` jump to the first non-whitespace / end of the logical
        // line. `j/k` move by logical line; `Alt-j/k` move by one visual row (the only "visual"
        // motion now — used to step inside wrapped content). `0` (below) goes to literal col 0
        // for cases where you want column zero, not first non-blank.
        (KeyCode::Char('h'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::LineFirstNonblank, extend).await?,
        (KeyCode::Char('h'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::Char { direction: Direction::Backward, count }, extend).await?,
        (KeyCode::Char('l'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::LineEnd, extend).await?,
        (KeyCode::Char('l'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::Char { direction: Direction::Forward, count }, extend).await?,
        (KeyCode::Char('k'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Up, count }, extend).await?,
        (KeyCode::Char('k'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::LogicalLine { direction: Direction::Backward, count, preserve_col: true }, extend).await?,
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Down, count }, extend).await?,
        (KeyCode::Char('j'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::LogicalLine { direction: Direction::Forward, count, preserve_col: true }, extend).await?,

        // ---- motions: WORD (w/b/e) and Alt for word ----
        // Plain `w/b/e` use big WORDs (whitespace-delimited); `Alt-w/b/e` use small words
        // (alphanumeric/symbol category transitions). Forward `w` is exclusive when extending —
        // Shift-w selects up to (but not including) the start of the next WORD, matching the
        // vim/helix convention that operator-style selections don't bleed into the next word.
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::Word { direction: Direction::Forward, count, boundary: WordBoundary::Word, exclusive: extend }, extend).await?,
        (KeyCode::Char('w'), m) if !m.contains(KeyModifiers::CONTROL) =>
            move_motion(client, state, Motion::Word { direction: Direction::Forward, count, boundary: WordBoundary::BigWord, exclusive: extend }, extend).await?,
        (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::Word { direction: Direction::Backward, count, boundary: WordBoundary::Word, exclusive: false }, extend).await?,
        (KeyCode::Char('b'), m) if !m.contains(KeyModifiers::CONTROL) =>
            move_motion(client, state, Motion::Word { direction: Direction::Backward, count, boundary: WordBoundary::BigWord, exclusive: false }, extend).await?,
        (KeyCode::Char('e'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::WordEnd { direction: Direction::Forward, count, boundary: WordBoundary::Word }, extend).await?,
        (KeyCode::Char('e'), _) =>
            move_motion(client, state, Motion::WordEnd { direction: Direction::Forward, count, boundary: WordBoundary::BigWord }, extend).await?,

        // ---- motions: line start ----
        (KeyCode::Char('0'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::LineStart, extend).await?,

        // ---- motions: find char (`f`/`t` + Alt for backward, Shift to extend) ----
        // After pressing one of these, the *next* keystroke is interpreted as the target
        // character (see the `pending_find` block at the top of this handler).
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) =>
            state.pending_find = Some(PendingFind { direction: Direction::Backward, till: false, extend, count }),
        (KeyCode::Char('f'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            state.pending_find = Some(PendingFind { direction: Direction::Forward, till: false, extend, count }),
        (KeyCode::Char('t'), m) if m.contains(KeyModifiers::ALT) =>
            state.pending_find = Some(PendingFind { direction: Direction::Backward, till: true, extend, count }),
        (KeyCode::Char('t'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            state.pending_find = Some(PendingFind { direction: Direction::Forward, till: true, extend, count }),

        // ---- motions: goto line ----
        // `g` jumps to line N (1-indexed; no prefix = line 1). `Alt-g` jumps to the last line.
        // Shift extends the selection. The server clamps line numbers past EOF.
        (KeyCode::Char('g'), m) if m.contains(KeyModifiers::ALT) => {
            let target = LogicalPosition { line: state.line_count.saturating_sub(1), col: 0 };
            move_motion(client, state, Motion::Goto { position: target }, extend).await?
        }
        (KeyCode::Char('g'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            let target = LogicalPosition { line: count.saturating_sub(1), col: 0 };
            move_motion(client, state, Motion::Goto { position: target }, extend).await?
        }

        // ---- line selection ----
        // `x` always grows the selection's bottom edge downward; `Alt-x` always grows the top
        // edge upward. With no selection: `x` picks the current line (or the next at end-of-line)
        // and `Alt-x` picks the previous (or the current at end-of-line). The `Shift` variants
        // keep the other edge in place (extending); the non-shift variants collapse onto a single
        // line at the moved edge. The cursor stays on whichever end (top/bottom) it was on, so
        // the bindings behave the same after `o` flips the selection direction.
        (KeyCode::Char('x'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            select_line(client, state, Direction::Forward, extend, count).await?,
        (KeyCode::Char('x'), m) if m.contains(KeyModifiers::ALT) =>
            select_line(client, state, Direction::Backward, extend, count).await?,

        // ---- selection manipulation ----
        // `o` swaps the cursor and anchor — flips which end of the selection is the "leading"
        // edge, so a subsequent `Shift-*` motion extends from the other side.
        (KeyCode::Char('o'), m) if m == KeyModifiers::NONE => swap_anchor(client, state).await?,

        // Motion undo / redo — per-client history of cursor/selection changes, capped at the
        // last buffer mutation. Distinct from `Ctrl-u`/`Ctrl-Alt-u` which rewind buffer edits.
        (KeyCode::Char('u'), m) if m == ALT_ONLY => motion_redo(client, state, count).await?,
        (KeyCode::Char('u'), m) if m == KeyModifiers::NONE => motion_undo(client, state, count).await?,

        // Repeat the last *repeatable* motion (see `is_repeatable_motion`). `r` runs it as a
        // plain cursor move; `Shift-r` runs it extending the current selection. `Nr` loops the
        // motion N times — so e.g. after `f x`, `5r` jumps to the 5th next `x`.
        (KeyCode::Char('r'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            if let Some(motion) = state.last_motion.clone() {
                for _ in 0..count.max(1) {
                    move_motion(client, state, motion.clone(), extend).await?;
                }
            }
        }

        // ---- mode transitions ----
        (KeyCode::Char('i'), m) if m == KeyModifiers::NONE => enter_insert_at(client, state, InsertWhere::SelectionStart).await?,
        (KeyCode::Char('a'), m) if m == KeyModifiers::NONE => enter_insert_at(client, state, InsertWhere::SelectionEnd).await?,
        (KeyCode::Char('i'), m) if m == ALT_ONLY => enter_insert_at(client, state, InsertWhere::FirstLineStart).await?,
        (KeyCode::Char('a'), m) if m == ALT_ONLY => enter_insert_at(client, state, InsertWhere::LastLineEnd).await?,

        // ---- viewport ----
        (KeyCode::Char('w'), CTRL_ONLY) => toggle_wrap(client, state).await?,
        (KeyCode::Char('z'), m) if m == KeyModifiers::NONE => center_cursor(client, state).await?,

        // ---- edits ----
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('u'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            redo(client, state, count).await?,
        (KeyCode::Char('u'), CTRL_ONLY) => undo(client, state, count).await?,
        (KeyCode::Char('j'), CTRL_ONLY) => move_lines(client, state, VerticalDirection::Down, count).await?,
        (KeyCode::Char('k'), CTRL_ONLY) => move_lines(client, state, VerticalDirection::Up, count).await?,
        (KeyCode::Char('g'), CTRL_ONLY) => join_lines(client, state, count).await?,
        (KeyCode::Char('l'), CTRL_ONLY) => indent(client, state, count).await?,
        (KeyCode::Char('h'), CTRL_ONLY) => dedent(client, state, count).await?,
        (KeyCode::Char('o'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            open_line_above(client, state).await?,
        (KeyCode::Char('o'), CTRL_ONLY) => open_line_below(client, state).await?,
        (KeyCode::Char('d'), CTRL_ONLY) | (KeyCode::Delete, _) => {
            delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count }).await?
        }
        (KeyCode::Backspace, _) => {
            delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count }).await?
        }

        // ---- change ----
        // `Ctrl-c` replaces the current selection (or the cursor's 1-char range) with a fresh
        // edit — delete + enter Insert mode in one step.
        (KeyCode::Char('c'), CTRL_ONLY) => change_selection(client, state).await?,

        // ---- clipboard ----
        (KeyCode::Char('y'), CTRL_ONLY) => copy_to_clipboard(client, state, CopyScope::Selection).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => cut_to_clipboard(client, state, CopyScope::Selection).await?,
        (KeyCode::Char('p'), CTRL_ONLY) => paste_before(client, state, count).await?,
        (KeyCode::Char('r'), CTRL_ONLY) => paste_replace(client, state, count).await?,

        // ---- file browser ----
        // `-` lists the parent of the current file (or the first project path if scratch).
        (KeyCode::Char('-'), m) if m == KeyModifiers::NONE =>
            open_file_browser(client, state).await?,

        // ---- search ----
        (KeyCode::Char('/'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            enter_search_mode(client, state).await?,
        (KeyCode::Char('/'), m) if m == ALT_ONLY =>
            search_from_selection(client, state).await?,
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::ALT) =>
            search_cycle(client, state, Direction::Backward, count).await?,
        (KeyCode::Char('n'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            search_cycle(client, state, Direction::Forward, count).await?,

        _ => {}
    }
    Ok(())
}

async fn handle_insert_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    let (code, mods) = normalize_key(k);
    match (code, mods) {
        (KeyCode::Esc, _) => leave_insert(state),

        // Allow Ctrl-S / Ctrl-U / Ctrl-Alt-U to work in insert mode too.
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('u'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            redo(client, state, 1).await?,
        (KeyCode::Char('u'), CTRL_ONLY) => undo(client, state, 1).await?,

        // Clipboard: in insert mode copy/cut operate on the current line.
        (KeyCode::Char('y'), CTRL_ONLY) => copy_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => cut_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('p'), CTRL_ONLY) => paste_at_cursor(client, state).await?,

        (KeyCode::Backspace, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }).await?,
        (KeyCode::Delete, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }).await?,
        (KeyCode::Enter, _) => insert_text(client, state, "\n").await?,
        (KeyCode::Tab, _) => insert_text(client, state, "\t").await?,
        (KeyCode::Left, _) => move_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }, false).await?,
        (KeyCode::Right, _) => move_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }, false).await?,
        (KeyCode::Up, _) => move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Up, count: 1 }, false).await?,
        (KeyCode::Down, _) => move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Down, count: 1 }, false).await?,

        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            // `normalize_key` lowercased the char and synthesised SHIFT so the Ctrl-* bindings
            // above can match consistently. Reverse that for actual text insertion.
            let c = if m.contains(KeyModifiers::SHIFT) { c.to_ascii_uppercase() } else { c };
            insert_text(client, state, &c.to_string()).await?;
        }

        _ => {}
    }
    Ok(())
}

/// Open the file browser, listing the parent directory of the current file (or the first project
/// path for a scratch buffer). The previous buffer stays loaded server-side and is restored by
/// `Esc`. The current file's entry is pre-selected in the listing so the user lands on it.
async fn open_file_browser(client: &mut Client, state: &mut AppState) -> Result<()> {
    let file_name = state
        .file_path
        .as_ref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|os| os.to_str())
        .map(|s| s.to_string());
    let start = state
        .file_path
        .as_ref()
        .and_then(|p| std::path::Path::new(p).parent().map(|p| p.display().to_string()));
    load_file_browser(client, state, start).await?;
    if let Some(name) = file_name {
        select_entry_by_name(&mut state.file_browser, &name);
    }
    state.mode = Mode::FileBrowser;
    apply_cursor_style(state.mode);
    Ok(())
}

/// Move the highlight to the entry named `name`. No-op if no entry matches.
fn select_entry_by_name(fb: &mut FileBrowserState, name: &str) {
    if let Some(idx) = fb.entries.iter().position(|e| e.name == name) {
        fb.selected = idx;
    }
}

/// Ask the server for a directory listing and stash the result in `state.file_browser`.
/// `path = None` lets the server default to the first project path.
async fn load_file_browser(
    client: &mut Client,
    state: &mut AppState,
    path: Option<String>,
) -> Result<()> {
    let result: DirectoryListResult = client
        .rpc::<DirectoryList>(DirectoryListParams { path })
        .await?;
    state.file_browser = FileBrowserState {
        path: result.path,
        parent: result.parent,
        entries: result.entries,
        selected: 0,
        prompt: None,
    };
    Ok(())
}

async fn handle_file_browser_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    // Active prompt swallows input. Use the raw key (skipping `normalize_key`) so filenames keep
    // their original casing.
    if state.file_browser.prompt.is_some() {
        return handle_file_browser_prompt_key(client, state, k).await;
    }

    let (code, mods) = normalize_key(k);
    match (code, mods) {
        (KeyCode::Char('q'), CTRL_ONLY) => state.should_quit = true,
        (KeyCode::Char('n'), CTRL_ONLY) => begin_prompt(state, FileBrowserPromptKind::NewFile),
        (KeyCode::Char('n'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            begin_prompt(state, FileBrowserPromptKind::NewDirectory),
        (KeyCode::Esc, _) => leave_file_browser(state),
        // Move the highlight.
        (KeyCode::Char('j'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            if !state.file_browser.entries.is_empty() {
                state.file_browser.selected =
                    (state.file_browser.selected + 1).min(state.file_browser.entries.len() - 1);
            }
        }
        (KeyCode::Char('k'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            state.file_browser.selected = state.file_browser.selected.saturating_sub(1);
        }
        (KeyCode::Down, _) => {
            if !state.file_browser.entries.is_empty() {
                state.file_browser.selected =
                    (state.file_browser.selected + 1).min(state.file_browser.entries.len() - 1);
            }
        }
        (KeyCode::Up, _) => {
            state.file_browser.selected = state.file_browser.selected.saturating_sub(1);
        }
        // Go to the parent directory (clamped to the project boundary by the server). Pre-select
        // the entry corresponding to the directory we're leaving so the user keeps their bearings.
        (KeyCode::Char('-'), m) if m == KeyModifiers::NONE => {
            if let Some(parent) = state.file_browser.parent.clone() {
                let leaving = std::path::Path::new(&state.file_browser.path)
                    .file_name()
                    .and_then(|os| os.to_str())
                    .map(|s| s.to_string());
                load_file_browser(client, state, Some(parent)).await?;
                if let Some(name) = leaving {
                    select_entry_by_name(&mut state.file_browser, &name);
                }
            }
        }
        // Open the highlighted entry: descend if dir, switch to editing if file.
        (KeyCode::Enter, _) => {
            let Some(entry) = state.file_browser.entries.get(state.file_browser.selected) else {
                return Ok(());
            };
            let entry_path = std::path::Path::new(&state.file_browser.path)
                .join(&entry.name)
                .display()
                .to_string();
            if entry.is_dir {
                load_file_browser(client, state, Some(entry_path)).await?;
            } else {
                open_file_in_browser(client, state, entry_path).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn leave_file_browser(state: &mut AppState) {
    state.file_browser.prompt = None;
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);
}

fn begin_prompt(state: &mut AppState, kind: FileBrowserPromptKind) {
    state.file_browser.prompt = Some(FileBrowserPrompt { kind, input: String::new() });
    // Bar cursor while typing in the prompt — restored on commit/cancel.
    let _ = execute!(stdout(), SetCursorStyle::SteadyBar);
}

async fn handle_file_browser_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => {
            state.file_browser.prompt = None;
            apply_cursor_style(state.mode);
        }
        (KeyCode::Enter, _) => {
            let prompt = state.file_browser.prompt.take();
            apply_cursor_style(state.mode);
            if let Some(prompt) = prompt {
                if !prompt.input.is_empty() {
                    commit_prompt(client, state, prompt).await?;
                }
            }
        }
        (KeyCode::Backspace, _) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.pop();
            }
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.push(c);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn commit_prompt(
    client: &mut Client,
    state: &mut AppState,
    prompt: FileBrowserPrompt,
) -> Result<()> {
    let target_abs = std::path::Path::new(&state.file_browser.path)
        .join(&prompt.input)
        .display()
        .to_string();
    match prompt.kind {
        FileBrowserPromptKind::NewFile => {
            open_file_in_browser_with_options(client, state, target_abs, true).await?;
        }
        FileBrowserPromptKind::NewDirectory => {
            let result = client
                .rpc::<DirectoryCreate>(DirectoryCreateParams { path: target_abs })
                .await?;
            // Step into the new directory.
            load_file_browser(client, state, Some(result.path)).await?;
        }
    }
    Ok(())
}

/// Switch the active buffer to the file at `path` and return to Normal mode. Subscribes a fresh
/// viewport for the new buffer; the old viewport is left to be cleaned up by the server when the
/// client disconnects (no `viewport/unsubscribe` here keeps the minimal slice minimal).
async fn open_file_in_browser(
    client: &mut Client,
    state: &mut AppState,
    abs_path: String,
) -> Result<()> {
    open_file_in_browser_with_options(client, state, abs_path, false).await
}

async fn open_file_in_browser_with_options(
    client: &mut Client,
    state: &mut AppState,
    abs_path: String,
    create_if_missing: bool,
) -> Result<()> {
    // Find a `path_index` + `relative_path` pair the server will accept. Each project path is
    // either a file or a directory; we want the directory that contains the target.
    let target = std::path::PathBuf::from(&abs_path);
    let (path_index, relative) = state
        .project_paths
        .iter()
        .enumerate()
        .find_map(|(i, p)| {
            let project_root = std::path::PathBuf::from(p);
            target
                .strip_prefix(&project_root)
                .ok()
                .map(|rel| (i as u32, rel.display().to_string()))
        })
        .ok_or_else(|| anyhow::anyhow!("file {} is outside any project path", abs_path))?;

    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            path_index: Some(path_index),
            relative_path: Some(relative.clone()),
            language: None,
            create_if_missing,
        })
        .await?;
    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: state.viewport_cols,
            rows: state.viewport_rows,
            overscan_rows: state.viewport_rows,
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: state.wrap,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
        })
        .await?;

    state.buffer_id = open.buffer_id;
    state.viewport_id = sub.viewport_id;
    state.cursor = CursorState::default();
    state.scroll_logical_line = 0;
    state.window_first_logical_line = sub.window.first_logical_line;
    state.lines = sub.window.lines;
    state.line_count = sub.window.line_count;
    state.max_scroll_logical_line = sub.window.max_scroll_logical_line;
    state.revision = open.revision;
    state.saved_revision = open.saved_revision;
    state.scroll_col = 0;
    state.pending_scroll_lines = 0;
    state.file_path = open.path.clone();
    state.file_label = relative;
    state.search = SearchState::default();
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);
    Ok(())
}

async fn handle_search_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Don't `normalize_key` here — that lowercases uppercase chars and synthesises SHIFT, which
    // is what Normal-mode keymaps want but would strip case from the literal search query.
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => abort_search(client, state).await?,
        (KeyCode::Enter, _) => commit_search(state),
        (KeyCode::Up, _) => {
            history_up(state);
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Down, _) => {
            history_down(state);
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Backspace, _) => {
            state.search.query.pop();
            state.search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            state.search.query.push(c);
            state.search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn enter_search_mode(client: &mut Client, state: &mut AppState) -> Result<()> {
    state.search.snapshot = Some(SearchSnapshot {
        cursor: state.cursor,
        scroll_logical_line: state.scroll_logical_line,
        query: std::mem::take(&mut state.search.query),
        active: state.search.active,
    });
    state.search.active = false;
    state.search.summary = None;
    state.search.history_cursor = None;
    state.search.history_draft.clear();
    state.mode = Mode::Search;
    apply_cursor_style(state.mode);
    // Clear the server-side search so highlights disappear immediately. Restored on Esc.
    let _ = client
        .rpc::<SearchClear>(SearchClearParams { buffer_id: state.buffer_id })
        .await;
    Ok(())
}

fn commit_search(state: &mut AppState) {
    state.search.snapshot = None;
    if !state.search.query.is_empty() {
        state.search.active = true;
        push_history(state, state.search.query.clone());
    } else {
        state.search.active = false;
        state.search.summary = None;
    }
    state.search.history_cursor = None;
    state.search.history_draft.clear();
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);
}

const SEARCH_HISTORY_MAX: usize = 100;

fn push_history(state: &mut AppState, query: String) {
    if query.is_empty() {
        return;
    }
    if state.search.history.last() == Some(&query) {
        return; // dedup consecutive duplicates
    }
    state.search.history.push(query);
    let overflow = state.search.history.len().saturating_sub(SEARCH_HISTORY_MAX);
    if overflow > 0 {
        state.search.history.drain(..overflow);
    }
}

fn history_up(state: &mut AppState) {
    if state.search.history.is_empty() {
        return;
    }
    let new_idx = match state.search.history_cursor {
        None => {
            state.search.history_draft = state.search.query.clone();
            state.search.history.len() - 1
        }
        Some(0) => 0,
        Some(i) => i - 1,
    };
    state.search.history_cursor = Some(new_idx);
    state.search.query = state.search.history[new_idx].clone();
}

fn history_down(state: &mut AppState) {
    match state.search.history_cursor {
        None => {} // already past the newest entry
        Some(i) if i + 1 < state.search.history.len() => {
            state.search.history_cursor = Some(i + 1);
            state.search.query = state.search.history[i + 1].clone();
        }
        Some(_) => {
            state.search.history_cursor = None;
            state.search.query = std::mem::take(&mut state.search.history_draft);
        }
    }
}

async fn abort_search(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(snap) = state.search.snapshot.take() else {
        state.mode = Mode::Normal;
        apply_cursor_style(state.mode);
        return Ok(());
    };
    // Restore the prior server-side search query (if any). Done before cursor restoration so the
    // server's view of "current_index" matches once we move the cursor back.
    if snap.active && !snap.query.is_empty() {
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.buffer_id,
                query: snap.query.clone(),
                anchor: None,
            })
            .await?;
        state.search.summary = Some(r.summary);
    } else {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams { buffer_id: state.buffer_id })
            .await;
        state.search.summary = None;
    }
    state.search.query = snap.query;
    state.search.active = snap.active;
    // Restore cursor + selection.
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.buffer_id,
            position: snap.cursor.position,
            anchor: snap.cursor.anchor,
        })
        .await?;
    state.cursor = new;
    // Restore scroll if it moved during incremental search.
    if snap.scroll_logical_line != state.scroll_logical_line {
        scroll_to(client, state, snap.scroll_logical_line).await?;
    }
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);
    Ok(())
}

/// Incremental-search step: tell the server the latest query and let it jump the cursor onto
/// the first match at-or-after where `/` was pressed. The server's response carries the new
/// cursor + summary; per-viewport highlight notifications follow asynchronously.
async fn run_incremental_search(client: &mut Client, state: &mut AppState) -> Result<()> {
    if state.search.query.is_empty() {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams { buffer_id: state.buffer_id })
            .await;
        state.search.summary = None;
        // No matches — revert the cursor to the pre-search position so the user sees where
        // they started rather than wherever the previous query stranded them.
        if let Some(snap_cursor) = state.search.snapshot.as_ref().map(|s| s.cursor) {
            if state.cursor.position != snap_cursor.position
                || state.cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.cursor = new;
            }
        }
        return Ok(());
    }
    let anchor = state
        .search
        .snapshot
        .as_ref()
        .map(|s| selection_start(&s.cursor));
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id: state.buffer_id,
            query: state.search.query.clone(),
            anchor,
        })
        .await;
    let revert_needed = match result {
        Ok(r) => {
            state.cursor = r.cursor;
            state.search.summary = Some(r.summary.clone());
            // Zero matches: revert below so a failed keystroke doesn't strand the user.
            r.summary.total == 0
        }
        Err(_) => {
            // Most commonly an invalid regex while the user is mid-type (e.g. a trailing `\`).
            // Treat it as a transient "no matches" state — empty highlights, cursor reverted,
            // a short note in the status so the user knows why their search isn't matching.
            state.search.summary = Some(SearchSummary {
                buffer_id: state.buffer_id,
                total: 0,
                truncated: false,
                current_index: 0,
            });
            state.status = "invalid regex".into();
            true
        }
    };
    if revert_needed {
        if let Some(snap_cursor) = state.search.snapshot.as_ref().map(|s| s.cursor) {
            if state.cursor.position != snap_cursor.position
                || state.cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.cursor = new;
            }
        }
    }
    Ok(())
}

fn selection_start(c: &CursorState) -> LogicalPosition {
    match c.anchor {
        Some(a) if pos_tuple(a) < pos_tuple(c.position) => a,
        _ => c.position,
    }
}

fn pos_tuple(p: LogicalPosition) -> (u32, u32) { (p.line, p.col) }

/// `Some("3/47")` when a search is active and the server says the cursor is currently on a match
/// (i.e., `current_index != 0`). The status bar only shows the counter when the cursor is
/// meaningfully "on" a result. The total gets a trailing `+` if the server truncated.
pub fn search_counter_label(state: &AppState) -> Option<String> {
    if !state.search.active {
        return None;
    }
    let summary = state.search.summary.as_ref()?;
    if summary.current_index == 0 || summary.total == 0 {
        return None;
    }
    Some(format!("{}/{}", summary.current_index, format_total(summary)))
}

fn format_total(s: &SearchSummary) -> String {
    if s.truncated { format!("{}+", s.total) } else { s.total.to_string() }
}

/// Summary line for the search prompt: "3/47", "3/10000+", or "no matches". `None` when the
/// query is empty (the bare `/` already conveys "no search yet").
pub fn search_match_count_label(state: &AppState) -> Option<String> {
    if state.search.query.is_empty() {
        return None;
    }
    let summary = state.search.summary.as_ref()?;
    if summary.total == 0 {
        return Some(String::from("no matches"));
    }
    let total = format_total(summary);
    Some(if summary.current_index == 0 {
        total
    } else {
        format!("{}/{total}", summary.current_index)
    })
}

/// Take the current selection's text, escape its regex metacharacters, and use it as the active
/// search term. The cursor stays on the original selection — `n` / `Alt-n` then cycle from there.
async fn search_from_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: BufferCopyResult = client
        .rpc::<BufferCopy>(BufferCopyParams { buffer_id: state.buffer_id, scope: CopyScope::Selection })
        .await?;
    if r.text.is_empty() {
        return Ok(());
    }
    state.search.query = regex_escape(&r.text);
    state.search.active = true;
    push_history(state, state.search.query.clone());
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id: state.buffer_id,
            query: state.search.query.clone(),
            anchor: None,
        })
        .await?;
    state.search.summary = Some(result.summary);
    // search/set with anchor=None doesn't move the cursor server-side, so state.cursor is still
    // valid (mirrors the selection that prompted the search).
    Ok(())
}

/// Escape regex metacharacters so a literal string can be embedded in the search regex. Mirrors
/// `regex::escape` (we don't pull `regex` into the TUI just for this one call).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&' | '-' | '~') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn search_cycle(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
    count: u32,
) -> Result<()> {
    if !state.search.active {
        // No active search: revive the most recent history entry server-side, then cycle.
        let Some(last) = state.search.history.last().cloned() else { return Ok(()) };
        state.search.query = last.clone();
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.buffer_id,
                query: last,
                anchor: None,
            })
            .await?;
        state.cursor = r.cursor;
        state.search.summary = Some(r.summary);
        state.search.active = true;
    }
    let summary_total = state.search.summary.as_ref().map(|s| s.total).unwrap_or(0);
    if summary_total == 0 {
        return Ok(());
    }
    for _ in 0..count.max(1) {
        let params = SearchNavParams { buffer_id: state.buffer_id };
        let result = match direction {
            Direction::Forward => client.rpc::<SearchNext>(params).await?,
            Direction::Backward => client.rpc::<SearchPrev>(params).await?,
        };
        state.cursor = result.cursor;
        state.search.summary = Some(result.summary);
    }
    Ok(())
}

async fn handle_resize(client: &mut Client, state: &mut AppState, cols: u16, rows: u16) -> Result<()> {
    let viewport_rows = rows.saturating_sub(1) as u32;
    state.viewport_cols = cols as u32;
    state.viewport_rows = viewport_rows;
    let r = client
        .rpc::<ViewportResize>(ViewportResizeParams {
            viewport_id: state.viewport_id,
            cols: cols as u32,
            rows: viewport_rows,
        })
        .await?;
    state.window_first_logical_line = r.window.first_logical_line;
    state.line_count = r.window.line_count;
    state.max_scroll_logical_line = r.window.max_scroll_logical_line;
    state.lines = r.window.lines;
    Ok(())
}

async fn move_motion(client: &mut Client, state: &mut AppState, motion: Motion, extend: bool) -> Result<()> {
    let new: CursorState = client
        .rpc::<CursorMove>(CursorMoveParams {
            buffer_id: state.buffer_id,
            motion: motion.clone(),
            extend_selection: extend,
        })
        .await?;
    state.cursor = new;
    if is_repeatable_motion(&motion) {
        state.last_motion = Some(motion);
    }
    Ok(())
}

/// Motions worth remembering for `r`/`Shift-r` repeat: those where each press makes incremental
/// progress. Absolute positions (line endpoints, buffer endpoints, goto) are excluded because
/// repeating them is a no-op.
fn is_repeatable_motion(motion: &Motion) -> bool {
    match motion {
        Motion::Char { .. }
        | Motion::Word { .. }
        | Motion::WordEnd { .. }
        | Motion::LogicalLine { .. }
        | Motion::VisualLine { .. }
        | Motion::FindChar { .. } => true,
        Motion::LineStart
        | Motion::LineEnd
        | Motion::LineFirstNonblank
        | Motion::BufferStart
        | Motion::BufferEnd
        | Motion::Goto { .. }
        | Motion::VisualLineStart { .. }
        | Motion::VisualLineEnd { .. } => false,
    }
}

async fn select_line(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
    extend: bool,
    count: u32,
) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorSelectLine>(CursorSelectLineParams {
                buffer_id: state.buffer_id,
                direction,
                extend,
            })
            .await?;
        state.cursor = new;
    }
    Ok(())
}

async fn swap_anchor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new = client
        .rpc::<CursorSwapAnchor>(CursorSwapAnchorParams { buffer_id: state.buffer_id })
        .await?;
    state.cursor = new;
    Ok(())
}

async fn motion_undo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: CursorUndoResult = client
            .rpc::<CursorUndo>(CursorUndoParams { buffer_id: state.buffer_id })
            .await?;
        let applied = r.applied;
        apply_motion_undo_result(state, r, "motion undo");
        if !applied {
            break;
        }
    }
    Ok(())
}

async fn motion_redo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: CursorUndoResult = client
            .rpc::<CursorRedo>(CursorUndoParams { buffer_id: state.buffer_id })
            .await?;
        let applied = r.applied;
        apply_motion_undo_result(state, r, "motion redo");
        if !applied {
            break;
        }
    }
    Ok(())
}

fn apply_motion_undo_result(state: &mut AppState, r: CursorUndoResult, label: &str) {
    if r.applied {
        state.cursor = r.cursor;
    } else {
        state.status = format!("nothing to {label}");
    }
}

async fn clear_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.buffer_id,
            position: state.cursor.position,
            anchor: None,
        })
        .await?;
    state.cursor = new;
    Ok(())
}

enum InsertWhere {
    /// `i` — at the cursor (or at the lower end of the selection).
    SelectionStart,
    /// `a` — after the cursor (or at the upper end of the selection).
    SelectionEnd,
    /// `Alt-i` — column 0 of the first line of the selection (or the cursor's line).
    FirstLineStart,
    /// `Alt-a` — end of the last line of the selection (or the cursor's line).
    LastLineEnd,
}

async fn enter_insert_at(client: &mut Client, state: &mut AppState, where_: InsertWhere) -> Result<()> {
    match (where_, state.cursor.anchor) {
        // `i` — start of selection (or cursor if collapsed).
        (InsertWhere::SelectionStart, Some(anchor)) => {
            let target = min_pos(state.cursor.position, anchor);
            let new = client
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id: state.buffer_id,
                    position: target,
                    anchor: None,
                })
                .await?;
            state.cursor = new;
        }
        (InsertWhere::SelectionStart, None) => {
            // Already at the right place; nothing to do.
        }

        // `a` — just *past* the selection (or one char after the cursor if collapsed). Selection
        // is inclusive, so for a forward selection the cursor char IS the last selected char;
        // "after the selection" is one char past the max position.
        (InsertWhere::SelectionEnd, anchor_opt) => {
            let max = match anchor_opt {
                Some(anchor) => max_pos(state.cursor.position, anchor),
                None => state.cursor.position,
            };
            // Park the cursor at the last-selected position (with no anchor), then step one char
            // forward — handles multi-byte chars and end-of-line transitions correctly.
            client
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id: state.buffer_id,
                    position: max,
                    anchor: None,
                })
                .await?;
            let new = client
                .rpc::<CursorMove>(CursorMoveParams {
                    buffer_id: state.buffer_id,
                    motion: Motion::Char { direction: Direction::Forward, count: 1 },
                    extend_selection: false,
                })
                .await?;
            state.cursor = new;
        }

        // `Alt-i` — start of the first line in the selection (or the cursor's line).
        (InsertWhere::FirstLineStart, anchor_opt) => {
            let first_line = match anchor_opt {
                Some(anchor) => state.cursor.position.line.min(anchor.line),
                None => state.cursor.position.line,
            };
            let new = client
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id: state.buffer_id,
                    position: LogicalPosition { line: first_line, col: 0 },
                    anchor: None,
                })
                .await?;
            state.cursor = new;
        }

        // `Alt-a` — end of the last line in the selection (server clamps the huge col).
        (InsertWhere::LastLineEnd, anchor_opt) => {
            let last_line = match anchor_opt {
                Some(anchor) => state.cursor.position.line.max(anchor.line),
                None => state.cursor.position.line,
            };
            let new = client
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id: state.buffer_id,
                    position: LogicalPosition { line: last_line, col: u32::MAX },
                    anchor: None,
                })
                .await?;
            state.cursor = new;
        }
    }
    enter_insert_mode(state);
    Ok(())
}

fn enter_insert_mode(state: &mut AppState) {
    state.mode = Mode::Insert;
    apply_cursor_style(state.mode);
}

fn leave_insert(state: &mut AppState) {
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);
}

fn min_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) <= (b.line, b.col) { a } else { b }
}

fn max_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) >= (b.line, b.col) { a } else { b }
}

async fn insert_text(client: &mut Client, state: &mut AppState, text: &str) -> Result<()> {
    insert_text_inner(client, state, text, false).await
}

async fn insert_text_inner(
    client: &mut Client,
    state: &mut AppState,
    text: &str,
    select_pasted: bool,
) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputText>(InputTextParams {
            buffer_id: state.buffer_id,
            text: text.into(),
            select_pasted,
        })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    Ok(())
}

/// Delete the current selection (or the 1-char range at the cursor when there's no anchor) and
/// enter Insert mode — the "change" operator. Server-side `apply_edit` treats the selection as
/// the edit range when an anchor exists, so a forward Char(1) motion is just a placeholder.
async fn change_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    delete_with_motion(
        client,
        state,
        Motion::Char { direction: Direction::Forward, count: 1 },
    )
    .await?;
    enter_insert_mode(state);
    Ok(())
}

async fn delete_with_motion(client: &mut Client, state: &mut AppState, motion: Motion) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputDelete>(InputDeleteParams { buffer_id: state.buffer_id, motion })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    Ok(())
}

async fn join_lines(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputJoinLines>(BufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        state.revision = r.revision;
        state.cursor = r.cursor;
    }
    Ok(())
}

async fn indent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputIndent>(BufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        state.revision = r.revision;
        state.cursor = r.cursor;
    }
    Ok(())
}

async fn dedent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputDedent>(BufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        state.revision = r.revision;
        state.cursor = r.cursor;
    }
    Ok(())
}

/// Add a blank line after the cursor's current line and drop into Insert mode at its start.
/// Implemented as: park cursor at end of current line, insert "\n", enter Insert. The newline
/// pushes the cursor onto the (now empty) line below.
async fn open_line_below(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.cursor.position.line;
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.buffer_id,
            position: LogicalPosition { line, col: u32::MAX },
            anchor: None,
        })
        .await?;
    state.cursor = new;
    insert_text(client, state, "\n").await?;
    enter_insert_mode(state);
    Ok(())
}

/// Insert a blank line *above* the cursor's current line and drop into Insert mode on it.
/// Park at col 0 of the current line, insert "\n" (which pushes the original line down a row
/// and lands the cursor at its new start), then step back up onto the freshly-blank line.
async fn open_line_above(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.cursor.position.line;
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.buffer_id,
            position: LogicalPosition { line, col: 0 },
            anchor: None,
        })
        .await?;
    state.cursor = new;
    insert_text(client, state, "\n").await?;
    move_motion(
        client,
        state,
        Motion::LogicalLine { direction: Direction::Backward, count: 1, preserve_col: false },
        false,
    )
    .await?;
    enter_insert_mode(state);
    Ok(())
}

async fn move_lines(
    client: &mut Client,
    state: &mut AppState,
    direction: VerticalDirection,
    count: u32,
) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputMoveLines>(InputMoveLinesParams {
                buffer_id: state.buffer_id,
                direction,
            })
            .await?;
        state.revision = r.revision;
        state.cursor = r.cursor;
    }
    Ok(())
}

async fn copy_to_clipboard(client: &mut Client, state: &mut AppState, scope: CopyScope) -> Result<()> {
    let r: BufferCopyResult = client
        .rpc::<BufferCopy>(BufferCopyParams { buffer_id: state.buffer_id, scope })
        .await?;
    let len = r.text.len();
    match clipboard::copy(&mut state.clipboard, r.text) {
        Ok(()) => state.status = format!("copied {len} bytes"),
        Err(e) => state.status = format!("copy failed: {e}"),
    }
    Ok(())
}

async fn cut_to_clipboard(client: &mut Client, state: &mut AppState, scope: CopyScope) -> Result<()> {
    let r: BufferCutResult = client
        .rpc::<BufferCut>(BufferCopyParams { buffer_id: state.buffer_id, scope })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    let len = r.text.len();
    match clipboard::copy(&mut state.clipboard, r.text) {
        Ok(()) => state.status = format!("cut {len} bytes"),
        Err(e) => state.status = format!("cut to clipboard failed: {e}"),
    }
    Ok(())
}

/// Normal-mode paste: insert clipboard content *before* the selection's start and select the
/// pasted text. `count` repeats the clipboard contents, so `3p` pastes three copies in a row.
async fn paste_before(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
            return Ok(());
        }
    };
    let text = text.repeat(count.max(1) as usize);
    // Collapse to the start of the selection (or stay put if no anchor).
    let start = match state.cursor.anchor {
        Some(anchor) => min_pos(state.cursor.position, anchor),
        None => state.cursor.position,
    };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.buffer_id,
            position: start,
            anchor: None,
        })
        .await?;
    state.cursor = new;
    insert_text_inner(client, state, &text, true).await
}

/// Normal-mode paste-replace: replace the current selection (or the cursor char) with the
/// clipboard content and select what was pasted. `count` repeats the clipboard contents.
async fn paste_replace(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
            return Ok(());
        }
    };
    let text = text.repeat(count.max(1) as usize);
    insert_text_inner(client, state, &text, true).await
}

/// Insert-mode paste: just insert at the cursor, no selection of the inserted text.
async fn paste_at_cursor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
            return Ok(());
        }
    };
    insert_text_inner(client, state, &text, false).await
}

async fn undo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: UndoResult = client
            .rpc::<InputUndo>(BufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        let applied = r.applied;
        apply_undo_result(state, r, "undo");
        if !applied {
            break;
        }
    }
    Ok(())
}

async fn redo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: UndoResult = client
            .rpc::<InputRedo>(BufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        let applied = r.applied;
        apply_undo_result(state, r, "redo");
        if !applied {
            break;
        }
    }
    Ok(())
}

fn apply_undo_result(state: &mut AppState, r: UndoResult, label: &str) {
    if !r.applied {
        state.status = format!("nothing to {label}");
        return;
    }
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.status = format!("{label} (rev {})", r.revision);
}

async fn save_buffer(client: &mut Client, state: &mut AppState) -> Result<()> {
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id: state.buffer_id,
            path_index: None,
            relative_path: None,
        })
        .await;
    match result {
        Ok(r) => {
            state.revision = r.revision;
            state.saved_revision = r.revision;
            state.status = format!("saved (rev {})", r.revision);
        }
        Err(e) => {
            state.status = format!("save failed: {e}");
        }
    }
    Ok(())
}

async fn ensure_cursor_in_window(client: &mut Client, state: &mut AppState) -> Result<()> {
    // Commit any pending scroll first so the visibility check below sees the user's intended
    // scroll position; otherwise we'd snap against stale state and possibly miss the snap-back.
    flush_pending_scroll(client, state).await?;

    // Horizontal dimension first — only matters when wrap is off. Adjust `scroll_col` so the
    // cursor's column is within `[scroll_col, scroll_col + viewport_cols)`. Pure client-side.
    if matches!(state.wrap, WrapMode::None) && state.viewport_cols > 0 {
        let col = state.cursor.position.col;
        if col < state.scroll_col {
            state.scroll_col = col;
        } else if col >= state.scroll_col.saturating_add(state.viewport_cols) {
            state.scroll_col = col.saturating_sub(state.viewport_cols.saturating_sub(1));
        }
    }

    let cursor_line = state.cursor.position.line;
    let top = state.scroll_logical_line;

    // Above the top: scroll up so the cursor's line is the new top.
    if cursor_line < top {
        scroll_to(client, state, cursor_line).await?;
        return Ok(());
    }

    // Below the bottom (counting *visual* rows, not logical lines): scroll the cursor's line to
    // the top. Clamp the target to `max_scroll_logical_line` so a jump to (or near) the last
    // line doesn't overscroll — `Alt-g` would otherwise put the last line at the very top of
    // an otherwise-empty viewport.
    let cursor_visible =
        ui::cursor_visual_position(state, state.viewport_rows).is_some();
    if !cursor_visible {
        let target = cursor_line.min(state.max_scroll_logical_line);
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

/// Scroll the viewport so the cursor's logical line sits at the vertical center. Clamped to
/// `max_scroll_logical_line` so jumps near EOF don't overscroll. Approximate under soft wrap —
/// the line's first visual row lands near center, which is close enough for a quick `zz`.
async fn center_cursor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let half = state.viewport_rows / 2;
    let target = state.cursor.position.line.saturating_sub(half);
    let target = target.min(state.max_scroll_logical_line);
    if target != state.scroll_logical_line {
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

async fn toggle_wrap(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new_wrap = match state.wrap {
        WrapMode::Soft => WrapMode::None,
        WrapMode::None => WrapMode::Soft,
    };
    let r = client
        .rpc::<ViewportSetWrap>(ViewportSetWrapParams { viewport_id: state.viewport_id, wrap: new_wrap })
        .await?;
    state.wrap = new_wrap;
    state.window_first_logical_line = r.window.first_logical_line;
    state.lines = r.window.lines;
    // Horizontal scroll is meaningless under soft wrap — content never overflows right.
    if matches!(new_wrap, WrapMode::Soft) {
        state.scroll_col = 0;
    }
    state.status = format!("wrap: {}", match new_wrap {
        WrapMode::Soft => "on",
        WrapMode::None => "off",
    });
    Ok(())
}

/// Accumulate a vertical-scroll delta. Doesn't touch the cursor and doesn't issue an RPC — the
/// actual `viewport/scroll` is sent when `flush_pending_scroll` runs (before the next draw, or
/// at the start of `ensure_cursor_in_window`). This lets a trackpad burst of N scroll events
/// collapse into one server round-trip.
fn scroll_lines(state: &mut AppState, delta: i64) {
    state.pending_scroll_lines = state.pending_scroll_lines.saturating_add(delta);
}

/// Apply any accumulated `pending_scroll_lines` to the server via one `viewport/scroll` call.
/// No-op if zero. Called before every draw and from inside `ensure_cursor_in_window` so the
/// cursor-visibility check sees the user's intended scroll position.
async fn flush_pending_scroll(client: &mut Client, state: &mut AppState) -> Result<()> {
    if state.pending_scroll_lines == 0 {
        return Ok(());
    }
    let delta = state.pending_scroll_lines;
    state.pending_scroll_lines = 0;
    let raw = if delta >= 0 {
        state.scroll_logical_line.saturating_add(delta as u32)
    } else {
        state.scroll_logical_line.saturating_sub((-delta) as u32)
    };
    // Server-computed: highest scroll position that still puts the buffer's last visual row at
    // the bottom of the viewport. Accounts for wrap (where one logical line can be multiple
    // visual rows).
    let target = raw.min(state.max_scroll_logical_line);
    if target == state.scroll_logical_line {
        return Ok(()); // no movement after clamping; skip the RPC
    }
    scroll_to(client, state, target).await
}

/// Scroll the viewport horizontally by `delta` columns. Only meaningful under `WrapMode::None`;
/// no-op when soft wrap is on (wrapped content never overflows right).
fn scroll_cols(state: &mut AppState, delta: i64) {
    if !matches!(state.wrap, WrapMode::None) {
        return;
    }
    state.scroll_col = if delta >= 0 {
        state.scroll_col.saturating_add(delta as u32)
    } else {
        state.scroll_col.saturating_sub((-delta) as u32)
    };
}

async fn scroll_to(client: &mut Client, state: &mut AppState, target_line: u32) -> Result<()> {
    let r = client
        .rpc::<ViewportScroll>(ViewportScrollParams {
            viewport_id: state.viewport_id,
            scroll: ScrollPosition { logical_line: target_line, sub_row: 0.0 },
        })
        .await?;
    state.scroll_logical_line = target_line;
    state.window_first_logical_line = r.window.first_logical_line;
    state.line_count = r.window.line_count;
    state.max_scroll_logical_line = r.window.max_scroll_logical_line;
    state.lines = r.window.lines;
    Ok(())
}

