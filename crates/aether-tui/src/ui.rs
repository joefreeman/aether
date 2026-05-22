//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::{search_counter_label, search_match_count_label, AppState, Mode};
use aether_protocol::cursor::CursorState;
use aether_protocol::search::SearchMatchRange;
use aether_protocol::viewport::{Highlight, VisualRow, WrapMode};
use aether_protocol::LogicalPosition;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Glyph rendered at the start of each *continuation* row (rows after the first row of a
/// wrapped logical line) under `WrapMode::Soft`. The width (2 cols: "↪" + space) is what the
/// client tells the server to reserve in wrap math.
pub const CONTINUATION_MARKER: &str = "↪ ";
pub const CONTINUATION_MARKER_WIDTH: u32 = 2;

/// Display width of a tab character. Tabs render as spaces aligned to the next multiple of
/// this — i.e. proper tab stops, not a fixed-width substitution. Hardcoded for v1; making it
/// per-buffer (driven by `IndentStyle::Tab(width)`) is the obvious follow-up.
pub const TAB_WIDTH: u32 = 4;

/// Number of columns a character contributes when rendered at visual column `current_col`.
/// Tabs advance to the next tab stop; everything else falls back to `UnicodeWidthChar`. Used
/// by every code path that converts between byte offsets and on-screen columns.
fn char_display_width(c: char, current_col: u32) -> u32 {
    if c == '\t' {
        TAB_WIDTH - (current_col % TAB_WIDTH)
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0) as u32
    }
}

// ---- Nord palette ------------------------------------------------------------------------------
// https://www.nordtheme.com/. Used for both the syntax-highlight foreground colors and the
// painted background/status colors so the editor's appearance is independent of the terminal's
// own color scheme.

const NORD0: Color = Color::Rgb(46, 52, 64);    // Polar Night — main background
const NORD1: Color = Color::Rgb(59, 66, 82);    // Polar Night — status line / panel
const NORD2: Color = Color::Rgb(67, 76, 94);    // Polar Night — selection background
const NORD3: Color = Color::Rgb(76, 86, 106);   // Polar Night — comments / dim
const NORD4: Color = Color::Rgb(216, 222, 233); // Snow Storm — main foreground
const NORD7: Color = Color::Rgb(143, 188, 187); // Frost — types
const NORD8: Color = Color::Rgb(136, 192, 208); // Frost — functions, accents
const NORD9: Color = Color::Rgb(129, 161, 193); // Frost — keywords, operators
const NORD10: Color = Color::Rgb(94, 129, 172); // Frost — deep blue (active selection bg)
const NORD12: Color = Color::Rgb(208, 135, 112);// Aurora orange — attributes, macros
const NORD13: Color = Color::Rgb(235, 203, 139);// Aurora yellow — string escapes
const NORD14: Color = Color::Rgb(163, 190, 140);// Aurora green — strings
const NORD15: Color = Color::Rgb(180, 142, 173);// Aurora purple — numbers, constants

pub fn draw(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    if matches!(state.mode, Mode::FileBrowser) {
        draw_file_browser(f, state, chunks[0]);
    } else {
        draw_buffer(f, state, chunks[0]);
    }
    draw_status(f, state, chunks[1]);
    place_terminal_cursor(f, state, chunks[0], chunks[1]);
}

fn draw_file_browser(f: &mut Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::with_capacity(state.file_browser.entries.len());
    for (i, entry) in state.file_browser.entries.iter().enumerate() {
        let highlighted = i == state.file_browser.selected;
        let label = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        lines.push(Line::from(vec![Span::styled(
            label,
            entry_style(entry.is_dir, highlighted),
        )]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(NORD0).fg(NORD4)),
        area,
    );
}

/// Strip the longest matching project-path prefix from `abs` and return what's left (or `abs`
/// unchanged if no project path matches). Returns an empty string when the path *is* a project
/// root — the caller decides how to render that.
fn project_relative_path(abs: &str, project_paths: &[String]) -> String {
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

fn entry_style(is_dir: bool, highlighted: bool) -> Style {
    let mut style = Style::default();
    if is_dir {
        style = style.fg(NORD8);
    }
    if highlighted {
        style = style.bg(NORD2);
    }
    style
}

fn draw_buffer(f: &mut Frame, state: &AppState, area: Rect) {
    let top = state.scroll_logical_line;
    let selection = ordered_selection(&state.cursor);
    let viewport_rows = area.height as usize;
    let viewport_cols = area.width;
    // Horizontal scroll only kicks in for wrap-off; soft-wrapped content always fits horizontally.
    let scroll_col = if matches!(state.wrap, WrapMode::None) { state.scroll_col } else { 0 };
    // `selection_on_visual_row` normally omits the trailing char of the selection, expecting the
    // block cursor to overdraw it. That assumption only holds when the cursor is at the *end* of
    // the selection (forward selection). Extend the paint range when either:
    //   - we're in Search mode (the hardware cursor is on the status row, not on the buffer), or
    //   - the selection is backward (cursor at start, anchor at end — block cursor doesn't reach
    //     the trailing char, so without the extension the anchor goes unpainted).
    let backward_selection = state.cursor.anchor.is_some_and(|a| {
        (a.line, a.col) > (state.cursor.position.line, state.cursor.position.col)
    });
    let extend_sel_to_cursor = matches!(state.mode, Mode::Search) || backward_selection;

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
            let matches_on_row =
                matches_on_visual_row(vrow.byte_offset, row_text_len, &render.search_matches);

            // Apply horizontal scroll to the row's text + highlights + selection. Skips zero
            // bytes when scroll_col == 0 (the common case), so this is a no-op under soft wrap.
            let (clipped_text, clipped_highlights, clipped_sel, clipped_matches) =
                clip_horizontal(&segment.text, &segment.highlights, sel_on_row, &matches_on_row, scroll_col);

            // Continuation row when byte_offset > 0. Prepend the marker; the server already
            // reserved this width when wrapping.
            let is_continuation = vrow.byte_offset > 0;
            let marker_width = if is_continuation { CONTINUATION_MARKER_WIDTH } else { 0 };
            let indent = vrow.continuation_indent;
            let prefix_width = marker_width.saturating_add(indent).min(viewport_cols as u32) as u16;
            let body_width = viewport_cols.saturating_sub(prefix_width);

            let mut spans: Vec<Span<'static>> = Vec::new();
            if is_continuation {
                spans.push(Span::styled(
                    CONTINUATION_MARKER.to_string(),
                    Style::default().fg(NORD3),
                ));
            }
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(indent as usize)));
            }
            spans.extend(build_spans(&clipped_text, &clipped_highlights, clipped_sel, &clipped_matches, body_width, extend_sel_to_cursor));
            lines.push(Line::from(spans));
        }
        logical_line = match logical_line.checked_add(1) {
            Some(n) => n,
            None => break,
        };
    }

    // Paint the whole buffer area with the Nord base style: spans without explicit fg/bg
    // inherit it, and any empty/short visual rows get the background filled too.
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(NORD0).fg(NORD4)),
        area,
    );
}

/// Drop the first `scroll_col` bytes of the row's text, then shift highlight + selection + match
/// ranges to match the new origin. Anything fully scrolled off the left is filtered out.
fn clip_horizontal(
    text: &str,
    highlights: &[Highlight],
    sel: Option<(u32, u32)>,
    matches: &[(u32, u32)],
    scroll_col: u32,
) -> (String, Vec<Highlight>, Option<(u32, u32)>, Vec<(u32, u32)>) {
    if scroll_col == 0 {
        return (text.to_string(), highlights.to_vec(), sel, matches.to_vec());
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
    let shift_range = |(s, e): (u32, u32)| -> Option<(u32, u32)> {
        let e2 = (e as usize).saturating_sub(skip);
        if e2 == 0 {
            return None;
        }
        let s2 = (s as usize).saturating_sub(skip);
        Some((s2 as u32, e2 as u32))
    };
    let new_sel = sel.and_then(shift_range);
    let new_matches = matches.iter().copied().filter_map(shift_range).collect();
    (clipped_text, new_highlights, new_sel, new_matches)
}

/// Clip per-logical-line search match ranges (delivered by the server in `LogicalLineRender`) to
/// this visual row's byte range, returning row-relative offsets.
fn matches_on_visual_row(
    row_byte_offset: u32,
    row_text_len: u32,
    matches: &[SearchMatchRange],
) -> Vec<(u32, u32)> {
    if row_text_len == 0 {
        return Vec::new();
    }
    let row_end = row_byte_offset + row_text_len;
    matches
        .iter()
        .filter_map(|m| {
            let s = m.start.max(row_byte_offset);
            let e = m.end.min(row_end);
            if s < e {
                Some((s - row_byte_offset, e - row_byte_offset))
            } else {
                None
            }
        })
        .collect()
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
    matches: &[(u32, u32)],
    max_chars: u16,
    extend_sel_to_cursor: bool,
) -> Vec<Span<'static>> {
    let truncated: String = text.chars().take(max_chars as usize).collect();
    let trunc_len = truncated.len();
    if trunc_len == 0 {
        return Vec::new();
    }
    // When asked, grow the selection to include the cursor's own char so the paint reaches the
    // end of the match — used in Search mode where the hardware cursor lives on the status row.
    let sel = if extend_sel_to_cursor {
        sel.map(|(s, e)| {
            let e_usize = e as usize;
            let extra = truncated
                .get(e_usize..)
                .and_then(|tail| tail.chars().next())
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            (s, ((e_usize + extra).min(trunc_len)) as u32)
        })
    } else {
        sel
    };

    // Build a per-byte highlight-kind table. Highlights from the server are non-overlapping.
    let mut byte_kind: Vec<Option<&str>> = vec![None; trunc_len];
    for h in highlights {
        let s = (h.start as usize).min(trunc_len);
        let e = (h.end as usize).min(trunc_len);
        for i in s..e {
            byte_kind[i] = Some(h.kind.as_str());
        }
    }

    let mut byte_in_match: Vec<bool> = vec![false; trunc_len];
    for (s, e) in matches {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        for i in s..e {
            byte_in_match[i] = true;
        }
    }

    let style_at = |byte_idx: usize| -> Style {
        let mut style = byte_kind[byte_idx].map(theme_for).unwrap_or_default();
        // Match bg first; the active selection paints over it with a more saturated blue so the
        // selection stands out from the surrounding match highlights.
        if byte_in_match[byte_idx] {
            style = style.bg(NORD2);
        }
        if let Some((s, e)) = sel {
            if byte_idx >= s as usize && byte_idx < e as usize {
                style = style.bg(NORD10);
            }
        }
        style
    };

    // Walk char-by-char so we can substitute tabs with the right number of spaces — ratatui
    // would render a raw `\t` as a single zero-width control glyph and the rest of the line
    // would visually collapse. Track `display_col` to size each tab to the next tab stop;
    // highlight/selection byte ranges still apply to the *original* byte positions so they
    // keep working untouched.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;
    let mut display_col: u32 = 0;
    for (byte_idx, c) in truncated.char_indices() {
        let style = style_at(byte_idx);
        let pad = if c == '\t' { TAB_WIDTH - (display_col % TAB_WIDTH) } else { 0 };
        display_col += char_display_width(c, display_col);
        let rendered: std::borrow::Cow<'_, str> = if c == '\t' {
            std::borrow::Cow::Owned(" ".repeat(pad as usize))
        } else {
            std::borrow::Cow::Borrowed(&truncated[byte_idx..byte_idx + c.len_utf8()])
        };
        match current_style {
            Some(s) if s != style => {
                spans.push(Span::styled(std::mem::take(&mut current_text), s));
                current_style = Some(style);
            }
            None => current_style = Some(style),
            _ => {}
        }
        current_text.push_str(&rendered);
    }
    if let Some(s) = current_style {
        spans.push(Span::styled(current_text, s));
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
        "keyword" => s.fg(NORD9),
        "string" => s.fg(NORD14),
        "string.escape" | "string.special" => s.fg(NORD13),
        "comment" => s.fg(NORD3).add_modifier(Modifier::ITALIC),
        "number" | "boolean" | "constant" | "constant.builtin" => s.fg(NORD15),
        "function" | "function.call" => s.fg(NORD8),
        "function.macro" => s.fg(NORD12),
        "type" | "type.builtin" => s.fg(NORD7),
        "variable" => s,
        "variable.parameter" => s.fg(NORD4),
        "variable.builtin" => s.fg(NORD9),
        "operator" => s.fg(NORD9),
        "punctuation.bracket" | "punctuation.delimiter" => s.fg(NORD4),
        "punctuation.special" => s.fg(NORD12),
        "attribute" | "label" => s.fg(NORD12),
        "tag" => s.fg(NORD9),
        "property" => s.fg(NORD4),
        // Markdown (tree-sitter-md uses these "text.*" capture names).
        "text.title" => s.fg(NORD8).add_modifier(Modifier::BOLD),
        "text.literal" => s.fg(NORD14),
        "text.uri" => s.fg(NORD8).add_modifier(Modifier::UNDERLINED),
        "text.reference" => s.fg(NORD8),
        "text.emphasis" => s.add_modifier(Modifier::ITALIC),
        "text.strong" => s.add_modifier(Modifier::BOLD),
        _ => return None,
    })
}


fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
    let line = if matches!(state.mode, Mode::FileBrowser) {
        if let Some(prompt) = state.file_browser.prompt.as_ref() {
            let label = match prompt.kind {
                crate::app::FileBrowserPromptKind::NewFile => "new file",
                crate::app::FileBrowserPromptKind::NewDirectory => "new directory",
            };
            Line::from(vec![Span::raw(format!(" {label}: {}", prompt.input))])
        } else {
            let rel = project_relative_path(&state.file_browser.path, &state.project_paths);
            let suffix = if rel.is_empty() { String::new() } else { format!(" {rel}/") };
            Line::from(vec![Span::raw(format!(" [{}]{}", state.project_name, suffix))])
        }
    } else if matches!(state.mode, Mode::Search) {
        // Search-mode prompt takes over the status row. Append the live match-count summary
        // (derived from the search state at render time, not from `state.status`).
        let prompt = format!("/{}", state.search.query);
        let text = match search_match_count_label(state) {
            Some(count) => format!("{prompt}    {count}"),
            None => prompt,
        };
        Line::from(vec![Span::raw(text)])
    } else {
        let dirty_marker = if state.dirty() { "[+]" } else { "" };
        let counter = search_counter_label(state)
            .map(|c| format!("  {c}"))
            .unwrap_or_default();
        let main = format!(
            " [{project}] {file} {dirty}  {pos}{counter}",
            project = state.project_name,
            file = state.file_label,
            dirty = dirty_marker,
            pos = format_position(state),
        );
        let status_span = if state.status.is_empty() {
            Span::raw(main)
        } else {
            Span::raw(format!("{main}    {}", state.status))
        };
        Line::from(vec![status_span])
    };
    let p = Paragraph::new(line).style(Style::default().bg(NORD1).fg(NORD4));
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
        Mode::FileBrowser => String::new(),
        Mode::Normal | Mode::Search => {
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
            if start.line == end_inclusive.line {
                format!(
                    "{}:{}-{}",
                    start.line + 1,
                    start.col + 1,
                    end_inclusive.col + 2,
                )
            } else {
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
}

fn place_terminal_cursor(f: &mut Frame, state: &AppState, buffer_area: Rect, status_area: Rect) {
    if matches!(state.mode, Mode::Search) {
        // Park the terminal cursor on the status row, just past `/` + the typed query.
        let prompt_width = 1 + state.search.query.width() as u16;
        let col = status_area.x.saturating_add(prompt_width.min(status_area.width.saturating_sub(1)));
        f.set_cursor_position((col, status_area.y));
        return;
    }
    if matches!(state.mode, Mode::FileBrowser) {
        if let Some(prompt) = state.file_browser.prompt.as_ref() {
            // Cursor sits at end of the prompt input on the status row. Label width matches the
            // string built in `draw_status`: " <label>: " is `label.len() + 3` chars.
            let label_len = match prompt.kind {
                crate::app::FileBrowserPromptKind::NewFile => "new file".len(),
                crate::app::FileBrowserPromptKind::NewDirectory => "new directory".len(),
            };
            let prefix_width = (label_len + 3) as u16; // " " + label + ": "
            let col = status_area
                .x
                .saturating_add(prefix_width.saturating_add(prompt.input.width() as u16))
                .min(status_area.x.saturating_add(status_area.width.saturating_sub(1)));
            f.set_cursor_position((col, status_area.y));
            return;
        }
        // Park the cursor at the highlighted listing entry. Entries start at row 0.
        let row = buffer_area
            .y
            .saturating_add(state.file_browser.selected as u16);
        f.set_cursor_position((buffer_area.x, row));
        return;
    }
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
            // Walk chars in the row's text up to the cursor's byte offset, summing display
            // widths. The cursor lives in byte coordinates on the wire, but we render at display
            // columns — without this conversion a multi-byte char like `—` (3 bytes, 1 cell)
            // would push the cursor 2 columns past where the char visually ends.
            let row_text = row
                .segments
                .first()
                .map(|s| s.text.as_str())
                .unwrap_or("");
            let cursor_byte_in_row = cursor
                .col
                .saturating_sub(row.byte_offset)
                .min(row_text.len() as u32);
            let mut display_col_in_text: u32 = 0;
            let mut byte_cursor: usize = 0;
            for c in row_text.chars() {
                if byte_cursor >= cursor_byte_in_row as usize {
                    break;
                }
                display_col_in_text += char_display_width(c, display_col_in_text);
                byte_cursor += c.len_utf8();
            }
            let marker = if row.byte_offset > 0 { CONTINUATION_MARKER_WIDTH } else { 0 };
            let logical_visual_col = marker + row.continuation_indent + display_col_in_text;
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

/// Inverse of `cursor_visual_position`: convert a screen `(row, col)` inside the buffer area
/// (0-indexed from the top of the buffer pane) to a logical `(line, col)`. Returns `None` if the
/// click is outside the buffer pane (e.g., on the status row).
///
/// Clicks past the end of a visual row map to the end of that row's text; clicks below the last
/// rendered visual row map to the end of the buffer (the server clamps).
pub fn screen_to_logical(
    state: &AppState,
    screen_row: u16,
    screen_col: u16,
) -> Option<LogicalPosition> {
    if (screen_row as u32) >= state.viewport_rows {
        return None;
    }
    let mut rows_remaining = screen_row as u32;
    let mut logical_line = state.scroll_logical_line;
    loop {
        let local_idx = (logical_line as i64) - (state.window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.lines.len() as i64 {
            // Click is past the last line we have rendered — clamp to the end of the buffer.
            let last_line = state.line_count.saturating_sub(1);
            return Some(LogicalPosition { line: last_line, col: u32::MAX });
        }
        let render = &state.lines[local_idx as usize];
        let visual_rows_in_line = render.visual_rows.len() as u32;
        if rows_remaining < visual_rows_in_line {
            let vrow = &render.visual_rows[rows_remaining as usize];
            return Some(LogicalPosition {
                line: logical_line,
                col: byte_at_screen_col(state, vrow, screen_col),
            });
        }
        rows_remaining -= visual_rows_in_line;
        logical_line = match logical_line.checked_add(1) {
            Some(n) => n,
            None => return None,
        };
    }
}

/// Walk the visual row's text by display width to find the byte offset (within the logical line)
/// that lines up with `screen_col`. Clicks on the marker / continuation indent map to the start
/// of the row's text. Clicks past the end of the text map to the end of the text.
fn byte_at_screen_col(state: &AppState, vrow: &VisualRow, screen_col: u16) -> u32 {
    let scroll_col = if matches!(state.wrap, WrapMode::None) { state.scroll_col } else { 0 };
    let marker = if vrow.byte_offset > 0 { CONTINUATION_MARKER_WIDTH } else { 0 };
    let prefix = marker + vrow.continuation_indent;
    let target_display = (screen_col as u32).saturating_add(scroll_col);
    if target_display < prefix {
        return vrow.byte_offset;
    }
    let target_in_text = target_display - prefix;
    let text = vrow.segments.first().map(|s| s.text.as_str()).unwrap_or("");
    let mut display_col: u32 = 0;
    let mut byte: u32 = 0;
    for c in text.chars() {
        let w = char_display_width(c, display_col);
        if display_col + w > target_in_text {
            break;
        }
        display_col += w;
        byte += c.len_utf8() as u32;
    }
    vrow.byte_offset + byte
}
