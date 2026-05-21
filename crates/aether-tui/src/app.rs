//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use crate::client::Client;
use crate::clipboard;
use crate::ui;
use aether_protocol::buffer::{
    BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpenResult,
    BufferSave, BufferSaveParams, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo,
    CursorUndoParams, CursorUndoResult, Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::handshake::ClientHelloResult;
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputDelete, InputDeleteParams, InputJoinLines, InputRedo,
    InputText, InputTextParams, InputUndo, UndoResult,
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
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
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
    pub wrap: WrapMode,
    /// Horizontal scroll, in bytes. Only meaningful when `wrap == WrapMode::None`; reset to 0 when
    /// soft wrap is on (wrapped content never overflows the viewport horizontally). Client-only —
    /// the server doesn't know about horizontal scroll.
    pub scroll_col: u32,
    pub revision: u64,
    pub dirty: bool,
    pub should_quit: bool,
    pub status: String,
    pub mode: Mode,
    /// Digit-prefix count for the next motion. Reset after consumption.
    pub pending_count: u32,
    /// System clipboard handle. Held for the app's lifetime so the X11 selection isn't
    /// abandoned every operation. `None` if the clipboard couldn't be initialised (e.g. headless).
    pub clipboard: Option<arboard::Clipboard>,
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
            },
            f.to_string(),
        ),
        None => (
            aether_protocol::buffer::BufferOpenParams {
                path_index: None,
                relative_path: None,
                language: None,
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
        wrap: WrapMode::Soft,
        scroll_col: 0,
        revision: open.revision,
        dirty: open.dirty,
        should_quit: false,
        status: String::new(),
        mode: Mode::Normal,
        pending_count: 0,
        clipboard: clipboard::new_handle(),
    })
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &mut Client,
    state: &mut AppState,
) -> Result<()> {
    let mut events = EventStream::new();
    apply_cursor_style(state.mode);
    terminal.draw(|f| ui::draw(f, state))?;
    while !state.should_quit {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else { break };
                let ev = ev?;
                if let Event::Resize(cols, rows) = &ev {
                    handle_resize(client, state, *cols, *rows).await?;
                } else {
                    handle_event(client, state, ev).await?;
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
        terminal.draw(|f| ui::draw(f, state))?;
    }
    Ok(())
}

fn apply_cursor_style(mode: Mode) {
    let style = match mode {
        Mode::Normal => SetCursorStyle::SteadyBlock,
        Mode::Insert => SetCursorStyle::SteadyBar,
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
                state.dirty = p.dirty;
                state.revision = p.revision;
                if !p.dirty {
                    state.status = format!("saved (rev {})", p.revision);
                }
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad buffer/state params: {e}"),
        }
    }
}

fn splice_lines(state: &mut AppState, p: ViewportLinesChangedParams) {
    state.revision = p.revision;
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
    let Event::Key(k) = ev else { return Ok(()) };
    if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
        return Ok(());
    }
    match state.mode {
        Mode::Normal => handle_normal_key(client, state, k).await?,
        Mode::Insert => handle_insert_key(client, state, k).await?,
    }
    ensure_cursor_in_window(client, state).await?;
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
            // Collapse any selection by re-setting the cursor to its current position.
            if state.cursor.anchor.is_some() {
                clear_selection(client, state).await?;
            }
        }

        // ---- non-letter motions ----
        // Home/End map to logical-line start/end; PageUp/Down scroll without moving the cursor.
        (KeyCode::Home, _) => move_motion(client, state, Motion::LineStart, extend).await?,
        (KeyCode::End, _) => move_motion(client, state, Motion::LineEnd, extend).await?,
        (KeyCode::PageDown, _) => {
            let target = state.scroll_logical_line.saturating_add(state.viewport_rows);
            scroll_to(client, state, target).await?;
        }
        (KeyCode::PageUp, _) => {
            let target = state.scroll_logical_line.saturating_sub(state.viewport_rows);
            scroll_to(client, state, target).await?;
        }

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

        // ---- motions: word (w/b/e) and Alt for WORD ----
        // Forward `w` is exclusive when extending — Shift-w selects up to (but not including) the
        // start of the next word, matching the convention from vim/helix that operator-style
        // selections don't bleed into the next word.
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::Word { direction: Direction::Forward, count, boundary: WordBoundary::BigWord, exclusive: extend }, extend).await?,
        (KeyCode::Char('w'), m) if !m.contains(KeyModifiers::CONTROL) =>
            move_motion(client, state, Motion::Word { direction: Direction::Forward, count, boundary: WordBoundary::Word, exclusive: extend }, extend).await?,
        (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::Word { direction: Direction::Backward, count, boundary: WordBoundary::BigWord, exclusive: false }, extend).await?,
        (KeyCode::Char('b'), m) if !m.contains(KeyModifiers::CONTROL) =>
            move_motion(client, state, Motion::Word { direction: Direction::Backward, count, boundary: WordBoundary::Word, exclusive: false }, extend).await?,
        (KeyCode::Char('e'), m) if m.contains(KeyModifiers::ALT) =>
            move_motion(client, state, Motion::WordEnd { direction: Direction::Forward, count, boundary: WordBoundary::BigWord }, extend).await?,
        (KeyCode::Char('e'), _) =>
            move_motion(client, state, Motion::WordEnd { direction: Direction::Forward, count, boundary: WordBoundary::Word }, extend).await?,

        // ---- motions: line start ----
        (KeyCode::Char('0'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            move_motion(client, state, Motion::LineStart, extend).await?,

        // ---- line selection ----
        // `x` always grows the selection's bottom edge downward; `Alt-x` always grows the top
        // edge upward. With no selection: `x` picks the current line (or the next at end-of-line)
        // and `Alt-x` picks the previous (or the current at end-of-line). The `Shift` variants
        // keep the other edge in place (extending); the non-shift variants collapse onto a single
        // line at the moved edge. The cursor stays on whichever end (top/bottom) it was on, so
        // the bindings behave the same after `s` flips the selection direction.
        (KeyCode::Char('x'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY =>
            select_line(client, state, Direction::Forward, extend).await?,
        (KeyCode::Char('x'), m) if m.contains(KeyModifiers::ALT) =>
            select_line(client, state, Direction::Backward, extend).await?,

        // ---- selection manipulation ----
        // Swap the cursor and anchor — flips which end of the selection is the "leading" edge,
        // so a subsequent `Shift-*` motion extends from the other side.
        (KeyCode::Char('s'), m) if m == KeyModifiers::NONE => swap_anchor(client, state).await?,

        // Motion undo / redo — per-client history of cursor/selection changes, capped at the
        // last buffer mutation. Distinct from `Ctrl-z`/`Ctrl-y` which rewind buffer edits.
        (KeyCode::Char('z'), m) if m == KeyModifiers::NONE => motion_undo(client, state).await?,
        (KeyCode::Char('y'), m) if m == KeyModifiers::NONE => motion_redo(client, state).await?,

        // ---- mode transitions ----
        (KeyCode::Char('i'), m) if m == KeyModifiers::NONE => enter_insert_at(client, state, InsertWhere::SelectionStart).await?,
        (KeyCode::Char('a'), m) if m == KeyModifiers::NONE => enter_insert_at(client, state, InsertWhere::SelectionEnd).await?,
        (KeyCode::Char('i'), m) if m == ALT_ONLY => enter_insert_at(client, state, InsertWhere::FirstLineStart).await?,
        (KeyCode::Char('a'), m) if m == ALT_ONLY => enter_insert_at(client, state, InsertWhere::LastLineEnd).await?,

        // ---- viewport ----
        (KeyCode::Char('w'), CTRL_ONLY) => toggle_wrap(client, state).await?,

        // ---- edits ----
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('z'), CTRL_ONLY) => undo(client, state).await?,
        (KeyCode::Char('y'), CTRL_ONLY) => redo(client, state).await?,
        (KeyCode::Char('j'), CTRL_ONLY) => join_lines(client, state).await?,
        (KeyCode::Char('d'), CTRL_ONLY) | (KeyCode::Delete, _) => {
            delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count }).await?
        }
        (KeyCode::Backspace, _) => {
            delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count }).await?
        }

        // ---- clipboard ----
        (KeyCode::Char('c'), CTRL_ONLY) => copy_to_clipboard(client, state, CopyScope::Selection).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => cut_to_clipboard(client, state, CopyScope::Selection).await?,
        (KeyCode::Char('v'), CTRL_ONLY) => paste_before(client, state).await?,
        (KeyCode::Char('r'), CTRL_ONLY) => paste_replace(client, state).await?,

        _ => {}
    }
    Ok(())
}

async fn handle_insert_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    let (code, mods) = normalize_key(k);
    match (code, mods) {
        (KeyCode::Esc, _) => leave_insert(state),

        // Allow Ctrl-S/Z/Y to work in insert mode too.
        (KeyCode::Char('s'), CTRL_ONLY) => save_buffer(client, state).await?,
        (KeyCode::Char('z'), CTRL_ONLY) => undo(client, state).await?,
        (KeyCode::Char('y'), CTRL_ONLY) => redo(client, state).await?,

        // Clipboard: in insert mode copy/cut operate on the current line.
        (KeyCode::Char('c'), CTRL_ONLY) => copy_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => cut_to_clipboard(client, state, CopyScope::Line).await?,
        (KeyCode::Char('v'), CTRL_ONLY) => paste_at_cursor(client, state).await?,

        (KeyCode::Backspace, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }).await?,
        (KeyCode::Delete, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }).await?,
        (KeyCode::Enter, _) => insert_text(client, state, "\n").await?,
        (KeyCode::Tab, _) => insert_text(client, state, "\t").await?,
        (KeyCode::Left, _) => move_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }, false).await?,
        (KeyCode::Right, _) => move_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }, false).await?,
        (KeyCode::Up, _) => move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Up, count: 1 }, false).await?,
        (KeyCode::Down, _) => move_motion(client, state, Motion::VisualLine { viewport_id: state.viewport_id, direction: VerticalDirection::Down, count: 1 }, false).await?,

        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            insert_text(client, state, &c.to_string()).await?;
        }

        _ => {}
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
    state.lines = r.window.lines;
    Ok(())
}

async fn move_motion(client: &mut Client, state: &mut AppState, motion: Motion, extend: bool) -> Result<()> {
    let new: CursorState = client
        .rpc::<CursorMove>(CursorMoveParams {
            buffer_id: state.buffer_id,
            motion,
            extend_selection: extend,
        })
        .await?;
    state.cursor = new;
    Ok(())
}

async fn select_line(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
    extend: bool,
) -> Result<()> {
    let new = client
        .rpc::<CursorSelectLine>(CursorSelectLineParams {
            buffer_id: state.buffer_id,
            direction,
            extend,
        })
        .await?;
    state.cursor = new;
    Ok(())
}

async fn swap_anchor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new = client
        .rpc::<CursorSwapAnchor>(CursorSwapAnchorParams { buffer_id: state.buffer_id })
        .await?;
    state.cursor = new;
    Ok(())
}

async fn motion_undo(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: CursorUndoResult = client
        .rpc::<CursorUndo>(CursorUndoParams { buffer_id: state.buffer_id })
        .await?;
    apply_motion_undo_result(state, r, "motion undo");
    Ok(())
}

async fn motion_redo(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: CursorUndoResult = client
        .rpc::<CursorRedo>(CursorUndoParams { buffer_id: state.buffer_id })
        .await?;
    apply_motion_undo_result(state, r, "motion redo");
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
    state.dirty = r.dirty;
    Ok(())
}

async fn delete_with_motion(client: &mut Client, state: &mut AppState, motion: Motion) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputDelete>(InputDeleteParams { buffer_id: state.buffer_id, motion })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.dirty = r.dirty;
    Ok(())
}

async fn join_lines(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputJoinLines>(BufferOnlyParams { buffer_id: state.buffer_id })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.dirty = r.dirty;
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
    state.dirty = r.dirty;
    let len = r.text.len();
    match clipboard::copy(&mut state.clipboard, r.text) {
        Ok(()) => state.status = format!("cut {len} bytes"),
        Err(e) => state.status = format!("cut to clipboard failed: {e}"),
    }
    Ok(())
}

/// Normal-mode paste: insert clipboard content *before* the selection's start and select the
/// pasted text.
async fn paste_before(client: &mut Client, state: &mut AppState) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
            return Ok(());
        }
    };
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
/// clipboard content and select what was pasted.
async fn paste_replace(client: &mut Client, state: &mut AppState) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
            return Ok(());
        }
    };
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

async fn undo(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: UndoResult = client
        .rpc::<InputUndo>(BufferOnlyParams { buffer_id: state.buffer_id })
        .await?;
    apply_undo_result(state, r, "undo");
    Ok(())
}

async fn redo(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: UndoResult = client
        .rpc::<InputRedo>(BufferOnlyParams { buffer_id: state.buffer_id })
        .await?;
    apply_undo_result(state, r, "redo");
    Ok(())
}

fn apply_undo_result(state: &mut AppState, r: UndoResult, label: &str) {
    if !r.applied {
        state.status = format!("nothing to {label}");
        return;
    }
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.dirty = r.dirty;
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
            state.dirty = false;
            state.revision = r.revision;
            state.status = format!("saved (rev {})", r.revision);
        }
        Err(e) => {
            state.status = format!("save failed: {e}");
        }
    }
    Ok(())
}

async fn ensure_cursor_in_window(client: &mut Client, state: &mut AppState) -> Result<()> {
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
    // the top. This is a conservative heuristic — a wrapped line that's tall could push the
    // cursor's visual row past the bottom but accurate "scroll just enough to fit" would need
    // walking backward from the cursor counting visual rows, which we'd rather hand off to a
    // future refinement. Putting cursor.line at top keeps the cursor visible in all cases.
    let cursor_visible =
        ui::cursor_visual_position(state, state.viewport_rows).is_some();
    if !cursor_visible {
        scroll_to(client, state, cursor_line).await?;
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

async fn scroll_to(client: &mut Client, state: &mut AppState, target_line: u32) -> Result<()> {
    let r = client
        .rpc::<ViewportScroll>(ViewportScrollParams {
            viewport_id: state.viewport_id,
            scroll: ScrollPosition { logical_line: target_line, sub_row: 0.0 },
        })
        .await?;
    state.scroll_logical_line = target_line;
    state.window_first_logical_line = r.window.first_logical_line;
    state.lines = r.window.lines;
    Ok(())
}

