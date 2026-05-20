//! Application state and event loop.

use crate::client::Client;
use crate::ui;
use aether_protocol::buffer::BufferOpenResult;
use aether_protocol::cursor::{CursorMove, CursorMoveParams, CursorState, Direction, Motion};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::handshake::ClientHelloResult;
use aether_protocol::input::{InputDelete, InputDeleteParams, InputText, InputTextParams};
use aether_protocol::viewport::{
    LogicalLineRender, ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams,
    ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportSubscribe,
    ViewportSubscribeParams, ViewportSubscribeResult, WrapMode,
};
use aether_protocol::{BufferId, ViewportId};
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;

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
    pub revision: u64,
    pub dirty: bool,
    pub should_quit: bool,
    pub status: String,
}

pub async fn bootstrap(
    client: &mut Client,
    token: String,
    file: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<AppState> {
    // Reserve one row for the status bar.
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
            overscan_rows: viewport_rows, // simple: equal overscan
            scroll: ScrollPosition { logical_line: 0, sub_row: 0.0 },
            wrap: WrapMode::None,
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
        revision: open.revision,
        dirty: open.dirty,
        should_quit: false,
        status: String::new(),
    })
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &mut Client,
    state: &mut AppState,
) -> Result<()> {
    let mut events = EventStream::new();
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

fn apply_pending_notifications(state: &mut AppState, client: &mut Client) {
    for n in client.drain_notifications() {
        let notif = aether_protocol::envelope::Notification {
            jsonrpc: aether_protocol::envelope::JsonRpc,
            method: n.method.clone(),
            params: n.params.clone(),
        };
        apply_notification(state, notif);
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
    }
}

fn splice_lines(state: &mut AppState, p: ViewportLinesChangedParams) {
    state.revision = p.revision;
    // Compute splice indices relative to the local `lines` buffer.
    let local_start = (p.range.start_logical_line as i64) - (state.window_first_logical_line as i64);
    let local_end = (p.range.end_logical_line_exclusive as i64) - (state.window_first_logical_line as i64);
    if local_end < 0 || local_start > state.lines.len() as i64 {
        // Affected range falls outside our window — request fresh state.
        // For phase 1 we just no-op; a fuller implementation would re-subscribe.
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
    match (k.code, k.modifiers) {
        (KeyCode::Char('q' | 'c'), KeyModifiers::CONTROL) => state.should_quit = true,

        (KeyCode::Left, m) => move_cursor(client, state, Motion::Char { direction: Direction::Backward, count: 1 }, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::Right, m) => move_cursor(client, state, Motion::Char { direction: Direction::Forward, count: 1 }, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::Up, m) => move_cursor(client, state, Motion::LogicalLine { direction: Direction::Backward, count: 1, preserve_col: true }, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::Down, m) => move_cursor(client, state, Motion::LogicalLine { direction: Direction::Forward, count: 1, preserve_col: true }, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::Home, m) => move_cursor(client, state, Motion::LineStart, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::End, m) => move_cursor(client, state, Motion::LineEnd, m.contains(KeyModifiers::SHIFT)).await?,
        (KeyCode::PageDown, _) => {
            let target = state.scroll_logical_line.saturating_add(state.viewport_rows);
            scroll_to(client, state, target).await?;
        }
        (KeyCode::PageUp, _) => {
            let target = state.scroll_logical_line.saturating_sub(state.viewport_rows);
            scroll_to(client, state, target).await?;
        }

        (KeyCode::Backspace, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Backward, count: 1 }).await?,
        (KeyCode::Delete, _) => delete_with_motion(client, state, Motion::Char { direction: Direction::Forward, count: 1 }).await?,
        (KeyCode::Enter, _) => insert_text(client, state, "\n").await?,
        (KeyCode::Tab, _) => insert_text(client, state, "\t").await?,
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
            insert_text(client, state, &c.to_string()).await?;
        }

        _ => {}
    }
    ensure_cursor_in_window(client, state).await?;
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

async fn move_cursor(client: &mut Client, state: &mut AppState, motion: Motion, extend: bool) -> Result<()> {
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

async fn insert_text(client: &mut Client, state: &mut AppState, text: &str) -> Result<()> {
    let r = client
        .rpc::<InputText>(InputTextParams {
            buffer_id: state.buffer_id,
            text: text.into(),
        })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.dirty = true;
    Ok(())
}

async fn delete_with_motion(client: &mut Client, state: &mut AppState, motion: Motion) -> Result<()> {
    let r = client
        .rpc::<InputDelete>(InputDeleteParams { buffer_id: state.buffer_id, motion })
        .await?;
    state.revision = r.revision;
    state.cursor = r.cursor;
    state.dirty = true;
    Ok(())
}

/// If the cursor has scrolled out of the viewport, send a `viewport/scroll` to bring it back in.
async fn ensure_cursor_in_window(client: &mut Client, state: &mut AppState) -> Result<()> {
    let cursor_line = state.cursor.position.line;
    let top = state.scroll_logical_line;
    let bottom = top.saturating_add(state.viewport_rows);
    let new_top = if cursor_line < top {
        Some(cursor_line)
    } else if cursor_line >= bottom {
        Some(cursor_line.saturating_sub(state.viewport_rows.saturating_sub(1)))
    } else {
        None
    };
    if let Some(new_top) = new_top {
        scroll_to(client, state, new_top).await?;
    }
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

