//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::AppState;
use aether_protocol::cursor::CursorState;
use aether_protocol::LogicalPosition;
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
    let selection = ordered_selection(&state.cursor);

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
        let sel_on_line = selection
            .and_then(|(s, e)| selection_range_on_line(logical_line, text.len() as u32, s, e));
        lines.push(Line::from(build_spans(&text, sel_on_line, area.width)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn ordered_selection(cursor: &CursorState) -> Option<(LogicalPosition, LogicalPosition)> {
    let anchor = cursor.anchor?;
    let p = cursor.position;
    if (p.line, p.col) <= (anchor.line, anchor.col) {
        Some((p, anchor))
    } else {
        Some((anchor, p))
    }
}

fn selection_range_on_line(
    line: u32,
    line_byte_len: u32,
    sel_start: LogicalPosition,
    sel_end: LogicalPosition,
) -> Option<(u32, u32)> {
    if line < sel_start.line || line > sel_end.line {
        return None;
    }
    let start = if line == sel_start.line { sel_start.col } else { 0 };
    let end = if line == sel_end.line { sel_end.col } else { line_byte_len };
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// Truncate `text` to fit `max_chars` columns, splitting into spans on the selection boundary.
/// Reversed style highlights the selection.
fn build_spans(text: &str, sel: Option<(u32, u32)>, max_chars: u16) -> Vec<Span<'static>> {
    let truncated: String = text.chars().take(max_chars as usize).collect();

    let Some((sel_start, sel_end)) = sel else {
        return vec![Span::raw(truncated)];
    };

    let len = truncated.len();
    let s = floor_char_boundary(&truncated, (sel_start as usize).min(len));
    let e = floor_char_boundary(&truncated, (sel_end as usize).min(len));

    let normal = Style::default();
    let selected = Style::default().add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    if s > 0 {
        spans.push(Span::styled(truncated[..s].to_string(), normal));
    }
    if s < e {
        spans.push(Span::styled(truncated[s..e].to_string(), selected));
    }
    if e < truncated.len() {
        spans.push(Span::styled(truncated[e..].to_string(), normal));
    }
    spans
}

fn floor_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut last = 0;
    for (i, _) in s.char_indices() {
        if i > target {
            return last;
        }
        last = i;
    }
    last
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
