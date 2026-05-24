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
    CursorBufferOnlyParams, CursorContract, CursorExpand, CursorMove, CursorMoveParams,
    CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet, CursorSetParams, CursorState,
    CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo, CursorUndoParams, CursorUndoResult,
    Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::picker::{
    PickerHide, PickerHideParams, PickerKind, PickerQuery, PickerQueryParams, PickerSelect,
    PickerSelectParams, PickerSelectResult, PickerUpdate, PickerUpdateParams, PickerView,
    PickerViewParams,
};
use aether_protocol::handshake::ClientHelloResult;
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDedent, InputDelete, InputDeleteParams, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent, InputRedo,
    InputText, InputTextParams, InputToggleComment,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Search,
    FileBrowser,
    Picker,
    /// Status-bar prompt for `Ctrl-Alt-s` (save-as). Typed text is the project-relative target
    /// path; Enter commits, Esc returns to the prior mode without saving.
    SavePrompt,
}

/// Multi-key prefixes the next keystroke completes. `Space` is the only one for now (used by
/// the `Space f` / `Space b` picker bindings) — adding more is "add a variant and a match arm
/// in `handle_leader_key`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLeader {
    Space,
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
    pub input: crate::text_input::TextInput,
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
    pub query: crate::text_input::TextInput,
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
    /// Multi-key chord state. `Some(Space)` after the user pressed the leader key; consumed by
    /// the next keystroke. Cleared in any other code path that decides the leader doesn't apply.
    pub pending_leader: Option<PendingLeader>,
    pub picker: crate::picker::PickerState,
    /// Active save-as prompt. `Some` iff `mode == Mode::SavePrompt`. Holds the typed path and
    /// the mode to restore on Esc.
    pub save_prompt: Option<SavePromptState>,
}

#[derive(Debug, Clone)]
pub struct SavePromptState {
    pub input: crate::text_input::TextInput,
    /// Mode the user was in when the prompt opened (Normal or Insert). Restored on Esc and on
    /// successful commit, so saving from inside Insert doesn't yank the user back to Normal.
    pub return_mode: Mode,
    /// `true` after the server rejected the save with `WOULD_OVERWRITE`. The prompt switches
    /// to a y/N question; `y` resends with `overwrite: true`; `n` / Enter / Esc returns to
    /// path editing with the same input still in place. The path being confirmed is read from
    /// `input.text` — no need to store it again here.
    pub pending_overwrite: bool,
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

    // Decide whether to open the file browser. With no arg we default to the first project
    // root; with an arg, we open the browser when it resolves to an existing directory.
    // Either way the underlying buffer is a scratch. The server canonicalizes and
    // project-bounds-checks whatever path we send to `directory/list`.
    let browse_dir: Option<String> = match file {
        None => hello.project.paths.first().cloned(),
        Some(f) => {
            let raw = std::path::Path::new(f);
            let abs = if raw.is_absolute() {
                Some(raw.to_path_buf())
            } else {
                hello.project.paths.first().map(|root| std::path::Path::new(root).join(raw))
            };
            abs.filter(|p| p.is_dir()).map(|p| p.display().to_string())
        }
    };

    let (buffer_open_params, file_label) = match (file, browse_dir.is_some()) {
        (Some(f), false) => (
            aether_protocol::buffer::BufferOpenParams {
                buffer_id: None,
                path_index: Some(0),
                relative_path: Some(f.into()),
                language: None,
                create_if_missing: false,
            },
            f.to_string(),
        ),
        _ => (
            aether_protocol::buffer::BufferOpenParams {
                buffer_id: None,
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

    let initial_scroll = open.scroll.unwrap_or(ScrollPosition { logical_line: 0, sub_row: 0.0 });
    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: viewport_cols,
            rows: viewport_rows,
            overscan_rows: viewport_rows,
            scroll: initial_scroll,
            wrap: WrapMode::Soft,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
            tab_width: ui::TAB_WIDTH,
        })
        .await?;

    let mut state = AppState {
        project_name: hello.project.name,
        file_label,
        buffer_id: open.buffer_id,
        viewport_id: sub.viewport_id,
        cursor: open.cursor,
        scroll_logical_line: initial_scroll.logical_line,
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
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
    };

    if let Some(dir) = browse_dir {
        load_file_browser(client, &mut state, Some(dir)).await?;
        state.mode = Mode::FileBrowser;
    }

    Ok(state)
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
        Mode::Insert | Mode::Search | Mode::Picker | Mode::SavePrompt => SetCursorStyle::SteadyBar,
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
    } else if n.method == PickerUpdate::NAME {
        match serde_json::from_value::<PickerUpdateParams>(n.params) {
            Ok(p) => {
                state.picker.apply_update(
                    p.kind,
                    p.generation,
                    p.offset,
                    p.items,
                    p.total_matches,
                    p.total_candidates,
                    p.ticking,
                );
            }
            Err(e) => state.status = format!("bad picker/update params: {e}"),
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
    let replacement_len = p.replacement_lines.len();
    state.lines.splice(lo..hi, p.replacement_lines);
    // The server's notification covers the *current* (post-edit) viewport range. If the edit
    // shrank the buffer, the OLD `state.lines` could extend past the new range — truncate any
    // stale tail so subsequent draws never read a line that no longer exists.
    state.lines.truncate(lo + replacement_len);
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
            // Pending leader chord (e.g. `Space f`): the next key resolves the binding, no
            // matter which mode is current. The leader-trigger key itself is mode-specific (each
            // mode that wants `Space` to mean "start a chord" sets `pending_leader` itself).
            if let Some(leader) = state.pending_leader.take() {
                return handle_leader_key(client, state, leader, k).await;
            }
            match state.mode {
                Mode::Normal => handle_normal_key(client, state, k).await?,
                Mode::Insert => handle_insert_key(client, state, k).await?,
                Mode::Search => handle_search_key(client, state, k).await?,
                Mode::FileBrowser => handle_file_browser_key(client, state, k).await?,
                Mode::SavePrompt => handle_save_prompt_key(client, state, k).await?,
                Mode::Picker => handle_picker_key(client, state, k).await?,
            }
        }
        Event::Mouse(m) => {
            if matches!(state.mode, Mode::Picker) {
                handle_picker_mouse(client, state, m).await?;
            } else {
                handle_mouse_event(client, state, m).await?;
            }
        }
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

        // ---- motion: matching bracket ----
        // `m` jumps to the bracket that matches the one under (or enclosing) the cursor.
        // `Shift-m` does the same with `extend=true`, producing a selection from the original
        // position to the match — a natural "select around brackets" gesture (Vim's `v%`).
        (KeyCode::Char('m'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::MatchBracket, extend).await?,

        // ---- motions: navigation units ----
        // `]` / `[` *navigate between* per-language navigation units (function, struct, HTML
        // element, CSS rule set, etc. — see `LanguageConfig::navigation_kinds`). The cursor's
        // position implicitly determines the level: inside a method the next hit is the next
        // method in the same class; on the class header the next hit is the next top-level
        // item. Scope boundaries are *not* crossed.
        // `}` / `{` *jump to the end / start* of the enclosing navigation unit, extending the
        // selection. Use `}` on a function header to select the whole function; use `{` to
        // select from the cursor back to the function's start. Unlike `]`/`[` they don't have
        // a "next" hop, so they work on the last unit in a container too.
        (KeyCode::Char(']'), m) if m == KeyModifiers::NONE =>
            move_motion(client, state, Motion::NextNavigationUnit, false).await?,
        (KeyCode::Char('['), m) if m == KeyModifiers::NONE =>
            move_motion(client, state, Motion::PrevNavigationUnit, false).await?,
        (KeyCode::Char('}'), _) =>
            move_motion(client, state, Motion::EndOfNavigationUnit, true).await?,
        (KeyCode::Char('{'), _) =>
            move_motion(client, state, Motion::StartOfNavigationUnit, true).await?,

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

        // Tree-sitter selection expansion / contraction. `,` grows the selection to the smallest
        // enclosing syntax node; `.` reverses one step. With `N` prefix, applied N times.
        (KeyCode::Char(','), m) if m == KeyModifiers::NONE => tree_expand(client, state, count).await?,
        (KeyCode::Char('.'), m) if m == KeyModifiers::NONE => tree_contract(client, state, count).await?,

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

        // ---- buffers ----
        (KeyCode::Char('n'), CTRL_ONLY) => new_scratch(client, state).await?,

        // ---- edits ----
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('s'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            begin_save_prompt(state),
        (KeyCode::Char('u'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            redo(client, state, count).await?,
        (KeyCode::Char('u'), CTRL_ONLY) => undo(client, state, count).await?,
        (KeyCode::Char('j'), CTRL_ONLY) => move_lines(client, state, VerticalDirection::Down, count).await?,
        (KeyCode::Char('k'), CTRL_ONLY) => move_lines(client, state, VerticalDirection::Up, count).await?,
        (KeyCode::Char('g'), CTRL_ONLY) => join_lines(client, state, count).await?,
        (KeyCode::Char('l'), CTRL_ONLY) => indent(client, state, count).await?,
        (KeyCode::Char('h'), CTRL_ONLY) => dedent(client, state, count).await?,
        (KeyCode::Char('b'), CTRL_ONLY) => toggle_comment(client, state).await?,
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

        // ---- leader (Space) ----
        // `Space` starts a multi-key chord; the next keystroke selects the action. See
        // `handle_leader_key`.
        (KeyCode::Char(' '), m) if m == KeyModifiers::NONE =>
            state.pending_leader = Some(PendingLeader::Space),

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

/// Dispatch the second key of a chord opened by `state.pending_leader`. The leader itself was
/// already consumed (and taken out of `pending_leader`) by the caller; this just resolves what
/// the user wants to do.
async fn handle_leader_key(
    client: &mut Client,
    state: &mut AppState,
    leader: PendingLeader,
    k: KeyEvent,
) -> Result<()> {
    let (code, mods) = normalize_key(k);
    match (leader, code, mods) {
        // `Space f` — open the file picker, resuming the prior query + highlight if any. The
        // server treats absent state as a fresh picker, so first-ever open just works. Clear the
        // query mid-picker with `Ctrl-u`; close (retaining state) with `Esc`.
        (PendingLeader::Space, KeyCode::Char('f'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Files).await?;
        }
        // `Space b` — open the buffer picker. MRU-ordered with the current buffer at the top;
        // selecting it is a no-op switch. Useful for quickly cycling back to a recent buffer
        // without going through the file browser.
        (PendingLeader::Space, KeyCode::Char('b'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Buffers).await?;
        }
        // Esc or any other key cancels the chord without further action.
        _ => {}
    }
    Ok(())
}

/// How many result rows the picker overlay can fit, given the current buffer-area dimensions.
/// Delegates to the ui module so the box geometry stays in one place.
fn picker_limit(state: &AppState) -> u32 {
    crate::ui::picker_result_rows(state.viewport_cols, state.viewport_rows).max(1)
}

async fn open_picker(
    client: &mut Client,
    state: &mut AppState,
    kind: PickerKind,
) -> Result<()> {
    let limit = picker_limit(state);
    let center_on = state.picker.last_selected.get(&kind).cloned();
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: false,
            offset: 0,
            limit,
            center_on: center_on.clone(),
        })
        .await?;
    state.picker.open = true;
    state.picker.kind = Some(kind);
    state.picker.return_mode = state.mode;
    state.picker.query.set(view.query);
    state.picker.generation = view.generation;
    state.picker.offset = view.effective_offset;
    state.picker.limit = limit;
    state.picker.items.clear();
    state.picker.total_matches = 0;
    state.picker.total_candidates = view.total_candidates;
    state.picker.ticking = true;
    state.picker.selected = 0;
    state.picker.resume_target = center_on;
    state.mode = Mode::Picker;
    apply_cursor_style(state.mode);
    Ok(())
}

async fn handle_picker_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    // Keep query input case-sensitive (so smartcase works), so skip `normalize_key`.
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => hide_picker(client, state).await?,
        (KeyCode::Enter, _) => select_picker_item(client, state).await?,
        (KeyCode::Up, _) => picker_move_selection(client, state, -1).await?,
        (KeyCode::Down, _) => picker_move_selection(client, state, 1).await?,
        (KeyCode::PageUp, _) => {
            let step = -(state.picker.limit as i64);
            picker_move_selection(client, state, step).await?;
        }
        (KeyCode::PageDown, _) => {
            let step = state.picker.limit as i64;
            picker_move_selection(client, state, step).await?;
        }
        // `Ctrl-u` — wipe the query without leaving the picker. The currently-highlighted item
        // is preserved as the resume anchor so the cursor stays on it once the broader (empty-
        // query) result set re-pushes, rather than snapping to the top.
        (KeyCode::Char('u'), m) if m == CTRL_ONLY => {
            if !state.picker.query.is_empty() {
                let anchor = state.picker.highlighted().cloned();
                state.picker.query.clear();
                send_picker_query(client, state).await?;
                state.picker.resume_target = anchor;
            }
        }
        (KeyCode::Left, _) => state.picker.query.move_left(),
        (KeyCode::Right, _) => state.picker.query.move_right(),
        (KeyCode::Backspace, _) => {
            if !state.picker.query.is_empty() {
                state.picker.query.backspace();
                send_picker_query(client, state).await?;
            }
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            state.picker.query.insert_char(c);
            send_picker_query(client, state).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Mouse wheel inside the picker moves the highlight. Click/drag is ignored for now — the
/// picker is a keyboard-driven modal; we can wire row-click-to-select later if it feels needed.
async fn handle_picker_mouse(
    client: &mut Client,
    state: &mut AppState,
    m: MouseEvent,
) -> Result<()> {
    match m.kind {
        MouseEventKind::ScrollUp => picker_move_selection(client, state, -1).await?,
        MouseEventKind::ScrollDown => picker_move_selection(client, state, 1).await?,
        _ => {}
    }
    Ok(())
}

/// Move selection by `delta` rows. Negative = up. When the move falls off the visible window, we
/// slide the server-side window via `picker/view` so the highlighted item stays in range.
async fn picker_move_selection(
    client: &mut Client,
    state: &mut AppState,
    delta: i64,
) -> Result<()> {
    // A user-driven move cancels any pending resume re-anchor.
    state.picker.resume_target = None;

    let items_len = state.picker.items.len();
    if items_len == 0 {
        return Ok(());
    }
    let new_local = (state.picker.selected as i64 + delta).clamp(0, items_len as i64 - 1);
    state.picker.selected = new_local as usize;

    // If we're sitting at the top/bottom of the window and the user keeps pushing in that
    // direction, slide the window by requesting a fresh view at a shifted offset.
    let at_top = state.picker.selected == 0 && state.picker.offset > 0 && delta < 0;
    let at_bottom_of_window = state.picker.selected + 1 == items_len
        && (state.picker.offset as usize) + items_len < state.picker.total_matches as usize
        && delta > 0;
    if at_top || at_bottom_of_window {
        let step = delta.unsigned_abs() as u32;
        let new_offset = if delta < 0 {
            state.picker.offset.saturating_sub(step)
        } else {
            (state.picker.offset + step).min(state.picker.total_matches.saturating_sub(state.picker.limit))
        };
        if new_offset != state.picker.offset {
            request_picker_window(client, state, new_offset).await?;
        }
    }
    Ok(())
}

async fn send_picker_query(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else { return Ok(()) };
    state.picker.generation = state.picker.generation.wrapping_add(1);
    state.picker.offset = 0;
    state.picker.selected = 0;
    state.picker.ticking = true;
    // Query changes invalidate the resume anchor — the user is steering somewhere new.
    state.picker.resume_target = None;
    client
        .rpc::<PickerQuery>(PickerQueryParams {
            kind,
            query: state.picker.query.text.clone(),
            generation: state.picker.generation,
        })
        .await?;
    Ok(())
}

async fn request_picker_window(
    client: &mut Client,
    state: &mut AppState,
    new_offset: u32,
) -> Result<()> {
    let Some(kind) = state.picker.kind else { return Ok(()) };
    let limit = state.picker.limit;
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: false,
            offset: new_offset,
            limit,
            center_on: None,
        })
        .await?;
    state.picker.offset = view.effective_offset;
    // Pin selection to the edge of the window we just scrolled toward. The follow-up
    // `picker/update` push will refresh `items`; in the meantime keep the cursor where the user
    // expects it.
    if new_offset < view.effective_offset {
        // Shouldn't happen with our offset math but guard anyway.
        state.picker.selected = 0;
    }
    Ok(())
}

async fn select_picker_item(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else { return Ok(()) };
    let Some(item) = state.picker.highlighted().cloned() else { return Ok(()) };
    state.picker.last_selected.insert(kind, item.clone());

    let result = client
        .rpc::<PickerSelect>(PickerSelectParams { kind, item: item.clone() })
        .await?;
    // Implicit hide: server keeps state alive for resume, just stops pushing.
    let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
    state.picker.open = false;
    state.mode = Mode::Normal;
    apply_cursor_style(state.mode);

    match result {
        PickerSelectResult::File { path } => {
            open_file_in_browser_with_options(client, state, path, false).await?;
        }
        PickerSelectResult::Buffer { buffer_id } => {
            attach_buffer(client, state, buffer_id).await?;
        }
    }
    Ok(())
}

/// Switch to an already-open buffer by id (no path lookup; works for scratch buffers too).
/// Subscribes a fresh viewport and restores per-buffer cursor + scroll from the server. No-op
/// in the sense that the buffer's contents and per-client state already exist server-side —
/// we're just rebinding the client to it.
async fn attach_buffer(client: &mut Client, state: &mut AppState, buffer_id: BufferId) -> Result<()> {
    if buffer_id == state.buffer_id {
        // Already attached. Skip the round-trip; the picker's "current at position 0" feature
        // makes selecting the current buffer a frequent no-op.
        return Ok(());
    }
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: Some(buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
        })
        .await?;
    subscribe_to_buffer(client, state, open).await
}

/// Create a fresh scratch buffer and switch to it. Empty buffer with no path; saved as a new
/// file when the user runs Save-As. MRU bumps server-side so it shows up at position 0 in the
/// buffer picker.
async fn new_scratch(client: &mut Client, state: &mut AppState) -> Result<()> {
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
        })
        .await?;
    subscribe_to_buffer(client, state, open).await
}

/// Shared post-`buffer/open` plumbing: subscribe a viewport and refresh AppState. Both
/// `attach_buffer` and `new_scratch` route through this; the only difference is the
/// `buffer/open` params they send.
async fn subscribe_to_buffer(
    client: &mut Client,
    state: &mut AppState,
    open: BufferOpenResult,
) -> Result<()> {
    let initial_scroll = open.scroll.unwrap_or(ScrollPosition { logical_line: 0, sub_row: 0.0 });
    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: state.viewport_cols,
            rows: state.viewport_rows,
            overscan_rows: state.viewport_rows,
            scroll: initial_scroll,
            wrap: state.wrap,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
            tab_width: ui::TAB_WIDTH,
        })
        .await?;

    state.buffer_id = open.buffer_id;
    state.viewport_id = sub.viewport_id;
    state.cursor = open.cursor;
    state.scroll_logical_line = initial_scroll.logical_line;
    state.window_first_logical_line = sub.window.first_logical_line;
    state.lines = sub.window.lines;
    state.line_count = sub.window.line_count;
    state.max_scroll_logical_line = sub.window.max_scroll_logical_line;
    state.revision = open.revision;
    state.saved_revision = open.saved_revision;
    state.scroll_col = 0;
    state.pending_scroll_lines = 0;
    state.file_path = open.path.clone();
    state.file_label = match open.path.as_deref() {
        Some(p) => project_relative_label(p, &state.project_paths),
        None => format!("[scratch {}]", open.buffer_id),
    };
    state.search = SearchState::default();
    Ok(())
}

/// Strip the longest matching project path off `abs`, or fall back to the raw absolute path.
/// Mirrors the server's display rule for buffer-picker items.
fn project_relative_label(abs: &str, project_paths: &[String]) -> String {
    let abs_path = std::path::Path::new(abs);
    let best = project_paths
        .iter()
        .filter_map(|p| {
            let root = std::path::Path::new(p);
            abs_path.strip_prefix(root).ok().map(|rel| (root, rel))
        })
        .max_by_key(|(root, _)| root.as_os_str().len());
    match best {
        Some((_, rel)) => rel.display().to_string(),
        None => abs.to_string(),
    }
}

async fn hide_picker(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else { return Ok(()) };
    // Persist the highlight so the next `Space f` resumes here. Done client-side; server doesn't
    // see selection at all.
    if let Some(item) = state.picker.highlighted().cloned() {
        state.picker.last_selected.insert(kind, item);
    }
    let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
    state.picker.open = false;
    state.mode = state.picker.return_mode;
    apply_cursor_style(state.mode);
    Ok(())
}

async fn handle_insert_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    let (code, mods) = normalize_key(k);
    match (code, mods) {
        (KeyCode::Esc, _) => leave_insert(state),

        // Allow Ctrl-S / Ctrl-Alt-S / Ctrl-U / Ctrl-Alt-U to work in insert mode too.
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('s'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            begin_save_prompt(state),
        (KeyCode::Char('u'), m) if m == KeyModifiers::CONTROL | KeyModifiers::ALT =>
            redo(client, state, 1).await?,
        (KeyCode::Char('u'), CTRL_ONLY) => undo(client, state, 1).await?,

        // Clipboard: in insert mode copy/cut operate on the current line.
        (KeyCode::Char('y'), CTRL_ONLY) => copy_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => cut_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('p'), CTRL_ONLY) => paste_at_cursor(client, state).await?,

        (KeyCode::Backspace, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }).await?,
        (KeyCode::Delete, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }).await?,
        (KeyCode::Enter, _) => newline_and_indent(client, state).await?,
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
        // `Space` starts a leader chord — same set as Normal mode (e.g. `Space f` opens the
        // file picker). The chord is consumed in `handle_event` before the next key reaches
        // this handler.
        (KeyCode::Char(' '), m) if m == KeyModifiers::NONE =>
            state.pending_leader = Some(PendingLeader::Space),
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
        (KeyCode::Char('-') | KeyCode::Char('h'), m) if m == KeyModifiers::NONE => {
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
        (KeyCode::Enter, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
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
    state.file_browser.prompt = Some(FileBrowserPrompt {
        kind,
        input: crate::text_input::TextInput::default(),
    });
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
        (KeyCode::Left, _) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.move_left();
            }
        }
        (KeyCode::Right, _) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.move_right();
            }
        }
        (KeyCode::Backspace, _) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.backspace();
            }
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            if let Some(p) = state.file_browser.prompt.as_mut() {
                p.input.insert_char(c);
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
        .join(&prompt.input.text)
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
            buffer_id: None,
            path_index: Some(path_index),
            relative_path: Some(relative.clone()),
            language: None,
            create_if_missing,
        })
        .await?;
    let initial_scroll = open.scroll.unwrap_or(ScrollPosition { logical_line: 0, sub_row: 0.0 });
    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: state.viewport_cols,
            rows: state.viewport_rows,
            overscan_rows: state.viewport_rows,
            scroll: initial_scroll,
            wrap: state.wrap,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
            tab_width: ui::TAB_WIDTH,
        })
        .await?;

    state.buffer_id = open.buffer_id;
    state.viewport_id = sub.viewport_id;
    state.cursor = open.cursor;
    state.scroll_logical_line = initial_scroll.logical_line;
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
        (KeyCode::Left, _) => state.search.query.move_left(),
        (KeyCode::Right, _) => state.search.query.move_right(),
        (KeyCode::Backspace, _) => {
            state.search.query.backspace();
            state.search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            state.search.query.insert_char(c);
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
        query: state.search.query.take_text(),
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
        push_history(state, state.search.query.text.clone());
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
            state.search.history_draft = state.search.query.text.clone();
            state.search.history.len() - 1
        }
        Some(0) => 0,
        Some(i) => i - 1,
    };
    state.search.history_cursor = Some(new_idx);
    state.search.query.set(state.search.history[new_idx].clone());
}

fn history_down(state: &mut AppState) {
    match state.search.history_cursor {
        None => {} // already past the newest entry
        Some(i) if i + 1 < state.search.history.len() => {
            state.search.history_cursor = Some(i + 1);
            state.search.query.set(state.search.history[i + 1].clone());
        }
        Some(_) => {
            state.search.history_cursor = None;
            state.search.query.set(std::mem::take(&mut state.search.history_draft));
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
    state.search.query.set(snap.query);
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
            query: state.search.query.text.clone(),
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
    state.search.query.set(regex_escape(&r.text));
    state.search.active = true;
    push_history(state, state.search.query.text.clone());
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id: state.buffer_id,
            query: state.search.query.text.clone(),
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
        state.search.query.set(last.clone());
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

    // If the picker is open, the resize changed how many result rows fit. Re-subscribe with the
    // new `limit`, keeping the current `offset`. The server's next push uses the new window.
    if state.picker.open {
        if let Some(kind) = state.picker.kind {
            let new_limit = picker_limit(state);
            let view = client
                .rpc::<PickerView>(PickerViewParams {
                    kind,
                    reset: false,
                    offset: state.picker.offset,
                    limit: new_limit,
                    center_on: None,
                })
                .await?;
            state.picker.limit = new_limit;
            state.picker.offset = view.effective_offset;
        }
    }
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
        | Motion::FindChar { .. }
        | Motion::NextNavigationUnit
        | Motion::PrevNavigationUnit => true,
        Motion::LineStart
        | Motion::LineEnd
        | Motion::LineFirstNonblank
        | Motion::BufferStart
        | Motion::BufferEnd
        | Motion::Goto { .. }
        | Motion::VisualLineStart { .. }
        | Motion::VisualLineEnd { .. }
        | Motion::MatchBracket
        | Motion::EndOfNavigationUnit
        | Motion::StartOfNavigationUnit => false,
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

async fn tree_expand(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorExpand>(CursorBufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        if new == state.cursor {
            break; // already at root
        }
        state.cursor = new;
    }
    Ok(())
}

async fn tree_contract(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorContract>(CursorBufferOnlyParams { buffer_id: state.buffer_id })
            .await?;
        if new == state.cursor {
            break; // history empty
        }
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

/// Server-side smart indent: insert `\n` + indent computed from the cursor's context (current
/// line's leading whitespace, plus one level if the cursor sits right after an opening bracket
/// outside a string/comment).
async fn newline_and_indent(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputNewlineAndIndent>(BufferOnlyParams { buffer_id: state.buffer_id })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    Ok(())
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

/// Toggle line-comment status on the cursor's line (or all selected lines). Server picks the
/// prefix from the buffer language's `line_comment` and no-ops for languages without one.
async fn toggle_comment(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputToggleComment>(BufferOnlyParams { buffer_id: state.buffer_id })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    Ok(())
}

/// Add a blank line after the cursor's current line and drop into Insert mode at its start.
/// Implemented as: park cursor at end of current line, then `newline_and_indent` (which copies
/// the line's leading whitespace and adds one level if the line ends in an opener). The newline
/// pushes the cursor onto the new line at the indent column.
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
    newline_and_indent(client, state).await?;
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
    if state.file_path.is_none() {
        // Scratch buffer — no path to save to. Don't auto-prompt: the user has to be explicit
        // about creating a file with Ctrl-Alt-s. This keeps `Ctrl-s` semantics uniform: it only
        // ever writes to an already-known path.
        state.status = "scratch buffer has no path — use Ctrl-Alt-s to save as".into();
        return Ok(());
    }
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id: state.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite: false,
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

/// Open the status-bar save-as prompt. Pre-filled with the current file's project-relative
/// path so a small rename is one Backspace + a few keys; empty for scratch buffers. Cursor
/// lands at the end of the pre-fill.
fn begin_save_prompt(state: &mut AppState) {
    let initial = state
        .file_path
        .as_deref()
        .map(|p| project_relative_label(p, &state.project_paths))
        .unwrap_or_default();
    state.save_prompt = Some(SavePromptState {
        input: crate::text_input::TextInput::new(initial),
        return_mode: state.mode,
        pending_overwrite: false,
    });
    state.mode = Mode::SavePrompt;
    apply_cursor_style(state.mode);
}

async fn handle_save_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    // Don't `normalize_key` here — that lowercases uppercase chars, which would mangle paths.
    let confirming = state.save_prompt.as_ref().is_some_and(|p| p.pending_overwrite);
    if confirming {
        match (k.code, k.modifiers) {
            // Default (Enter / Esc / n) is "don't overwrite" — matching the uppercase `N` in
            // the `[y/N]` prompt. Drops the confirmation and returns to path editing so the
            // user can pick a different filename.
            (KeyCode::Esc, _) | (KeyCode::Enter, _) | (KeyCode::Char('n' | 'N'), _) => {
                if let Some(p) = state.save_prompt.as_mut() {
                    p.pending_overwrite = false;
                }
            }
            (KeyCode::Char('y' | 'Y'), _) => {
                send_save_prompt(client, state, true).await?;
            }
            _ => {}
        }
        return Ok(());
    }
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => abort_save_prompt(state),
        (KeyCode::Enter, _) => send_save_prompt(client, state, false).await?,
        (KeyCode::Left, _) => {
            if let Some(p) = state.save_prompt.as_mut() {
                p.input.move_left();
            }
        }
        (KeyCode::Right, _) => {
            if let Some(p) = state.save_prompt.as_mut() {
                p.input.move_right();
            }
        }
        (KeyCode::Backspace, _) => {
            if let Some(p) = state.save_prompt.as_mut() {
                p.input.backspace();
            }
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            if let Some(p) = state.save_prompt.as_mut() {
                p.input.insert_char(c);
            }
        }
        _ => {}
    }
    Ok(())
}

fn abort_save_prompt(state: &mut AppState) {
    if let Some(prompt) = state.save_prompt.take() {
        state.mode = prompt.return_mode;
    } else {
        state.mode = Mode::Normal;
    }
    apply_cursor_style(state.mode);
}

/// Send the save-as RPC. With `overwrite = false` we may get a `WOULD_OVERWRITE` back, in
/// which case we keep the prompt open and switch it to the y/N confirmation; with `true` we
/// resend after confirmation. On success we close the prompt and refresh local state; on any
/// other error we just stash the message in the status bar and close.
async fn send_save_prompt(
    client: &mut Client,
    state: &mut AppState,
    overwrite: bool,
) -> Result<()> {
    let (path, return_mode) = match state.save_prompt.as_ref() {
        Some(p) if !p.input.trim().is_empty() => (p.input.text.clone(), p.return_mode),
        Some(p) => {
            // Empty input — treat as cancel.
            let return_mode = p.return_mode;
            state.save_prompt = None;
            state.mode = return_mode;
            apply_cursor_style(state.mode);
            return Ok(());
        }
        None => return Ok(()),
    };

    // TODO: multi-root support — when `project_paths.len() > 1` we should let the user pick a
    // root (or accept absolute paths in the prompt and infer the root) rather than silently
    // saving under the first project root.
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id: state.buffer_id,
            path_index: Some(0),
            relative_path: Some(path.clone()),
            overwrite,
        })
        .await;
    match result {
        Ok(r) => {
            state.save_prompt = None;
            state.mode = return_mode;
            apply_cursor_style(state.mode);
            state.revision = r.revision;
            state.saved_revision = r.revision;
            state.file_label = path.clone();
            if let Some(root) = state.project_paths.first() {
                state.file_path = Some(
                    std::path::Path::new(root).join(&path).display().to_string(),
                );
            }
            state.status = format!("saved as {} (rev {})", path, r.revision);
        }
        Err(e) if is_would_overwrite(&e) => {
            // Keep the prompt open and switch to confirmation. The user's typed path stays
            // in `input.text`; the confirmation row reads it from there.
            if let Some(p) = state.save_prompt.as_mut() {
                p.pending_overwrite = true;
            }
        }
        Err(e) => {
            state.save_prompt = None;
            state.mode = return_mode;
            apply_cursor_style(state.mode);
            state.status = format!("save failed: {e}");
        }
    }
    Ok(())
}

/// True iff `e` is the server's `WOULD_OVERWRITE` JSON-RPC error. `Client::rpc` wraps server
/// errors as `RpcError` inside an anyhow chain; we downcast rather than match on the message.
fn is_would_overwrite(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::client::RpcError>()
        .is_some_and(|r| r.code == ErrorCode::WOULD_OVERWRITE.code())
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

