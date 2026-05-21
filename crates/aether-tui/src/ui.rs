//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::{AppState, Mode};
use aether_protocol::cursor::CursorState;
use aether_protocol::viewport::{Highlight, VisualRow, WrapMode};
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
    let viewport_rows = area.height as usize;
    let viewport_cols = area.width;
    // Horizontal scroll only kicks in for wrap-off; soft-wrapped content always fits horizontally.
    let scroll_col = if matches!(state.wrap, WrapMode::None) { state.scroll_col } else { 0 };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_rows);
    let mut logical_line = top;

    'outer: loop {
        if lines.len() >= viewport_rows {
            break;
        }
        let local_idx = (logical_line as i64) - (state.window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.lines.len() as i64 {
            break;
        }
        let render = &state.lines[local_idx as usize];

        for vrow in &render.visual_rows {
            if lines.len() >= viewport_rows {
                break 'outer;
            }
            let segment = match vrow.segments.first() {
                Some(s) => s,
                None => {
                    lines.push(Line::from(""));
                    continue;
                }
            };
            let row_text_len = segment.text.len() as u32;
            let sel_on_row = selection.and_then(|(s, e)| {
                selection_on_visual_row(logical_line, vrow.byte_offset, row_text_len, s, e)
            });

            // Apply horizontal scroll to the row's text + highlights + selection. Skips zero
            // bytes when scroll_col == 0 (the common case), so this is a no-op under soft wrap.
            let (clipped_text, clipped_highlights, clipped_sel) =
                clip_horizontal(&segment.text, &segment.highlights, sel_on_row, scroll_col);

            let indent = vrow.continuation_indent.min(viewport_cols as u32) as u16;
            let body_width = viewport_cols.saturating_sub(indent);
            let mut spans: Vec<Span<'static>> = Vec::new();
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(indent as usize)));
            }
            spans.extend(build_spans(&clipped_text, &clipped_highlights, clipped_sel, body_width));
            lines.push(Line::from(spans));
        }
        logical_line = match logical_line.checked_add(1) {
            Some(n) => n,
            None => break,
        };
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Drop the first `scroll_col` bytes of the row's text, then shift highlight + selection ranges
/// to match the new origin. Anything fully scrolled off the left is filtered out.
fn clip_horizontal(
    text: &str,
    highlights: &[Highlight],
    sel: Option<(u32, u32)>,
    scroll_col: u32,
) -> (String, Vec<Highlight>, Option<(u32, u32)>) {
    if scroll_col == 0 {
        return (text.to_string(), highlights.to_vec(), sel);
    }
    let skip = scroll_col as usize;
    let clipped_text = if skip >= text.len() {
        String::new()
    } else {
        text[skip..].to_string()
    };
    let new_highlights = highlights
        .iter()
        .filter_map(|h| {
            let end = (h.end as usize).saturating_sub(skip);
            if end == 0 {
                return None;
            }
            let start = (h.start as usize).saturating_sub(skip);
            Some(Highlight { start: start as u32, end: end as u32, kind: h.kind.clone() })
        })
        .collect();
    let new_sel = sel.and_then(|(s, e)| {
        let e2 = (e as usize).saturating_sub(skip);
        if e2 == 0 {
            return None;
        }
        let s2 = (s as usize).saturating_sub(skip);
        Some((s2 as u32, e2 as u32))
    });
    (clipped_text, new_highlights, new_sel)
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

/// Intersect the selection with the byte range covered by `[row_byte_offset, +row_text_len)` on
/// `logical_line`. Returns row-relative offsets. The selection is conceptually inclusive on both
/// endpoints, but the block cursor renders the end char itself, so for the highlight we treat the
/// range as half-open and let the cursor draw its own char.
fn selection_on_visual_row(
    logical_line: u32,
    row_byte_offset: u32,
    row_text_len: u32,
    sel_start: LogicalPosition,
    sel_end: LogicalPosition,
) -> Option<(u32, u32)> {
    if logical_line < sel_start.line || logical_line > sel_end.line {
        return None;
    }
    let line_sel_start = if logical_line == sel_start.line { sel_start.col } else { 0 };
    let line_sel_end_excl = if logical_line == sel_end.line {
        sel_end.col
    } else {
        row_byte_offset + row_text_len
    };
    let row_end = row_byte_offset + row_text_len;
    let start = line_sel_start.max(row_byte_offset);
    let end = line_sel_end_excl.min(row_end);
    if start >= end {
        return None;
    }
    Some((start - row_byte_offset, end - row_byte_offset))
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
        " [{project}] {file} {dirty}  {pos}  ({rev})",
        project = state.project_name,
        file = state.file_label,
        dirty = dirty_marker,
        pos = format_position(state),
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

/// In insert mode: `A:B` (just the cursor). In normal mode: `A:B-C:D` (half-open) — A:B is the
/// first byte of the selection, C:D is one byte past the last char (the byte just after the
/// block cursor). With no explicit anchor the selection is the implicit 1-char range at the
/// cursor.
fn format_position(state: &AppState) -> String {
    let pos = state.cursor.position;
    match state.mode {
        Mode::Insert => format!("{}:{}", pos.line + 1, pos.col + 1),
        Mode::Normal => {
            let (start, end_inclusive) = match state.cursor.anchor {
                None => (pos, pos),
                Some(anchor) => {
                    if (pos.line, pos.col) <= (anchor.line, anchor.col) {
                        (pos, anchor)
                    } else {
                        (anchor, pos)
                    }
                }
            };
            // The half-open exclusive end is one byte past the block cursor's char. For phase 1
            // we approximate by incrementing the column; multi-byte chars and line-end overflow
            // would need server help to compute exactly.
            format!(
                "{}:{}-{}:{}",
                start.line + 1,
                start.col + 1,
                end_inclusive.line + 1,
                end_inclusive.col + 2,
            )
        }
    }
}

fn place_terminal_cursor(f: &mut Frame, state: &AppState, buffer_area: Rect) {
    let Some((visual_row, visual_col)) = cursor_visual_position(state, buffer_area.height as u32)
    else {
        return; // cursor off-screen
    };
    let row = buffer_area.y + visual_row as u16;
    let col = buffer_area.x.saturating_add(visual_col.min(buffer_area.width.saturating_sub(1)));
    f.set_cursor_position((col, row));
}

/// Map the cursor's logical (line, col) to (visual_row_offset_from_top_of_viewport, visual_col).
/// Returns `None` if the cursor is off-screen (above the top, below the bottom, off-screen left
/// after horizontal scroll, or its logical line hasn't been pushed into the window yet).
pub fn cursor_visual_position(state: &AppState, viewport_rows: u32) -> Option<(u16, u16)> {
    let top = state.scroll_logical_line;
    let cursor = state.cursor.position;
    if cursor.line < top {
        return None;
    }
    let scroll_col = if matches!(state.wrap, WrapMode::None) { state.scroll_col } else { 0 };

    let mut visual_offset: u32 = 0;
    for line_idx in top..=cursor.line {
        let local_idx = (line_idx as i64) - (state.window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.lines.len() as i64 {
            return None;
        }
        let render = &state.lines[local_idx as usize];
        if line_idx == cursor.line {
            let row_idx = find_row_idx_for_col(&render.visual_rows, cursor.col);
            visual_offset += row_idx as u32;
            if visual_offset >= viewport_rows {
                return None;
            }
            let row = &render.visual_rows[row_idx];
            let text_len: u32 = row.segments.iter().map(|s| s.text.len() as u32).sum();
            let col_in_text = cursor.col.saturating_sub(row.byte_offset).min(text_len);
            let logical_visual_col = row.continuation_indent + col_in_text;
            if logical_visual_col < scroll_col {
                return None; // scrolled off the left
            }
            let visual_col = logical_visual_col - scroll_col;
            return Some((visual_offset as u16, visual_col as u16));
        }
        visual_offset += render.visual_rows.len() as u32;
        if visual_offset >= viewport_rows {
            return None;
        }
    }
    None
}

/// Pick the visual row whose `byte_offset` is the largest value `<= col`. The dropped break
/// whitespace between rows maps to the end of the *preceding* row (so the cursor appears just
/// past that row's last visible character rather than at the start of the next row).
pub fn find_row_idx_for_col(rows: &[VisualRow], col: u32) -> usize {
    let mut idx = 0;
    for (i, row) in rows.iter().enumerate() {
        if row.byte_offset <= col {
            idx = i;
        } else {
            break;
        }
    }
    idx
}
