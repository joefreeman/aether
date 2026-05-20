//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::AppState;
use aether_protocol::cursor::CursorState;
use aether_protocol::viewport::Highlight;
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
        // Phase-1 wrap=none invariant: one visual row per logical line, one segment per row.
        let (text, highlights): (String, &[Highlight]) = match render.visual_rows.first() {
            Some(vr) => match vr.segments.first() {
                Some(seg) => (seg.text.clone(), seg.highlights.as_slice()),
                None => (String::new(), &[]),
            },
            None => (String::new(), &[]),
        };
        let sel_on_line = selection
            .and_then(|(s, e)| selection_range_on_line(logical_line, text.len() as u32, s, e));
        lines.push(Line::from(build_spans(&text, highlights, sel_on_line, area.width)));
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

/// Truncate `text` to fit `max_chars` columns and emit styled spans. Style at each byte is the
/// combination of the syntax-highlight color (per `highlights`) and, if that byte falls in `sel`,
/// the `REVERSED` modifier.
fn build_spans(
    text: &str,
    highlights: &[Highlight],
    sel: Option<(u32, u32)>,
    max_chars: u16,
) -> Vec<Span<'static>> {
    let truncated: String = text.chars().take(max_chars as usize).collect();
    let trunc_len = truncated.len();
    if trunc_len == 0 {
        return Vec::new();
    }

    // Build a per-byte highlight-kind table. Highlights from the server are non-overlapping.
    let mut byte_kind: Vec<Option<&str>> = vec![None; trunc_len];
    for h in highlights {
        let s = (h.start as usize).min(trunc_len);
        let e = (h.end as usize).min(trunc_len);
        for i in s..e {
            byte_kind[i] = Some(h.kind.as_str());
        }
    }

    let style_at = |byte_idx: usize| -> Style {
        let mut style = byte_kind[byte_idx].map(theme_for).unwrap_or_default();
        if let Some((s, e)) = sel {
            if byte_idx >= s as usize && byte_idx < e as usize {
                style = style.add_modifier(Modifier::REVERSED);
            }
        }
        style
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_start = 0usize;
    let mut current_style: Option<Style> = None;
    for (byte_idx, _) in truncated.char_indices() {
        let style = style_at(byte_idx);
        match current_style {
            None => {
                current_style = Some(style);
                current_start = byte_idx;
            }
            Some(s) if s != style => {
                spans.push(Span::styled(truncated[current_start..byte_idx].to_string(), s));
                current_style = Some(style);
                current_start = byte_idx;
            }
            _ => {}
        }
    }
    if let Some(s) = current_style {
        spans.push(Span::styled(truncated[current_start..].to_string(), s));
    }
    spans
}

/// Map a tree-sitter highlight name to a `Style`. Falls back along dotted prefixes
/// (e.g. `function.macro` → `function`) before defaulting.
fn theme_for(kind: &str) -> Style {
    let mut current = kind;
    loop {
        if let Some(style) = lookup_exact(current) {
            return style;
        }
        match current.rfind('.') {
            Some(idx) => current = &current[..idx],
            None => return Style::default(),
        }
    }
}

fn lookup_exact(name: &str) -> Option<Style> {
    let s = Style::default();
    Some(match name {
        "keyword" => s.fg(Color::Yellow),
        "string" => s.fg(Color::Green),
        "string.escape" | "string.special" => s.fg(Color::LightGreen),
        "comment" => s.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        "number" | "boolean" | "constant" | "constant.builtin" => s.fg(Color::Magenta),
        "function" | "function.call" => s.fg(Color::Cyan),
        "function.macro" => s.fg(Color::LightCyan),
        "type" | "type.builtin" => s.fg(Color::Blue),
        "variable" => s,
        "variable.parameter" => s.fg(Color::LightYellow),
        "variable.builtin" => s.fg(Color::Magenta),
        "operator" => s.fg(Color::LightYellow),
        "punctuation.bracket" | "punctuation.delimiter" => s.fg(Color::Gray),
        "punctuation.special" => s.fg(Color::Magenta),
        "attribute" | "label" => s.fg(Color::LightCyan),
        "tag" => s.fg(Color::Magenta),
        "property" => s.fg(Color::LightBlue),
        // Markdown (tree-sitter-md uses these "text.*" capture names).
        "text.title" => s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        "text.literal" => s.fg(Color::Green),
        "text.uri" => s.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
        "text.reference" => s.fg(Color::Cyan),
        "text.emphasis" => s.add_modifier(Modifier::ITALIC),
        "text.strong" => s.add_modifier(Modifier::BOLD),
        _ => return None,
    })
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
