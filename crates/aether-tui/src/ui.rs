//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::AppState;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub fn draw(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    draw_buffer(f, state, chunks[0]);
    draw_status(f, state, chunks[1]);
    place_terminal_cursor(f, state, chunks[0]);
}

fn draw_buffer(f: &mut Frame, state: &AppState, area: Rect) {
    let top = state.scroll_logical_line;
    // Render the slice of the local window that's visible. Each logical line is one visual row
    // for now (wrap = none).
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    for row in 0..area.height as u32 {
        let logical_line = top + row;
        let local_idx = (logical_line as i64) - (state.window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.lines.len() as i64 {
            lines.push(Line::from(""));
            continue;
        }
        let render = &state.lines[local_idx as usize];
        let text = render
            .visual_rows
            .first()
            .map(|r| r.segments.iter().map(|s| s.text.as_str()).collect::<String>())
            .unwrap_or_default();
        // Truncate to viewport width.
        let truncated: String = text.chars().take(area.width as usize).collect();
        lines.push(Line::from(Span::raw(truncated)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
    let dirty_marker = if state.dirty { "[+]" } else { "" };
    let main = format!(
        " {project}  {file} {dirty}  L{line} C{col}  rev {rev}",
        project = state.project_name,
        file = state.file_label,
        dirty = dirty_marker,
        line = state.cursor.position.line + 1,
        col = state.cursor.position.col + 1,
        rev = state.revision,
    );
    let status_span = if state.status.is_empty() {
        Span::raw(main)
    } else {
        Span::raw(format!("{main}    {}", state.status))
    };
    let p = Paragraph::new(Line::from(vec![status_span]))
        .style(Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(p, area);
}

fn place_terminal_cursor(f: &mut Frame, state: &AppState, buffer_area: Rect) {
    let row_offset = (state.cursor.position.line as i64) - (state.scroll_logical_line as i64);
    if row_offset < 0 || row_offset >= buffer_area.height as i64 {
        return; // cursor off-screen
    }
    let row = buffer_area.y + row_offset as u16;
    // col is bytes; for ASCII this equals display cols. Unicode handling deferred.
    let col = buffer_area.x.saturating_add((state.cursor.position.col as u16).min(buffer_area.width.saturating_sub(1)));
    f.set_cursor_position((col, row));
}
