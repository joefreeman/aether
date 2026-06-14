//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::{
    grep_counter_label, search_counter_label, search_match_count_label, AppState, BufferStatusKind,
    EditorMode, HelpTab, BUFFER_STATUS_DOT,
};
use aether_client::keymap;
use aether_client::keymap::KeyCode;
use aether_client::markdown::{Block as MdBlock, Inline as MdInline};
use aether_protocol::cursor::CursorState;
use aether_protocol::git::{BlameInfo, GitStatus};
use aether_protocol::lsp::{LspProgress, LspStatus};
use aether_protocol::picker::{BufferDirtyState, PickerItem};
use aether_protocol::search::SearchMatchRange;
use aether_protocol::viewport::{
    DiagnosticSeverity, DiagnosticSpan, DiffMarker, DiffStage, Highlight, VisualRow, WrapMode,
};
use aether_protocol::LogicalPosition;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Glyph rendered at the start of each *continuation* row (rows after the first row of a
/// wrapped logical line) under `WrapMode::Soft`. The width (2 cols: "↪" + space) is what the
/// client tells the server to reserve in wrap math.
pub const CONTINUATION_MARKER: &str = "↪ ";
pub const CONTINUATION_MARKER_WIDTH: u32 = 2;

/// Width of the always-on left gutter (the Git change-bar column). Reserved from the content
/// width: the client subtracts it from the cols it reports to the server, so soft-wrap and all
/// the server's column math operate on the narrower content area, and the client paints the
/// gutter in the reclaimed column.
pub const GUTTER_WIDTH: u16 = 1;

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

const NORD0: Color = Color::Rgb(46, 52, 64); // Polar Night — main background
const NORD1: Color = Color::Rgb(59, 66, 82); // Polar Night — status line / panel
const NORD2: Color = Color::Rgb(67, 76, 94); // Polar Night — selection background
const NORD3: Color = Color::Rgb(76, 86, 106); // Polar Night — comments / dim
const NORD3_BRIGHT: Color = Color::Rgb(97, 110, 136); // Polar Night — lighter dim (ignored entries)
const NORD4: Color = Color::Rgb(216, 222, 233); // Snow Storm — main foreground
const NORD6: Color = Color::Rgb(236, 239, 244); // Snow Storm — brightest (headings)
const NORD7: Color = Color::Rgb(143, 188, 187); // Frost — types
const NORD8: Color = Color::Rgb(136, 192, 208); // Frost — functions, accents
const NORD9: Color = Color::Rgb(129, 161, 193); // Frost — keywords, operators
const NORD10: Color = Color::Rgb(94, 129, 172); // Frost — deep blue (active selection bg)
const NORD11: Color = Color::Rgb(191, 97, 106); // Aurora red — error text
const NORD12: Color = Color::Rgb(208, 135, 112); // Aurora orange — attributes, macros
const NORD13: Color = Color::Rgb(235, 203, 139); // Aurora yellow — string escapes
const NORD14: Color = Color::Rgb(163, 190, 140); // Aurora green — strings
const NORD15: Color = Color::Rgb(180, 142, 173); // Aurora purple — numbers, constants

// Inline diff backgrounds. Phantom "deleted" rows render red-on-dark-red so they read as removed
// without being mistaken for real buffer content; added/modified real lines get a subtle dark
// tint behind their normal syntax-highlighted text.
const GIT_DELETED_BG: Color = Color::Rgb(59, 34, 38); // dark muted red
const GIT_ADDED_BG: Color = Color::Rgb(45, 58, 45); // dark muted green
const GIT_MODIFIED_BG: Color = Color::Rgb(58, 54, 40); // dark muted olive
                                                       // Staged variants keep each kind's hue but dimmed/desaturated — hue says *what* changed,
                                                       // brightness says whether it still needs staging (bright = unstaged, muted = in the index).
const GIT_STAGED_ADDED: Color = Color::Rgb(110, 128, 96); // dimmed NORD14
const GIT_STAGED_MODIFIED: Color = Color::Rgb(158, 138, 98); // dimmed NORD13
const GIT_STAGED_DELETED: Color = Color::Rgb(132, 76, 83); // dimmed NORD11
const GIT_STAGED_ADDED_BG: Color = Color::Rgb(47, 54, 49); // staged line tints, likewise dimmer
const GIT_STAGED_MODIFIED_BG: Color = Color::Rgb(53, 52, 45);
const GIT_STAGED_DELETED_BG: Color = Color::Rgb(51, 37, 42); // staged phantom rows

// Current-line highlight (Vim's `cursorline`). A custom tint ~40% of the way from the NORD0
// background to NORD1: subtler than NORD1 (which the status line uses, so the cursorline doesn't
// read as heavy as a panel) while still clearly marking the line. Off-palette by necessity — Nord
// has no shade between NORD0 and NORD1.
const CURSOR_LINE_BG: Color = Color::Rgb(52, 58, 72);
// Cursorline variants for changed lines under the diff view: a brighter green/olive so the cursor's
// line still reads as added/modified instead of the plain blue cursorline hiding the diff tint.
const CURSOR_LINE_ADDED_BG: Color = Color::Rgb(58, 77, 58);
const CURSOR_LINE_MODIFIED_BG: Color = Color::Rgb(74, 70, 50);
// ...and their staged counterparts, lifted from the staged tints the same way — so the cursor
// landing on a staged line doesn't make it flare back up to the unstaged brightness.
const CURSOR_LINE_STAGED_ADDED_BG: Color = Color::Rgb(58, 69, 60);
const CURSOR_LINE_STAGED_MODIFIED_BG: Color = Color::Rgb(67, 65, 56);

pub fn draw(f: &mut Frame, state: &AppState) {
    // The status row carries save-as / new-file prompts and the dirty + cursor indicator for an
    // active editor. The add-root prompt lives *inside* the settings overlay, not here. Transient
    // feedback no longer lives here — it floats as a toast (see `draw_toast_overlay`) — so the row
    // is shown only for an active editor, leaving the no-project view its full vertical space.
    let show_status = state.has_editor();
    let constraints: &[Constraint] = if show_status {
        &[Constraint::Min(1), Constraint::Length(1)]
    } else {
        &[Constraint::Min(1)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    if state.has_editor() {
        draw_buffer(f, state, chunks[0]);
    } else {
        draw_no_project_view(f, state, chunks[0]);
    }
    // Hover popup (Space k): floats over the buffer, below any modal (a keypress that opens a modal
    // first dismisses hover, so they never coexist).
    if state.has_editor() && state.hover.is_some() {
        draw_hover_overlay(f, state, chunks[0]);
    }
    // A centered modal dims the content behind it so it stands out. Done once here, before any
    // overlay paints: each overlay `Clear`s and repaints its own box opaquely, so only the area
    // *behind* the dialog ends up dimmed.
    let modal_open = state.picker.open || state.project_settings.is_some() || state.help.open;
    // Status-bar prompts dim the editor too, so attention moves to the prompt: the save-as path
    // input and the y/N confirm prompts. Search is deliberately excluded — it live-highlights
    // matches in the buffer, so the editor must stay legible (and it sets neither flag below).
    let status_prompt_open = state.save_prompt.is_some() || state.confirm_prompt.is_some();
    if modal_open || status_prompt_open {
        dim_backdrop(f.buffer_mut(), chunks[0]);
    }
    // The unified picker overlay sits on top of either screen — same renderer for Files /
    // Buffers / Grep / Explorer / Projects.
    if state.picker.open {
        draw_picker_overlay(f, state, chunks[0]);
    }
    // Project settings overlay (Space P): centered modal listing the active project's roots.
    if state.project_settings.is_some() {
        draw_project_settings_overlay(f, state, chunks[0]);
    }
    if show_status {
        draw_status(f, state, chunks[1]);
    }
    // Keyboard-shortcut help (Space ?) is the topmost overlay — drawn last so it covers anything
    // underneath, and openable with or without an editor.
    if state.help.open {
        draw_help_overlay(f, state, chunks[0]);
    }
    // The settings overlay needs a caret on its input row even when no editor exists (e.g. right
    // after `project/create`). Fall back to a zero Rect for the status area in that case — the
    // settings branch in `place_terminal_cursor` doesn't read it.
    if state.has_editor() || state.project_settings.is_some() {
        let buffer_area = chunks[0];
        let status_area = chunks.get(1).copied().unwrap_or(Rect::default());
        place_terminal_cursor(f, state, buffer_area, status_area);
    }
    // Transient toasts: stacked in the bottom-right of the content area (above the status row) over
    // everything, since they're ephemeral feedback. Drawn last so a modal never hides them.
    draw_toast_overlay(f, state, chunks[0]);
}

/// Mute every cell in `area` to a faint grey on the base background — the modal backdrop. Keeps the
/// glyphs (so the content stays faintly legible) but drops their colour and emphasis, so a dialog
/// painted on top reads as the only live thing on screen.
fn dim_backdrop(buf: &mut Buffer, area: Rect) {
    let dim = Style::default()
        .fg(NORD3)
        .bg(NORD0)
        .remove_modifier(Modifier::all());
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(dim);
            }
        }
    }
}

/// Project-settings overlay. A bordered modal (no border title) holding, top-to-bottom:
/// a `Project Settings (<name>)` heading, a blank row, a `Project roots:` section label, the
/// list of roots, an always-present "Add root..." input row, and — when the last add/remove
/// attempt failed — a red error footer. Selection highlights the path text (bold + accent) on
/// root rows only; the input row carries no highlight (its terminal caret is the focus cue).
fn draw_project_settings_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(settings) = state.project_settings.as_ref() else {
        return;
    };
    let box_area = picker_box_rect(area);
    let Some(layout) = settings_layout(box_area, settings.error.is_some()) else {
        return;
    };
    f.render_widget(Clear, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(NORD4))
        .style(Style::default().bg(NORD0).fg(NORD4));
    f.render_widget(block, box_area);

    draw_settings_header(f, settings, layout.header);
    draw_settings_rows(f, state, settings, layout.rows);
    if let (Some(err_area), Some(msg)) = (layout.error, settings.error.as_deref()) {
        let style = Style::default().fg(NORD11).bg(NORD0);
        let text = truncate_right(msg, err_area.width as usize);
        f.render_widget(Paragraph::new(Span::styled(text, style)), err_area);
    }
}

/// Keyboard-shortcut help overlay (`Space ?`). A bordered, centered modal — same geometry as the
/// pickers — listing every binding grouped by context. The content is generated straight from the
/// `keymap` tables (see [`help_lines`]) so it can never drift from the actual dispatch. Read-only;
/// `state.help.scroll` pans the (possibly taller-than-the-box) content vertically.
/// Max body height (rows of text) for the hover box — beyond this it scrolls.
const HOVER_MAX_BODY: u16 = 16;
/// Horizontal padding (cols) between the hover box border and its text, each side. When a
/// scrollbar is shown it occupies the column flush against the right border, with this padding to
/// its left (so the gap sits between the text and the scrollbar, not the scrollbar and the border).
const HOVER_HPAD: u16 = 1;

/// Computed placement of the hover popup within `area`: where the box sits and how its body is laid
/// out. Shared by the renderer and the caret placement (which hides the terminal cursor when the box
/// covers it). `None` when no hover is showing or it can't fit.
struct HoverLayout {
    area: Rect,
    body_h: u16,
    text_w: u16,
    needs_scrollbar: bool,
    /// Fully-styled, width-wrapped display lines.
    lines: Vec<Line<'static>>,
}

/// The on-screen rectangle of the hover popup (border included), or `None` when no popup is showing.
/// Used by the mouse handler to hit-test clicks/wheel against the popover. Reconstructs the editor
/// area from the stored viewport size (the popup floats over the buffer, above the status row).
pub fn hover_rect(state: &AppState) -> Option<Rect> {
    let area = Rect::new(0, 0, state.viewport_cols as u16, state.viewport_rows as u16);
    hover_layout(state, area).map(|l| l.area)
}

/// Lay out the hover popup: bottom-anchored, capped at [`HOVER_MAX_BODY`] rows (taller content
/// scrolls), with the last inner column reserved for a scrollbar when it overflows.
fn hover_layout(state: &AppState, area: Rect) -> Option<HoverLayout> {
    let hover = state.hover.as_ref()?;
    let content_w = area.width.saturating_sub(2).min(80);
    let max_body = area.height.saturating_sub(2).min(HOVER_MAX_BODY);
    if content_w < 8 || max_body == 0 {
        return None;
    }
    // Text wraps inside the horizontal padding (one column reserved each side).
    let text_w_plain = content_w.saturating_sub(2 * HOVER_HPAD);
    let full = render_hover_lines(&hover.body, text_w_plain as usize);
    if full.is_empty() {
        return None;
    }
    let needs_scrollbar = full.len() as u16 > max_body;
    // With a scrollbar, it takes the column flush against the right border; the right-side padding
    // sits between the text and the scrollbar (so the text loses one more column).
    let (lines, text_w) = if needs_scrollbar {
        let w = content_w.saturating_sub(2 * HOVER_HPAD + 1);
        (render_hover_lines(&hover.body, w as usize), w)
    } else {
        (full, text_w_plain)
    };
    let body_h = (lines.len() as u16).min(max_body);
    let box_h = body_h + 2;
    Some(HoverLayout {
        area: Rect {
            x: area.x,
            y: area.bottom().saturating_sub(box_h),
            width: content_w + 2,
            height: box_h,
        },
        body_h,
        text_w,
        needs_scrollbar,
        lines,
    })
}

/// Hover popup showing the language server's hover text (or a diagnostic), anchored to the bottom of
/// the editor. Height is capped at [`HOVER_MAX_BODY`]; taller content scrolls (panned by the
/// keys/wheel handled in `app`) with a scrollbar in the last column.
fn draw_hover_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let (Some(layout), Some(hover)) = (hover_layout(state, area), state.hover.as_ref()) else {
        return;
    };
    let total = layout.lines.len() as u16;
    f.render_widget(Clear, layout.area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(hover_border_color(&hover.body)))
        .style(Style::default().bg(NORD0).fg(NORD4));
    f.render_widget(block, layout.area);
    let inner = Rect {
        x: layout.area.x + 1,
        y: layout.area.y + 1,
        width: layout.area.width - 2,
        height: layout.body_h,
    };

    hover.scroll.record(total, layout.body_h);
    let offset = hover.scroll.offset();
    // Inset the text by the left padding; the scrollbar (when shown) still sits in the last inner
    // column, flush against the right border.
    let text_area = Rect {
        x: inner.x + HOVER_HPAD,
        width: layout.text_w,
        ..inner
    };
    f.render_widget(
        Paragraph::new(layout.lines)
            .style(Style::default().bg(NORD0).fg(NORD4))
            .scroll((offset, 0)),
        text_area,
    );
    if layout.needs_scrollbar {
        let bar = Rect {
            x: inner.x + inner.width - 1,
            y: inner.y,
            width: 1,
            height: inner.height,
        };
        draw_vertical_scrollbar(f, bar, offset, total, layout.body_h);
    }
}

/// Flatten hover markdown to display lines: drop code-fence markers (```), word-wrap long lines to
/// `width`, and trim leading/trailing blank lines.
fn hover_lines(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            continue;
        }
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.extend(wrap_words(line, width));
        }
    }
    while out.first().is_some_and(String::is_empty) {
        out.remove(0);
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// Border color for the hover popup: the worst severity among its diagnostic blocks (matching the
/// gutter dot / text), or frost blue (`NORD8`) for a Markdown LSP-hover popup.
fn hover_border_color(body: &crate::app::HoverBody) -> Color {
    match body {
        crate::app::HoverBody::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.severity)
            .max_by_key(|s| severity_rank(*s))
            .map_or(NORD8, diag_color),
        crate::app::HoverBody::Markdown(_) => NORD8,
    }
}

/// Background for code (block & inline) in Markdown hovers.
const MD_CODE_BG: Color = NORD1;

/// Render a hover body to fully-styled, width-wrapped display lines. Diagnostic blocks keep their
/// severity-icon prefix and colour; Markdown is rendered with headings, code backgrounds, inline
/// emphasis, list indentation, and styled (non-clickable) links.
fn render_hover_lines(body: &crate::app::HoverBody, width: usize) -> Vec<Line<'static>> {
    match body {
        crate::app::HoverBody::Blocks(blocks) => hover_display_lines(blocks, width)
            .into_iter()
            .map(|(text, severity)| {
                let fg = severity.map_or(NORD4, diag_color);
                Line::from(Span::styled(text, Style::default().fg(fg)))
            })
            .collect(),
        crate::app::HoverBody::Markdown(blocks) => md_hover_lines(blocks, width),
    }
}

/// Render a parsed Markdown document (the shared `aether_client::markdown` AST) to styled lines,
/// wrapped to `width`. Blocks are separated by a blank line.
fn md_hover_lines(blocks: &[MdBlock], width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in blocks {
        let lines = md_block_lines(block, width);
        if lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(lines);
    }
    out
}

fn md_block_lines(block: &MdBlock, width: usize) -> Vec<Line<'static>> {
    match block {
        MdBlock::Heading { content, .. } => {
            let base = Style::default().fg(NORD6).add_modifier(Modifier::BOLD);
            let segs = md_inline_segs(content, base);
            wrap_styled(&segs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        MdBlock::Paragraph { content } => {
            let segs = md_inline_segs(content, Style::default().fg(NORD4));
            wrap_styled(&segs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        MdBlock::Code { code, .. } => {
            // Each code line gets a code background, padded out to the full width so the block reads
            // as a solid panel.
            let style = Style::default().fg(NORD4).bg(MD_CODE_BG);
            code.split('\n')
                .map(|raw| {
                    let mut s: String = raw.chars().take(width).collect();
                    let pad = width.saturating_sub(s.width());
                    if pad > 0 {
                        s.push_str(&" ".repeat(pad));
                    }
                    Line::from(Span::styled(s, style))
                })
                .collect()
        }
        MdBlock::List { ordered, items } => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", i + 1)
                } else {
                    "• ".to_string()
                };
                let indent = " ".repeat(marker.width());
                let inner_w = width.saturating_sub(marker.width());
                let item_lines = md_hover_lines(item, inner_w);
                for (j, line) in item_lines.into_iter().enumerate() {
                    // First line of the item carries the bullet/number; continuation lines hang under
                    // the text with a matching indent.
                    let prefix = if j == 0 { marker.clone() } else { indent.clone() };
                    let mut spans = vec![Span::styled(prefix, Style::default().fg(NORD4))];
                    spans.extend(line.spans);
                    out.push(Line::from(spans));
                }
            }
            out
        }
        MdBlock::Quote { content } => {
            let bar = Span::styled("│ ", Style::default().fg(NORD3));
            let inner = md_hover_lines(content, width.saturating_sub(2));
            inner
                .into_iter()
                .map(|line| {
                    let mut spans = vec![bar.clone()];
                    spans.extend(line.spans);
                    Line::from(spans)
                })
                .collect()
        }
        MdBlock::Rule => {
            vec![Line::from(Span::styled(
                "─".repeat(width),
                Style::default().fg(NORD3),
            ))]
        }
    }
}

/// Flatten inline nodes into styled `(text, style)` segments, given the base style for plain text.
fn md_inline_segs(inlines: &[MdInline], base: Style) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    md_collect_segs(inlines, base, &mut out);
    out
}

fn md_collect_segs(inlines: &[MdInline], base: Style, out: &mut Vec<(String, Style)>) {
    for inl in inlines {
        match inl {
            MdInline::Text { text } => out.push((text.clone(), base)),
            MdInline::Code { text } => {
                out.push((text.clone(), base.fg(NORD8).bg(MD_CODE_BG)));
            }
            MdInline::Strong { content } => {
                md_collect_segs(content, base.add_modifier(Modifier::BOLD), out);
            }
            MdInline::Emphasis { content } => {
                md_collect_segs(content, base.add_modifier(Modifier::ITALIC), out);
            }
            MdInline::Link { content, .. } => {
                // Terminals (ratatui's cell model) can't emit OSC 8 hyperlinks, so links are styled
                // (frost blue + underline) but not clickable.
                md_collect_segs(
                    content,
                    base.fg(NORD9).add_modifier(Modifier::UNDERLINED),
                    out,
                );
            }
        }
    }
}

/// Greedy word-wrap over styled segments, preserving per-segment styling. Words longer than `width`
/// are hard-broken. Returns one `Vec<Span>` per visual line.
fn wrap_styled(segs: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    for (text, style) in segs {
        // Split into words while keeping the whitespace runs that separate them, so we can drop a
        // space at a wrap point but keep interior spacing otherwise.
        for word in split_keep_spaces(text) {
            if word.chars().all(|c| c == ' ') {
                // Whitespace: emit only if not at the start of a line.
                if cur_w > 0 {
                    let w = word.width();
                    if cur_w + w <= width {
                        cur.push(Span::styled(word, *style));
                        cur_w += w;
                    } else {
                        lines.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                }
                continue;
            }
            let mut word = word;
            loop {
                let w = word.width();
                if cur_w + w <= width {
                    cur.push(Span::styled(word, *style));
                    cur_w += w;
                    break;
                }
                if cur_w == 0 {
                    // Word alone is wider than the line: hard-break it at the column limit and keep
                    // wrapping the remainder.
                    let (head, remainder) = break_at(&word, width);
                    cur.push(Span::styled(head, *style));
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                    if remainder.is_empty() {
                        break;
                    }
                    word = remainder;
                } else {
                    // Retry the word on a fresh line.
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Split `s` at the largest prefix whose display width is `<= width` (at least one char), returning
/// `(head, remainder)`.
fn break_at(s: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut head_w = 0;
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        let cw = c.width().unwrap_or(0);
        if !head.is_empty() && head_w + cw > width {
            break;
        }
        head.push(c);
        head_w += cw;
        chars.next();
    }
    (head, chars.collect())
}

/// Split a string into runs that are either all-spaces or all-non-spaces, preserving order.
fn split_keep_spaces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_space: Option<bool> = None;
    for c in s.chars() {
        let is_space = c == ' ';
        if in_space != Some(is_space) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            in_space = Some(is_space);
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Wrap each hover block to `width` and tag every produced line with the block's severity (for
/// coloring). Blocks are separated by a blank line; empty blocks are skipped.
fn hover_display_lines(
    blocks: &[crate::app::HoverBlock],
    width: usize,
) -> Vec<(String, Option<DiagnosticSeverity>)> {
    let mut out: Vec<(String, Option<DiagnosticSeverity>)> = Vec::new();
    for block in blocks {
        // Diagnostic blocks (those carrying a severity) get a leading severity icon on their first
        // line, matching the status-bar count and picker; reserve its 2 cols when wrapping and
        // indent continuation lines so they align under the text.
        let prefix_w = if block.severity.is_some() { 2 } else { 0 };
        let block_lines = hover_lines(&block.text, width.saturating_sub(prefix_w));
        if block_lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push((String::new(), None));
        }
        for (i, line) in block_lines.into_iter().enumerate() {
            let text = match block.severity {
                Some(sev) if i == 0 => format!("{} {line}", diag_glyph(sev)),
                Some(_) => format!("  {line}"),
                None => line,
            };
            out.push((text, block.severity));
        }
    }
    out
}

fn draw_help_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let box_area = picker_box_rect(area);
    if box_area.width < 4 || box_area.height < 4 {
        return;
    }
    f.render_widget(Clear, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(NORD4))
        .style(Style::default().bg(NORD0).fg(NORD4));
    f.render_widget(block, box_area);
    let inner = Rect {
        x: box_area.x + 1,
        y: box_area.y + 1,
        width: box_area.width - 2,
        height: box_area.height - 2,
    };
    let content = pad_horizontal(inner);
    if content.width == 0 || content.height == 0 {
        return;
    }
    // A fixed tab bar on the top row (it must stay put while the body scrolls), then the per-tab
    // body below it. When the box is too short for both, the tab bar wins and the body is dropped.
    let tab_bar = Rect {
        height: 1,
        ..content
    };
    f.render_widget(
        Paragraph::new(tab_bar_line(state.help.tab)).style(Style::default().fg(NORD4).bg(NORD0)),
        tab_bar,
    );
    if content.height < 3 {
        return;
    }
    let body = Rect {
        y: content.y + 2,
        height: content.height - 2,
        ..content
    };
    // Reserve the rightmost column for a scrollbar (plus a blank gap column before it) when the
    // tab is taller than the body (same cue as the picker). Decided from a full-width layout; if
    // the scrollbar is shown we re-wrap to the narrower text area, which can only add lines (so
    // the bar stays warranted).
    let full = help_lines(state.help.tab, body.width as usize);
    let needs_scrollbar = full.len() as u16 > body.height;
    let (lines, text_width) = if needs_scrollbar {
        let tw = body.width.saturating_sub(2);
        (help_lines(state.help.tab, tw as usize), tw)
    } else {
        (full, body.width)
    };
    // Feed the scroll state the current geometry so key/wheel handling can clamp to the real
    // bottom; then render from its clamped offset.
    let total = lines.len() as u16;
    state.help.scroll.record(total, body.height);
    let scroll = state.help.scroll.offset();
    let text_area = Rect {
        width: text_width,
        ..body
    };
    let para = Paragraph::new(lines)
        .style(Style::default().fg(NORD4).bg(NORD0))
        .scroll((scroll, 0));
    f.render_widget(para, text_area);
    if needs_scrollbar {
        let bar = Rect {
            x: body.x + body.width.saturating_sub(1),
            width: 1,
            ..body
        };
        draw_vertical_scrollbar(f, bar, scroll, total, body.height);
    }
}

/// A 1-column vertical scrollbar over `total` lines with `visible` rows shown from `offset`.
/// Thin wrapper over [`render_scrollbar`] for static overlays (help, search popover).
fn draw_vertical_scrollbar(f: &mut Frame, area: Rect, offset: u16, total: u16, visible: u16) {
    render_scrollbar(f, area, u64::from(offset), u64::from(total), u64::from(visible));
}

/// The one TUI scrollbar renderer: a 1-column track in the leftmost column of `area`, with a
/// thumb sized `visible/total` of the height and positioned at `offset/total`. Geometry comes
/// from [`aether_client::scrollbar::thumb`] (shared with the other shells); the glyphs/colours
/// (`█` thumb NORD8, `│` track NORD3) are the TUI's house style, shared by the editor pane,
/// pickers, and overlays. Draws nothing when the content fits (no [`thumb`] result).
///
/// Inputs are `u64` so the editor can pass full visual-row counts on very large files without
/// the old `u16` ceiling.
fn render_scrollbar(f: &mut Frame, area: Rect, offset: u64, total: u64, visible: u64) {
    let track_h = area.height;
    if track_h == 0 {
        return;
    }
    let Some((thumb_y, thumb_h)) = aether_client::scrollbar::thumb(
        f64::from(track_h),
        total as f64,
        visible as f64,
        offset as f64,
        1.0,
    ) else {
        return;
    };
    // Round to whole cells; the thumb is at least one cell tall by `min_len = 1.0` above.
    let thumb_y = thumb_y.round() as u16;
    let thumb_h = (thumb_h.round() as u16).max(1);

    let buf = f.buffer_mut();
    // Subtle bar: a faint `│` track whose current segment is a slightly bolder grey `┃`. Both
    // glyphs are centred in the cell, so the thumb reads as a denser stretch of one thin line
    // rather than a block punched out of it — and the thumb is a grey, not an accent, matching
    // the iced editor's theme-grey scrollbar.
    let thumb_style = Style::default().fg(NORD3_BRIGHT).bg(NORD0);
    let track_style = Style::default().fg(NORD2).bg(NORD0);
    for i in 0..track_h {
        let in_thumb = i >= thumb_y && i < thumb_y + thumb_h;
        let glyph = if in_thumb { "┃" } else { "│" };
        let style = if in_thumb { thumb_style } else { track_style };
        buf.set_string(area.x, area.y + i, glyph, style);
    }
}

/// The help overlay's tab bar: every [`HelpTab`] in display order, space-separated with no
/// dividers — the active tab is accented and underlined, the rest dimmed, so the underline (not a
/// separator glyph) carries the selection.
fn tab_bar_line(active: HelpTab) -> Line<'static> {
    let active_style = Style::default()
        .fg(NORD8)
        .add_modifier(Modifier::UNDERLINED);
    let inactive = Style::default().fg(NORD3);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, t) in HelpTab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let style = if *t == active { active_style } else { inactive };
        spans.push(Span::styled(t.label(), style));
    }
    Line::from(spans)
}

/// Bindings omitted from the help overlay. The leader *trigger* (`Space` → [`BeginLeader`]) is
/// hidden: its chords have their own Application tab, so the raw trigger would just be noise (and a
/// lone "Leader" group) on the Normal tab.
///
/// [`BeginLeader`]: keymap::Action::BeginLeader
fn help_hidden(b: &keymap::Binding) -> bool {
    matches!(b.action, keymap::Action::BeginLeader)
}

/// The keymap contexts shown on `tab`, in render order. The Normal and Insert tabs append the
/// shared `Global` (Ctrl-editing) table so each tab mirrors that mode's full dispatch fallback.
fn tab_contexts(tab: HelpTab) -> &'static [keymap::KeyContext] {
    use keymap::KeyContext as C;
    match tab {
        HelpTab::Normal => &[C::Normal, C::Global],
        HelpTab::Insert => &[C::Insert, C::Global],
        HelpTab::Search => &[C::Search],
        HelpTab::Application => &[C::Leader],
    }
}

/// Build one help *tab*'s lines from the `keymap` tables: each of the tab's contexts (see
/// [`tab_contexts`]) rendered as accent-coloured `group` headings followed by their rows. The tab
/// bar already names the mode, so contexts carry no heading of their own — the shared `Global`
/// block on the Normal/Insert tabs simply flows on as further groups. Within a sub-section, columns
/// are aligned to that section's own widths, a key's Alt variant occupies an aligned second column,
/// and descriptions word-wrap (with a hanging indent) to `width`. When a section is too narrow to
/// fit the Alt column, the Alt variant stacks on its own indented line instead.
fn help_lines(tab: HelpTab, width: usize) -> Vec<Line<'static>> {
    let heading = Style::default().fg(NORD8).add_modifier(Modifier::BOLD);
    let styles = HelpStyles {
        key: Style::default().fg(NORD9),
        desc: Style::default().fg(NORD4),
        // The `/` that joins a merged direction pair (e.g. `h / l`) renders dimmer, as a separator.
        sep: Style::default().fg(NORD3),
    };
    let w = width.max(24);

    let mut lines: Vec<Line> = Vec::new();

    // Render grouped by `group`, *merging same-named groups across the tab's contexts* — so the
    // Normal-mode `Delete` and the shared `Ctrl-d` (both "Edit") land in one section. The shared
    // (extra-context) groups render as a block after the primary context's own groups, keeping the
    // Ctrl-editing keys together. The table itself is ordered by key proximity to drive lookup, so
    // a group's rows aren't contiguous there; collecting them here keeps each heading single.
    let bindings: Vec<&'static keymap::Binding> = keymap::all().collect();
    let contexts = tab_contexts(tab);
    let primary = contexts[0];
    let extra = &contexts[1..];
    let mut done = vec![false; bindings.len()];

    // Group names from the shared (extra) contexts, in first-appearance order.
    let mut shared_groups: Vec<&'static str> = Vec::new();
    for &cx in extra {
        for b in &bindings {
            if b.ctx == cx && !help_hidden(b) && !shared_groups.contains(&b.group) {
                shared_groups.push(b.group);
            }
        }
    }
    // Primary groups that aren't also shared keep their place; the shared block follows them (a
    // primary group whose name *is* shared, like Normal's "Edit", merges into that shared section).
    let mut group_order: Vec<&'static str> = Vec::new();
    for b in &bindings {
        if b.ctx == primary
            && !help_hidden(b)
            && !shared_groups.contains(&b.group)
            && !group_order.contains(&b.group)
        {
            group_order.push(b.group);
        }
    }
    group_order.extend(shared_groups.iter().copied());

    for (gi, g) in group_order.iter().enumerate() {
        if gi > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(g.to_string(), heading)));

        // Gather the group's rows from every context in the tab (in context order), so a merged
        // group collects both the mode key and the shared Ctrl key. Alt folding (a key's same-key
        // Alt sibling, always later in the table) and direction-pair merging stay per-context —
        // they only ever pair within one context. The Leader context never folds Alt: there
        // `Space Alt-s` is a distinct chord, not a modifier variant of `Space s`.
        let mut display: Vec<DisplayRow> = Vec::new();
        for &cx in contexts {
            let fold_alt = cx != keymap::KeyContext::Leader;
            let mut rows: Vec<(&'static keymap::Binding, Option<&'static keymap::Binding>)> =
                Vec::new();
            for i in 0..bindings.len() {
                let b = bindings[i];
                if done[i] || b.ctx != cx || b.group != *g || help_hidden(b) {
                    continue;
                }
                done[i] = true;
                let sibling = fold_alt
                    .then(|| {
                        bindings.iter().enumerate().skip(i + 1).find(|(j, c)| {
                            !done[*j]
                                && c.ctx == cx
                                && c.group == b.group
                                && c.is_alt_pair(b)
                                && !help_hidden(c)
                        })
                    })
                    .flatten();
                rows.push(match sibling {
                    Some((j, c)) => {
                        done[j] = true;
                        if b.is_alt() {
                            (*c, Some(b))
                        } else {
                            (b, Some(*c))
                        }
                    }
                    None => (b, None),
                });
            }
            // Fold forward/backward direction pairs (h/l, j/k, ↑/↓, …) onto one row, merging their
            // keys and symmetric descriptions ("Char left"/"Char right" → "Char left/right").
            display.extend(build_display_rows(cx, &rows));
        }
        // Collapse aliases: two plain keys for the *same* command (e.g. `Delete` and `Ctrl-d`, both
        // "Delete selection") into one `Delete, Ctrl-d` row. Comma — not the direction-pair `/` —
        // signals "either key", not two opposite directions.
        merge_alias_rows(&mut display);

        // Column widths, computed per section so each lays out independently.
        let kw = display
            .iter()
            .map(|r| r.base.key.width())
            .max()
            .unwrap_or(0);
        // The base column spans *every* row, whether or not it has an Alt variant, so the Alt
        // column begins past the longest base description. Sizing it only to Alt-bearing rows
        // (whose descriptions can be short, e.g. `Search`/`Next match`) lets a long description
        // on an Alt-less row (`Esc  Clear the active search`) overrun the Alt column. The Alt
        // cell widths still only consider rows that actually carry an Alt variant.
        let bdw = display
            .iter()
            .map(|r| r.base.desc.width())
            .max()
            .unwrap_or(0);
        let (mut adw, mut akw, mut any_alt) = (0usize, 0usize, false);
        for r in &display {
            if let Some(a) = &r.alt {
                any_alt = true;
                adw = adw.max(a.desc.width());
                akw = akw.max(a.key.width());
            }
        }
        // The base and Alt cells share one column width so they read as two even columns. Size
        // it to content (the wider of the two natural cell widths) rather than stretching to
        // the box — that keeps the columns close together on wide terminals — but cap it at
        // half the width so two columns plus a gap always fit. Go side-by-side only when the
        // base cell fits unwrapped and the Alt cell keeps a usable description width; otherwise
        // the Alt variant stacks on its own line.
        const GAP: usize = 3;
        const MIN_ALT_DESC: usize = 10;
        let base_cell = kw + 2 + bdw;
        let alt_cell = akw + 1 + adw;
        let cap = w.saturating_sub(GAP) / 2;
        let col_w = base_cell.max(alt_cell).min(cap);
        let side_by_side = any_alt && cap >= base_cell && col_w >= akw + 1 + MIN_ALT_DESC;

        for r in &display {
            let bkey = &r.base.key;
            if let (Some(a), true) = (&r.alt, side_by_side) {
                // [ base key  base desc ]<gap>[ alt key  alt desc ] — two equal `col_w` columns.
                let base_field = col_w - kw - 2; // base desc fits unwrapped (col_w ≥ base_cell)
                let alt_desc_w = col_w - akw - 1;
                let chunks = wrap_words(&a.desc, alt_desc_w);
                // Only the key column dims its `/` separator; descriptions keep theirs in the
                // normal text colour (pass the description style as the separator style).
                let mut spans = padded_spans(bkey, kw, styles.key, styles.sep);
                spans.push(Span::raw("  "));
                spans.extend(padded_spans(
                    &r.base.desc,
                    base_field,
                    styles.desc,
                    styles.desc,
                ));
                spans.push(Span::raw(" ".repeat(GAP)));
                spans.extend(padded_spans(&a.key, akw, styles.key, styles.sep));
                spans.push(Span::raw(" "));
                spans.extend(sep_spans(&chunks[0], styles.desc, styles.desc));
                lines.push(Line::from(spans));
                let alt_desc_col = col_w + GAP + akw + 1;
                for c in &chunks[1..] {
                    let mut l = vec![Span::raw(" ".repeat(alt_desc_col))];
                    l.extend(sep_spans(c, styles.desc, styles.desc));
                    lines.push(Line::from(l));
                }
            } else {
                // Base on its own wrapped line(s); a stacked Alt (too narrow to align) indents
                // under the base description.
                push_wrapped(&mut lines, bkey, kw, &r.base.desc, w, styles);
                if let Some(a) = &r.alt {
                    let mut indented = vec![Span::raw(" ".repeat(kw + 2))];
                    let inner = wrapped_spans(&a.key, a.key.width(), &a.desc, w - (kw + 2), styles);
                    // Splice the first inner line after the indent; push the rest with indent.
                    let mut iter = inner.into_iter();
                    if let Some(first) = iter.next() {
                        indented.extend(first);
                        lines.push(Line::from(indented));
                    }
                    for rest in iter {
                        let mut l = vec![Span::raw(" ".repeat(kw + 2))];
                        l.extend(rest);
                        lines.push(Line::from(l));
                    }
                }
            }
        }
    }
    lines
}

/// The three text styles a help row is built from: the key column, the description, and the
/// dimmed separator (`/` between a merged direction pair, alias commas).
#[derive(Clone, Copy)]
struct HelpStyles {
    key: Style,
    desc: Style,
    sep: Style,
}

/// Push a `<key>  <description>` block to `lines`, word-wrapping the description to `width` with a
/// hanging indent aligned under the description column.
fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    key: &str,
    key_w: usize,
    desc: &str,
    width: usize,
    styles: HelpStyles,
) {
    let desc_col = key_w + 2;
    let chunks = wrap_words(desc, width.saturating_sub(desc_col));
    // Key column dims its `/`; the description keeps its `/` in the normal text colour.
    let mut first = padded_spans(key, key_w, styles.key, styles.sep);
    first.push(Span::raw("  "));
    first.extend(sep_spans(&chunks[0], styles.desc, styles.desc));
    lines.push(Line::from(first));
    for c in &chunks[1..] {
        let mut l = vec![Span::raw(" ".repeat(desc_col))];
        l.extend(sep_spans(c, styles.desc, styles.desc));
        lines.push(Line::from(l));
    }
}

/// Like [`push_wrapped`] but returns the span rows instead of pushing them (so a caller can add a
/// leading indent). Each returned `Vec<Span>` is one rendered line.
fn wrapped_spans(
    key: &str,
    key_w: usize,
    desc: &str,
    width: usize,
    styles: HelpStyles,
) -> Vec<Vec<Span<'static>>> {
    let desc_col = key_w + 1;
    let chunks = wrap_words(desc, width.saturating_sub(desc_col));
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    // Key column dims its `/`; the description keeps its `/` in the normal text colour.
    let mut first = padded_spans(key, key_w, styles.key, styles.sep);
    first.push(Span::raw(" "));
    first.extend(sep_spans(&chunks[0], styles.desc, styles.desc));
    out.push(first);
    for c in &chunks[1..] {
        let mut l = vec![Span::raw(" ".repeat(desc_col))];
        l.extend(sep_spans(c, styles.desc, styles.desc));
        out.push(l);
    }
    out
}

/// Render `text` as spans, dimming the row's separators with `sep` and everything else with `main`:
/// a standalone `/` token (the direction-pair separator) and a *trailing* comma on a token (the
/// alias separator, `Delete, Ctrl-d`). A lone `,` token is the literal comma *key* (`Space ,`), not
/// a separator, so it stays `main`. Descriptions pass `sep == main`, so their `/` and prose commas
/// (e.g. "Go to line (count, default 1)") are never dimmed.
fn sep_spans(text: &str, main: Style, sep: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tok) in text.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        if tok == "/" {
            spans.push(Span::styled("/", sep));
        } else if let Some(word) = tok.strip_suffix(',').filter(|w| !w.is_empty()) {
            spans.push(Span::styled(word.to_string(), main));
            spans.push(Span::styled(",", sep));
        } else if !tok.is_empty() {
            spans.push(Span::styled(tok.to_string(), main));
        }
    }
    spans
}

/// [`sep_spans`] then right-pad with spaces to a display width of `w`.
fn padded_spans(text: &str, w: usize, main: Style, sep: Style) -> Vec<Span<'static>> {
    let mut spans = sep_spans(text, main, sep);
    let pad = w.saturating_sub(text.width());
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans
}

/// Greedy word-wrap to `width` columns. Always returns at least one (possibly empty) line. Words
/// longer than `width` overflow rather than being hard-split — fine for the short help strings.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = word.width();
        if cur.is_empty() {
            cur.push_str(word);
            cur_w = ww;
        } else if cur_w + 1 + ww <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        }
    }
    out.push(cur);
    out
}

/// One key+description cell in the help overlay. Owned because direction-pair rows merge the text
/// of two bindings.
struct Cell {
    key: String,
    desc: String,
}

/// A help row ready to render: a base cell and its optional aligned Alt cell.
struct DisplayRow {
    base: Cell,
    alt: Option<Cell>,
}

/// Forward/backward key pairs (in display order) whose rows are folded onto one line, merging both
/// the keys (`h`,`l` → `h/l`) and their symmetric descriptions. Keyed by context.
const DIRECTION_PAIRS: &[(keymap::KeyContext, KeyCode, KeyCode)] = &[
    (
        keymap::KeyContext::Normal,
        KeyCode::Char('h'),
        KeyCode::Char('l'),
    ),
    (
        keymap::KeyContext::Normal,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
    ),
    (
        keymap::KeyContext::Normal,
        KeyCode::Char('['),
        KeyCode::Char(']'),
    ),
    (
        keymap::KeyContext::Normal,
        KeyCode::Char('{'),
        KeyCode::Char('}'),
    ),
    (
        keymap::KeyContext::Normal,
        KeyCode::Char('<'),
        KeyCode::Char('>'),
    ),
    (keymap::KeyContext::Normal, KeyCode::Up, KeyCode::Down),
    (keymap::KeyContext::Normal, KeyCode::Left, KeyCode::Right),
    (
        keymap::KeyContext::Normal,
        KeyCode::PageUp,
        KeyCode::PageDown,
    ),
    (keymap::KeyContext::Insert, KeyCode::Up, KeyCode::Down),
    (keymap::KeyContext::Insert, KeyCode::Left, KeyCode::Right),
    (keymap::KeyContext::Search, KeyCode::Up, KeyCode::Down),
    (keymap::KeyContext::Search, KeyCode::Left, KeyCode::Right),
];

/// The display-ordered pair `code` belongs to in `cx`, if any.
fn direction_pair(cx: keymap::KeyContext, code: KeyCode) -> Option<(KeyCode, KeyCode)> {
    DIRECTION_PAIRS
        .iter()
        .find(|(c, a, b)| *c == cx && (*a == code || *b == code))
        .map(|(_, a, b)| (*a, *b))
}

/// Turn a sub-section's paired `(base, Alt)` bindings into display rows, folding direction pairs
/// (h/l, j/k, …) onto a single merged row.
fn build_display_rows(
    cx: keymap::KeyContext,
    rows: &[(&'static keymap::Binding, Option<&'static keymap::Binding>)],
) -> Vec<DisplayRow> {
    let mut out: Vec<DisplayRow> = Vec::new();
    let mut used = vec![false; rows.len()];
    for i in 0..rows.len() {
        if used[i] {
            continue;
        }
        let (base, alt) = rows[i];
        // Fold a direction pair only when its partner is in this section and both sides agree on
        // having an Alt variant (so the merged columns stay symmetric).
        if let Some((first, second)) = direction_pair(cx, base.code) {
            let partner = if base.code == first { second } else { first };
            if let Some(j) = rows.iter().position(|(b, _)| b.code == partner) {
                let (pbase, palt) = rows[j];
                if j != i && !used[j] && alt.is_some() == palt.is_some() {
                    used[i] = true;
                    used[j] = true;
                    // Put the two sides in the pair's display order.
                    let (fb, fa, sb, sa) = if base.code == first {
                        (base, alt, pbase, palt)
                    } else {
                        (pbase, palt, base, alt)
                    };
                    out.push(DisplayRow {
                        base: Cell {
                            key: merge_keys(&fb.key_label(), &sb.key_label()),
                            desc: merge_descs(fb.desc, sb.desc),
                        },
                        alt: match (fa, sa) {
                            (Some(fa), Some(sa)) => Some(Cell {
                                key: merge_keys(&fa.key_label(), &sa.key_label()),
                                desc: merge_descs(fa.desc, sa.desc),
                            }),
                            _ => None,
                        },
                    });
                    continue;
                }
            }
        }
        used[i] = true;
        out.push(DisplayRow {
            base: Cell {
                key: base.key_label(),
                desc: base.desc.to_string(),
            },
            alt: alt.map(|a| Cell {
                key: a.key_label(),
                desc: a.desc.to_string(),
            }),
        });
    }
    out
}

/// Collapse rows that are *aliases* — different keys bound to the same command, identified by an
/// identical description — into a single row whose keys are joined with `, ` (e.g. `Delete` and
/// `Ctrl-d`, both "Delete selection", → `Delete, Ctrl-d`). Only plain rows merge: a row carrying an
/// Alt variant keeps its two-column shape. Keys join in first-appearance order, and three-plus
/// aliases chain (`A, B, C`). Comma rather than the direction-pair `/` keeps "either key" distinct
/// from "two opposite directions".
fn merge_alias_rows(display: &mut Vec<DisplayRow>) {
    let mut i = 0;
    while i < display.len() {
        if display[i].alt.is_none() {
            let mut j = i + 1;
            while j < display.len() {
                if display[j].alt.is_none() && display[j].base.desc == display[i].base.desc {
                    let other = display.remove(j).base.key;
                    display[i].base.key = format!("{}, {}", display[i].base.key, other);
                } else {
                    j += 1;
                }
            }
        }
        i += 1;
    }
}

/// Merge two key labels into `a / b` form. When factoring the common prefix/suffix leaves a single
/// differing char on each side (`Alt-h`/`Alt-l` → `Alt-h / l`, `↑`/`↓` → `↑ / ↓`) we use the
/// compact factored form; otherwise we show both keys in full (`PageUp` / `PageDown`) since a
/// factored `PageUp / Down` reads worse. Chars (not bytes) so multi-byte glyphs aren't split.
fn merge_keys(a: &str, b: &str) -> String {
    if a == b {
        return a.to_string();
    }
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let (pre, mid_a, mid_b, suf) = factor_common(&av, &bv);
    if mid_a.len() <= 1 && mid_b.len() <= 1 {
        format!(
            "{}{} / {}{}",
            pre.iter().collect::<String>(),
            mid_a.iter().collect::<String>(),
            mid_b.iter().collect::<String>(),
            suf.iter().collect::<String>(),
        )
    } else {
        format!("{a} / {b}")
    }
}

/// Merge two descriptions word-wise: factor out common leading/trailing *words* and join the
/// differing middles with ` / ` — e.g. `"Char left"`+`"Char right"` → `"Char left / right"`.
/// Word-level (not char-level) so a letter shared by two words — the `t` in `left`/`right` — isn't
/// split off.
fn merge_descs(a: &str, b: &str) -> String {
    if a == b {
        return a.to_string();
    }
    let aw: Vec<&str> = a.split(' ').collect();
    let bw: Vec<&str> = b.split(' ').collect();
    let (pre, mid_a, mid_b, suf) = factor_common(&aw, &bw);
    let mut parts: Vec<String> = Vec::new();
    if !pre.is_empty() {
        parts.push(pre.join(" "));
    }
    parts.push(format!("{} / {}", mid_a.join(" "), mid_b.join(" ")));
    if !suf.is_empty() {
        parts.push(suf.join(" "));
    }
    parts.join(" ")
}

/// Split two slices into their common prefix, the two differing middles, and their common suffix.
/// Shared by the char-wise [`merge_keys`] and word-wise [`merge_descs`].
fn factor_common<'a, T: PartialEq>(a: &'a [T], b: &'a [T]) -> (&'a [T], &'a [T], &'a [T], &'a [T]) {
    let max = a.len().min(b.len());
    let mut p = 0;
    while p < max && a[p] == b[p] {
        p += 1;
    }
    let mut s = 0;
    while s < max - p && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    (
        &a[..p],
        &a[p..a.len() - s],
        &b[p..b.len() - s],
        &b[b.len() - s..],
    )
}

/// Label above the editable project-name field.
const NAME_LABEL: &str = "Name:";

/// Header block: `Project Settings` heading, a blank spacer, the editable name field (a `Name:`
/// label with the value on the indented line below it), another blank, and the `Project roots:`
/// label. Degrades gracefully when the header area is shorter than its 6 rows. The value renders
/// in plain (white) text like the add-root input row; its terminal caret — placed separately — is
/// the focus cue.
fn draw_settings_header(f: &mut Frame, settings: &crate::app::ProjectSettingsState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let heading_style = Style::default()
        .fg(NORD8)
        .bg(NORD0)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(NORD4)
        .bg(NORD0)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(NORD4).bg(NORD0);
    let area_w = area.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(6);
    if area.height >= 1 {
        let heading = truncate_right("Project Settings", area_w);
        lines.push(Line::from(Span::styled(heading, heading_style)));
    }
    if area.height >= 2 {
        lines.push(Line::from(""));
    }
    if area.height >= 3 {
        lines.push(Line::from(Span::styled(NAME_LABEL, label_style)));
    }
    if area.height >= 4 {
        // Value on the line below the label, indented one column to match how roots sit under the
        // `Project roots:` label.
        let value = truncate_right(&settings.name_input.text, area_w.saturating_sub(1));
        lines.push(Line::from(vec![
            Span::styled(" ", value_style),
            Span::styled(value, value_style),
        ]));
    }
    if area.height >= 5 {
        lines.push(Line::from(""));
    }
    if area.height >= 6 {
        lines.push(Line::from(Span::styled("Project roots:", label_style)));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(NORD4).bg(NORD0)),
        area,
    );
}

/// Geometry of the settings overlay subareas. Computed once per draw and reused by the cursor
/// placement so they can't drift out of sync.
struct SettingsLayout {
    header: Rect,
    rows: Rect,
    error: Option<Rect>,
}

fn settings_layout(box_area: Rect, has_error: bool) -> Option<SettingsLayout> {
    if box_area.width < 4 || box_area.height < 4 {
        return None;
    }
    let inner = Rect {
        x: box_area.x + 1,
        y: box_area.y + 1,
        width: box_area.width - 2,
        height: box_area.height - 2,
    };
    let content = pad_horizontal(inner);
    if content.height == 0 || content.width == 0 {
        return None;
    }
    let header_h = 6u16.min(content.height);
    let remaining = content.height - header_h;
    let error_h = if has_error { 1u16.min(remaining) } else { 0u16 };
    let rows_h = remaining - error_h;
    let header = Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: header_h,
    };
    let rows = Rect {
        x: content.x,
        y: content.y + header_h,
        width: content.width,
        height: rows_h,
    };
    let error = if error_h > 0 {
        Some(Rect {
            x: content.x,
            y: content.y + header_h + rows_h,
            width: content.width,
            height: error_h,
        })
    } else {
        None
    };
    Some(SettingsLayout {
        header,
        rows,
        error,
    })
}

/// Render the roots + input row list. On a root row the path text is bolded in the accent color
/// when selected (no row-spanning bg bar — keeps the highlight subtle and consistent with the
/// project picker); the pending-delete row swaps the path for a red `Remove "<path>"? [y/N]`
/// prompt. The input row carries no selection styling — its visible terminal caret is the focus
/// cue. Each list item is indented one column past the section label.
fn draw_settings_rows(
    f: &mut Frame,
    state: &AppState,
    settings: &crate::app::ProjectSettingsState,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let base_style = Style::default().fg(NORD4).bg(NORD0);
    let total_items = settings.roots.len() + 1;
    let max = (area.height as usize).max(1);
    // `selected` is dialog-global (0 = name field, in the header); within this rows area item `i`
    // maps to global index `i + 1`. Drop one level to scroll relative to the rows.
    let rows_selected = settings.selected.saturating_sub(1);
    let start = rows_selected
        .saturating_sub(max.saturating_sub(1))
        .min(total_items.saturating_sub(max));
    let area_w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for i in start..(start + max).min(total_items) {
        let highlighted = settings.selected == i + 1;
        // 1-col indent so list items sit visually under the section label.
        let leading = Span::styled(" ", base_style);
        let text_budget = area_w.saturating_sub(1);
        if i < settings.roots.len() {
            let root = &settings.roots[i];
            let pending = settings.pending_delete && settings.selected == i + 1;
            if pending {
                const PREFIX: &str = "Remove \"";
                const SUFFIX: &str = "\"? [y/N]";
                let fixed_w = PREFIX.width() + SUFFIX.width();
                let path_budget = text_budget.saturating_sub(fixed_w);
                let path = truncate_middle(root, path_budget);
                let warn_style = Style::default()
                    .fg(NORD11)
                    .bg(NORD0)
                    .add_modifier(Modifier::BOLD);
                let body = Span::styled(format!("{PREFIX}{path}{SUFFIX}"), warn_style);
                lines.push(Line::from(vec![leading, body]));
                continue;
            }
            // A colour-coded dot when the active buffer under this root is dirty / changed on
            // disk (` •`), reserving its width so the path truncates to leave room.
            let status = root_buffer_status(state, root);
            let dot_w = if status.is_some() { 2 } else { 0 };
            let truncated = truncate_middle(root, text_budget.saturating_sub(dot_w));
            let bg = if highlighted { NORD2 } else { NORD0 };
            let path_style = Style::default().fg(NORD4).bg(bg);
            let mut spans = vec![leading, Span::styled(truncated, path_style)];
            if let Some(kind) = status {
                spans.push(Span::styled(" ".to_string(), path_style));
                spans.push(Span::styled(
                    BUFFER_STATUS_DOT.to_string(),
                    path_style.fg(buffer_status_color(kind)),
                ));
            }
            lines.push(Line::from(spans));
        } else {
            // Input row: no highlight regardless of selection. Placeholder when empty, plain
            // text otherwise; ratatui clips past the right edge for very long inputs.
            let (text, style) = if settings.add_input.text.is_empty() {
                (
                    "Add root...".to_string(),
                    Style::default()
                        .fg(NORD3)
                        .bg(NORD0)
                        .add_modifier(Modifier::ITALIC),
                )
            } else {
                (
                    settings.add_input.text.clone(),
                    Style::default().fg(NORD4).bg(NORD0),
                )
            };
            lines.push(Line::from(vec![leading, Span::styled(text, style)]));
        }
    }
    f.render_widget(Paragraph::new(lines).style(base_style), area);
}

/// Place the terminal caret on the settings overlay's name value (header line 3 — below the
/// heading, blank spacer, and `Name:` label, indented one column). Mirrors `draw_settings_header`.
/// Only places the caret when the header is tall enough to show the value; otherwise leaves it
/// unset (ratatui hides it).
fn place_settings_name_cursor(
    f: &mut Frame,
    settings: &crate::app::ProjectSettingsState,
    buffer_area: Rect,
) {
    let box_area = picker_box_rect(buffer_area);
    let Some(layout) = settings_layout(box_area, settings.error.is_some()) else {
        return;
    };
    let header = layout.header;
    if header.height < 4 || header.width == 0 {
        return;
    }
    let row_y = header.y + 3;
    let typed_w = settings.name_input.width_to_cursor() as u16;
    // +1 for the one-column indent the value row carries.
    let base = header.x.saturating_add(1);
    let max_x = header.x + header.width.saturating_sub(1);
    let col = base.saturating_add(typed_w).min(max_x);
    f.set_cursor_position((col, row_y));
}

/// Place the terminal caret on the settings overlay's input row. Mirrors the layout math in
/// `draw_project_settings_overlay`: same inner padding, same error-footer split, same scroll
/// slide. Only places the caret when the input row is currently visible (with a small list and
/// a tall box this is almost always true; if it scrolled off, we just leave the caret unset and
/// ratatui hides it for the frame).
fn place_settings_input_cursor(
    f: &mut Frame,
    settings: &crate::app::ProjectSettingsState,
    buffer_area: Rect,
) {
    let box_area = picker_box_rect(buffer_area);
    let Some(layout) = settings_layout(box_area, settings.error.is_some()) else {
        return;
    };
    let rows = layout.rows;
    if rows.height == 0 || rows.width == 0 {
        return;
    }
    let total_items = settings.roots.len() + 1;
    let max = (rows.height as usize).max(1);
    // See `draw_settings_rows`: `selected` is dialog-global (0 = name field); shift to a rows-area
    // index for the scroll math.
    let rows_selected = settings.selected.saturating_sub(1);
    let start = rows_selected
        .saturating_sub(max.saturating_sub(1))
        .min(total_items.saturating_sub(max));
    let input_idx = settings.roots.len();
    if input_idx < start || input_idx >= start + max {
        return;
    }
    let row_y = rows.y + (input_idx - start) as u16;
    // +1 for the leading " " indent each list item carries.
    let typed_w = settings.add_input.width_to_cursor() as u16;
    let max_x = rows.x + rows.width.saturating_sub(1);
    let col = rows.x.saturating_add(1).saturating_add(typed_w).min(max_x);
    f.set_cursor_position((col, row_y));
}

/// Middle-ellipsize `s` so it fits in `max_w` display columns. Preserves head and tail; collapses
/// the middle into a single `…`. Falls back to a bare `…` when there isn't even room for one
/// character on each side. Operates on display widths so wide chars don't break the budget.
fn truncate_middle(s: &str, max_w: usize) -> String {
    let total = s.width();
    if total <= max_w {
        return s.to_string();
    }
    if max_w == 0 {
        return String::new();
    }
    if max_w == 1 {
        return "…".to_string();
    }
    let budget = max_w - 1;
    let left_target = budget / 2;
    let right_target = budget - left_target;
    let mut left = String::new();
    let mut acc = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > left_target {
            break;
        }
        left.push(c);
        acc += cw;
    }
    let mut right_rev: Vec<char> = Vec::new();
    let mut acc = 0usize;
    for c in s.chars().rev() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > right_target {
            break;
        }
        right_rev.push(c);
        acc += cw;
    }
    let right: String = right_rev.into_iter().rev().collect();
    format!("{left}…{right}")
}

/// Right-truncate `s` to `max_w` display columns, appending `…`. Used for error messages where
/// the prefix carries the diagnostic.
fn truncate_right(s: &str, max_w: usize) -> String {
    let total = s.width();
    if total <= max_w {
        return s.to_string();
    }
    if max_w == 0 {
        return String::new();
    }
    if max_w == 1 {
        return "…".to_string();
    }
    let target = max_w - 1;
    let mut out = String::new();
    let mut acc = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > target {
            break;
        }
        out.push(c);
        acc += cw;
    }
    out.push('…');
    out
}

/// Mirror of the editor's status dot, applied per-root: returns the active buffer's state when
/// that buffer lives under `root` and is dirty / changed on disk, else `None`. This client only
/// knows about its own active editor, so the dot reflects "your active buffer is under this root
/// and is non-clean." Server-side dirty buffers from other clients won't show. Acceptable for v1.
fn root_buffer_status(state: &AppState, root: &str) -> Option<BufferStatusKind> {
    let ed = state.editor.as_ref()?;
    let status = state.buffer_status()?;
    let path = ed.file_path.as_deref()?;
    let root_path = std::path::Path::new(root);
    let buf_path = std::path::Path::new(path);
    (buf_path == root_path || buf_path.starts_with(root_path)).then_some(status)
}

/// Empty no-project view: a centered hint telling the user how to open the project picker.
/// Drawn instead of the buffer pane when `state.editor` is `None`. Fills the full pane in the
/// editor's NORD0 background so the no-project state visually matches an open editor instead of
/// falling through to the terminal's default colors.
/// The backdrop behind the Projects chooser before any project is selected: a bare NORD0 fill,
/// matching the native client's boot view. The chooser is the only UI here — dismissing it exits
/// the app (the shell sets `should_quit`), so this is only ever a momentary flash.
fn draw_no_project_view(f: &mut Frame, _state: &AppState, area: Rect) {
    f.render_widget(Paragraph::new("").style(Style::default().bg(NORD0)), area);
}

// ---- picker overlay ----------------------------------------------------------------------------

/// Picker box dimensions interpolate linearly with the buffer area. At or below the *min*
/// breakpoint the box fills the viewport (no padding). At or above the *max* breakpoint the box
/// is the *target percentage* of the viewport. In between, percentage scales linearly from 100%
/// down to the target. `area` here is the buffer pane (one row shorter than the terminal).
const PICKER_TARGET_WIDTH_PCT: u16 = 80;
const PICKER_TARGET_HEIGHT_PCT: u16 = 60;
const PICKER_MIN_COLS: u16 = 80;
const PICKER_MAX_COLS: u16 = 200;
const PICKER_MIN_ROWS: u16 = 24;
const PICKER_MAX_ROWS: u16 = 60;
/// Hard ceiling on the picker's width. The percentage scaling alone has no upper bound — 80%
/// of an ultrawide terminal is an enormous box whose rows are mostly padding and harder to
/// scan, so past this the extra terminal width stays with the editor. Mirrors the web client's
/// `min(720px, 80vw)` cap.
const PICKER_WIDTH_CAP: u16 = 120;

/// Compute the picker overlay's rectangle inside `area` (the buffer pane).
fn picker_box_rect(area: Rect) -> Rect {
    let width = scale_box_dim(
        area.width,
        PICKER_MIN_COLS,
        PICKER_MAX_COLS,
        PICKER_TARGET_WIDTH_PCT,
    );
    let height = scale_box_dim(
        area.height,
        PICKER_MIN_ROWS,
        PICKER_MAX_ROWS,
        PICKER_TARGET_HEIGHT_PCT,
    );
    let width = width.min(PICKER_WIDTH_CAP).min(area.width).max(1);
    let height = height.min(area.height).max(1);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Rows the full result set needs in the results pane — what the picker box collapses to when
/// that's shorter than the full-size box (mirroring the web client, whose list shrinks to fit
/// content). Grep uses the server-reported display-row total (hits + per-file headers); the
/// References empty states need one row for their message; the client-side synthetic
/// "Create …" row isn't counted in `total_matches`.
fn picker_content_rows(picker: &crate::picker::PickerState) -> u32 {
    use aether_protocol::picker::PickerKind;
    if picker.kind == Some(PickerKind::References) && picker.items.is_empty() {
        return 1; // "Finding references…" / "No references found"
    }
    if picker.kind == Some(PickerKind::Grep) {
        return picker.total_display_rows.unwrap_or(picker.total_matches);
    }
    picker.total_matches + picker.synthetic_create_idx.is_some() as u32
}

/// The picker box, collapsed to its content when the result set is shorter than the full-size
/// box (matching the web client). The top edge stays where the full-size box's top is — only
/// the bottom edge moves — so the input row doesn't jump as the result count changes. Chrome
/// around the results pane is 4 rows (borders + input + separator); with no content at all the
/// separator is dropped too, since it would double up against the bottom border.
fn collapsed_picker_box_rect(area: Rect, content_rows: u32, editor_open: bool) -> Rect {
    let full = picker_box_rect(area);
    let chrome: u32 = (if content_rows == 0 { 3 } else { 4 }) + editor_open as u32;
    let height = content_rows.saturating_add(chrome).min(full.height as u32) as u16;
    Rect { height, ..full }
}

/// Scale one box dimension: returns `dim` itself when `dim <= min` (no padding), `dim *
/// target_pct/100` when `dim >= max` (full padding), and interpolates the percentage linearly
/// from 100% down to `target_pct` in between.
fn scale_box_dim(dim: u16, min: u16, max: u16, target_pct: u16) -> u16 {
    if dim <= min {
        return dim;
    }
    if dim >= max {
        return ((dim as u32 * target_pct as u32) / 100) as u16;
    }
    let range = (max - min) as u32;
    let progress = (dim - min) as u32;
    let shrink = (100 - target_pct as u32) * progress / range; // 0 at min, 100 - target_pct at max
    let pct = 100u32 - shrink;
    ((dim as u32 * pct) / 100) as u16
}

/// How many result rows the picker can display given the buffer-area dimensions. Used by the
/// app to set the `limit` it sends to the server. Subtracts box borders (2), input row (1), and
/// separator row (1).
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub fn picker_result_rows(buffer_area_cols: u32, buffer_area_rows: u32) -> u32 {
    let area = Rect {
        x: 0,
        y: 0,
        width: buffer_area_cols as u16,
        height: buffer_area_rows as u16,
    };
    let box_rect = picker_box_rect(area);
    (box_rect.height as u32).saturating_sub(4)
}

/// Count how many items starting at `start` fit when rendered with the grep picker's
/// file-grouped layout (one non-selectable header row per distinct file path). Used by both the
/// scroll math (where it caps the visible window inside the over-fetched cache) and the
/// renderer (where it bounds the slice it draws).
pub fn grep_visible_item_count_from(
    items: &[PickerItem],
    start: usize,
    pane_height: usize,
) -> usize {
    if pane_height == 0 || start >= items.len() {
        return 0;
    }
    let mut rows_used: usize = 0;
    let mut prev_key: Option<(u32, &str)> = None;
    let mut visible: usize = 0;
    for item in &items[start..] {
        let cur_key = match item {
            PickerItem::GrepHit {
                path_index,
                relative_path,
                ..
            } => Some((*path_index, relative_path.as_str())),
            _ => None,
        };
        let needs_header = match cur_key {
            Some(k) => prev_key != Some(k),
            None => false,
        };
        let cost = if needs_header { 2 } else { 1 };
        if rows_used + cost > pane_height {
            break;
        }
        rows_used += cost;
        visible += 1;
        if let Some(k) = cur_key {
            prev_key = Some(k);
        }
    }
    visible
}

/// How many items fit when rendered starting at `start`, for any picker kind. Wraps the
/// grep-specific helper for `Grep`, and is a flat `min(items.len() - start, pane_height)` for
/// the rest.
pub fn picker_visible_item_count_from(
    items: &[PickerItem],
    start: usize,
    pane_height: usize,
    kind: Option<aether_protocol::picker::PickerKind>,
) -> usize {
    if matches!(kind, Some(aether_protocol::picker::PickerKind::Grep)) {
        grep_visible_item_count_from(items, start, pane_height)
    } else {
        items.len().saturating_sub(start).min(pane_height)
    }
}

/// Compute the picker scroll offset (first visible item index) that keeps `selected` on screen,
/// accounting for grep's non-selectable header rows. Returns the new offset given the current
/// one. For non-grep pickers this is the flat 1-row-per-item math; for grep the visible window
/// holds fewer items than `pane_height` because each file group spends a row on its header, so
/// a flat `selected + 1 - pane` under-scrolls by the header count — exactly the "selected row
/// sits below the box" symptom. Bottom-aligning walks the real layout (`grep_visible_item_count_from`)
/// to find the smallest start that still shows `selected` as the last fitting item.
pub fn picker_scroll_for_selected(
    items: &[PickerItem],
    selected: usize,
    current: usize,
    pane_height: usize,
    kind: Option<aether_protocol::picker::PickerKind>,
) -> usize {
    let pane = pane_height.max(1);
    // Scrolled above the window: pin the selection to the top.
    if selected < current {
        return selected;
    }
    // Already within the visible window: leave the scroll where it is.
    let count = picker_visible_item_count_from(items, current, pane, kind);
    if selected < current + count {
        return current;
    }
    // Below the window: bottom-align so `selected` is the last visible row.
    if !matches!(kind, Some(aether_protocol::picker::PickerKind::Grep)) {
        return (selected + 1).saturating_sub(pane);
    }
    let mut start = selected;
    while start > 0 {
        let candidate = start - 1;
        if candidate + grep_visible_item_count_from(items, candidate, pane) > selected {
            start = candidate;
        } else {
            break;
        }
    }
    start
}

fn draw_picker_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    // The LSP drill-down keeps the full-size box (its content is wrapped prose, not result
    // rows); everything else collapses to its content.
    let lsp_detail_open = state.picker.kind
        == Some(aether_protocol::picker::PickerKind::LspServers)
        && state.picker.lsp_detail.is_some();
    let editor_open = state.picker.chip_editor.is_some();
    let box_area = if lsp_detail_open {
        picker_box_rect(area)
    } else {
        collapsed_picker_box_rect(area, picker_content_rows(&state.picker), editor_open)
    };
    if box_area.width < 4 || box_area.height < 3 {
        return; // Too small to draw anything meaningful.
    }
    f.render_widget(Clear, box_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(NORD4))
        .style(Style::default().bg(NORD0).fg(NORD4));
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);

    // Inner layout: input row, the chip editor line when one is open (revealed *below* the
    // input so chips + query stay visible while editing), separator row (full-width, ties into
    // the borders), results. The separator row only exists when there's content to separate —
    // matching the chrome math in `collapsed_picker_box_rect`. This isn't just cosmetic: an
    // overconstrained vertical split (more `Length(1)` rows than the collapsed box has) makes
    // ratatui's solver zero out an *earlier* row, so an unconditional separator constraint in
    // a content-less box would swallow the editor line and render the separator in its place.
    let has_content = picker_content_rows(&state.picker) > 0;
    let mut constraints = vec![Constraint::Length(1)];
    if editor_open {
        constraints.push(Constraint::Length(1));
    }
    if has_content {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    // LSP-servers drill-down: one plain box (no input/separator split — that reads as a filter
    // field) with the title and the status/error as a single scrollable region. `Esc` returns to
    // the list.
    if let (Some(aether_protocol::picker::PickerKind::LspServers), Some(detail)) =
        (state.picker.kind, state.picker.lsp_detail.as_ref())
    {
        draw_lsp_detail(f, detail, pad_horizontal(inner));
        return;
    }
    draw_picker_input_row(f, state, pad_horizontal(rows[0]));
    let mut next = 1;
    if editor_open {
        draw_chip_editor_row(f, state, pad_horizontal(rows[next]));
        next += 1;
    }
    if has_content {
        draw_picker_separator(f, box_area, rows[next]);
        next += 1;
    }
    draw_picker_results(f, state, pad_horizontal(rows[next]));
}

/// The LSP-server detail drill-down: a status dot + bold name title, then labelled rows —
/// Language / Workspace / Error (crashed only) / Working (active progress) — matching the web
/// client's dialog field-for-field. The lifecycle state itself has no row: the dot's colour and
/// the presence of an Error/Working row already say it. No input/separator split, so it doesn't
/// masquerade as a filter box. Pre-wrapped so the scrollbar geometry is exact.
fn draw_lsp_detail(f: &mut Frame, detail: &crate::picker::LspServerDetail, area: Rect) {
    let text_w = area.width.saturating_sub(2).max(1); // reserve the scrollbar column + a gap
    let busy = matches!(detail.status, LspStatus::Ready) && !detail.progress.is_empty();
    let dot_color = if busy {
        NORD13
    } else {
        lsp_status_color(&detail.status)
    };
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("• ".to_string(), Style::default().fg(dot_color)),
            Span::styled(
                detail.name.clone(),
                Style::default().fg(NORD4).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    let w = text_w as usize;
    push_lsp_detail_row(&mut lines, "Language", &detail.language, NORD4, w);
    push_lsp_detail_row(&mut lines, "Workspace", &detail.workspace_root, NORD4, w);
    if let LspStatus::Crashed { code, message } = &detail.status {
        let mut msg = message.clone();
        if let Some(c) = code {
            msg.push_str(&format!(" (exit code {c})"));
        }
        push_lsp_detail_row(&mut lines, "Error", &msg, NORD11, w);
    }
    for (i, p) in detail.progress.iter().enumerate() {
        let mut text = p.title.clone();
        if let Some(pct) = p.percentage {
            text.push_str(&format!(" {pct}%"));
        }
        if let Some(msg) = &p.message {
            text.push_str(&format!("  {msg}"));
        }
        // The label appears once; further operations keep the value column.
        push_lsp_detail_row(
            &mut lines,
            if i == 0 { "Working" } else { "" },
            &text,
            NORD13,
            w,
        );
    }
    let total = lines.len() as u16;
    let body_h = area.height;
    detail.scroll.record(total, body_h);
    let offset = detail.scroll.offset();
    let text_area = Rect {
        width: text_w,
        ..area
    };
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(NORD0).fg(NORD4))
            .scroll((offset, 0)),
        text_area,
    );
    if total > body_h {
        let bar = Rect {
            x: area.x + area.width - 1,
            y: area.y,
            width: 1,
            height: area.height,
        };
        draw_vertical_scrollbar(f, bar, offset, total, body_h);
    }
}

/// One labelled row of the LSP detail: a dim `Label` column, then the value in `color`, wrapped
/// to the remaining width with continuation lines indented to the value column. An empty label
/// keeps the column (wrap continuations; second and later Working operations).
fn push_lsp_detail_row(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    color: Color,
    total_w: usize,
) {
    const KEY_W: usize = 12; // "Workspace" + gap — mirrors the web dialog's label column
    let val_w = total_w.saturating_sub(KEY_W).max(8);
    for (i, wrapped) in wrap_words(value, val_w).into_iter().enumerate() {
        let lbl = if i == 0 { label } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("{lbl:<KEY_W$}"), Style::default().fg(NORD3)),
            Span::styled(wrapped, Style::default().fg(color)),
        ]));
    }
}

/// Inset `area` by one column on each side. If the area is too narrow for any padding (≤2 cols),
/// returns it unchanged so we degrade gracefully.
fn pad_horizontal(area: Rect) -> Rect {
    if area.width <= 2 {
        return area;
    }
    Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width - 2,
        height: area.height,
    }
}

/// Query left-aligned, `N/M` (with a trailing `…` while ticking) right-aligned. When the query
/// is empty we render a dim placeholder describing what the picker matches against. For the
/// Explorer picker, an immutable dim prefix shows the directory the listing is for, sitting
/// flush with the typed query (cursor lands just after the prefix). Filter chips render
/// between the prefix and the query (see `docs/picker-filters.md`); while the in-row chip
/// prompt (glob/dir editor) is open it replaces the whole row. If the row is too narrow to
/// hold the counts, they get dropped first so the query stays visible.
fn draw_picker_input_row(f: &mut Frame, state: &AppState, area: Rect) {
    let base_style = Style::default().fg(NORD4).bg(NORD0);
    let placeholder_style = Style::default()
        .fg(NORD3)
        .bg(NORD0)
        .add_modifier(Modifier::ITALIC);
    // Both the root label and the relative-path portion are *committed* parts of the prefix —
    // colour them the same blue so the contrast in the row reads as "committed prefix" (blue)
    // vs "editable query" (default fg). Mirrored in the save-as prompt renderer.
    let label_style = Style::default().fg(NORD8).bg(NORD0);
    let path_style = Style::default().fg(NORD8).bg(NORD0);

    let total_width = area.width as usize;
    let (label_text, path_text) = explorer_input_prefix(state, total_width);
    let prefix_w = label_text.width() + path_text.width();
    let (chip_spans, chips_w) = picker_chip_spans(state, chip_budget(total_width, prefix_w));
    let prefix_has_content = prefix_w > 0 || chips_w > 0;

    let (left_text, left_style, left_w) = if state.picker.query.is_empty() {
        // Suppress the placeholder when the explorer prefix or a chip row is already telling
        // the user what's in effect — that *is* the context. Otherwise keep the placeholder.
        if prefix_has_content {
            (String::new(), base_style, 0)
        } else {
            let ph = picker_placeholder(state.picker.kind);
            (ph.to_string(), placeholder_style, ph.width())
        }
    } else {
        let q = state.picker.query.text.clone();
        let w = q.width();
        (q, base_style, w)
    };

    let counts = if state.picker.total_matches == 0 {
        // Initial phase (still searching, no hits yet): the throbber stands alone.
        state.picker.spinner.unwrap_or("").to_string()
    } else {
        // A filtered file/buffer list shows `matched/total`; an unfiltered list — and grep, where
        // every candidate is a hit — collapses to a single total. A throbber sits to the left while
        // results are still streaming.
        let num = if state.picker.total_matches == state.picker.total_candidates {
            format!("{}", state.picker.total_matches)
        } else {
            format!("{}/{}", state.picker.total_matches, state.picker.total_candidates)
        };
        match state.picker.spinner {
            Some(s) => format!("{s} {num}"),
            None => num,
        }
    };
    let counts_w = counts.width();

    // Chips lead the row, before the explorer's breadcrumb prefix — the scope they set applies
    // to everything after them, and the breadcrumb stays flush with the query it prefixes.
    let mut spans: Vec<Span<'static>> = chip_spans;
    if !label_text.is_empty() {
        spans.push(Span::styled(label_text, label_style));
    }
    if !path_text.is_empty() {
        spans.push(Span::styled(path_text, path_style));
    }
    spans.push(Span::styled(left_text, left_style));
    let used = prefix_w + chips_w + left_w;
    if !counts.is_empty() && used + counts_w < total_width {
        let pad = total_width.saturating_sub(used + counts_w);
        spans.push(Span::styled(" ".repeat(pad), base_style));
        spans.push(Span::styled(counts, base_style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(base_style), area);
}

/// The chip editor line revealed below the input row (`Alt-g` glob / `Alt-d` dir): builds its
/// spans plus the caret's x-offset within the line. One function so the renderer and the
/// caret-placement math can't drift. The dir editor renders as a single `dir:` field — root
/// segment, `:` separator, path segment — where both segments are ghost-text typeaheads
/// (save-as style): typed prefix, then the remainder of the current match in gray; Alt-j/k
/// swap the match — no candidate list, so the candidate count doesn't matter. The focused
/// segment is wherever the caret sits.
fn chip_editor_spans(state: &AppState) -> (Vec<Span<'static>>, u16) {
    use crate::picker::{ChipEditorField, ChipEditorKind};
    let Some(ed) = state.picker.chip_editor.as_ref() else {
        return (Vec::new(), 0);
    };
    let label_style = Style::default().fg(NORD8).bg(NORD0);
    let text_style = Style::default().fg(NORD4).bg(NORD0);
    let ghost_style = Style::default().fg(NORD3_BRIGHT).bg(NORD0);
    // An invalid segment (root matching no label / path that doesn't exist) renders red — the
    // visible form of "the commit gate will refuse this".
    let invalid_style = Style::default().fg(NORD11).bg(NORD0);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w: usize = 0;
    let mut cursor: usize = 0;
    let push = |spans: &mut Vec<Span<'static>>, w: &mut usize, text: String, style: Style| {
        *w += text.width();
        spans.push(Span::styled(text, style));
    };

    match ed.kind {
        ChipEditorKind::Glob { .. } => {
            push(&mut spans, &mut w, "glob: ".into(), label_style);
            cursor = w + ed.input.width_to_cursor();
            push(&mut spans, &mut w, ed.input.text.clone(), text_style);
        }
        ChipEditorKind::Dir { .. } => {
            // One field to the eye: `dir: {root}: {path}` in multi-root projects (the same
            // `root: path` shape the dir chip and status bar use), `dir: {path}` in
            // single-root ones. The root segment while unfocused — and the `:` separator —
            // render in the committed-prefix blue; the focused segment carries the caret;
            // invalid segments go red instead.
            let multi_root = state.project_paths.len() > 1;
            push(&mut spans, &mut w, "dir: ".into(), label_style);
            if multi_root {
                let labels = crate::labels::root_labels(&state.project_paths);
                let invalid = ed.root_invalid(&labels);
                if ed.field == ChipEditorField::Root {
                    cursor = w + ed.root_filter.width_to_cursor();
                    let style = if invalid { invalid_style } else { text_style };
                    push(&mut spans, &mut w, ed.root_filter.text.clone(), style);
                    // Ghost = the current match beyond the typed prefix. Nothing matches → no
                    // ghost; the red typed text is the cue.
                    if let Some((_, suffix)) = ed.root_ghost(&labels) {
                        push(&mut spans, &mut w, suffix, ghost_style);
                    }
                } else if invalid {
                    // An unfocused-but-unmatched root shows the raw red filter — not the
                    // fallback label, which would advertise a commit target the gate refuses.
                    push(
                        &mut spans,
                        &mut w,
                        ed.root_filter.text.clone(),
                        invalid_style,
                    );
                } else {
                    let chosen = ed.chosen_root(&labels) as usize;
                    let label = labels.get(chosen).cloned().unwrap_or_default();
                    push(&mut spans, &mut w, label, label_style);
                }
                // The separator appears once the path is in play (focused, or already holding
                // text) — a fresh root prompt shouldn't dangle a `:` off an unentered field.
                if ed.field == ChipEditorField::Path || !ed.input.text.is_empty() {
                    push(&mut spans, &mut w, ": ".into(), label_style);
                }
            }
            let path_style = if ed.path_invalid() {
                invalid_style
            } else {
                text_style
            };
            if ed.field == ChipEditorField::Path || !multi_root {
                cursor = w + ed.input.width_to_cursor();
                push(&mut spans, &mut w, ed.input.text.clone(), path_style);
                // Directory-only ghost suggestion (save-as idiom): the rest of the current
                // match plus its trailing `/`, gray after the caret.
                if let Some(suffix) = ed.path_ghost() {
                    push(&mut spans, &mut w, suffix, ghost_style);
                }
            } else {
                push(&mut spans, &mut w, ed.input.text.clone(), path_style);
            }
        }
    }
    (spans, cursor as u16)
}

/// Render the chip editor line (see [`chip_editor_spans`]).
fn draw_chip_editor_row(f: &mut Frame, state: &AppState, area: Rect) {
    let base_style = Style::default().fg(NORD4).bg(NORD0);
    let (spans, _) = chip_editor_spans(state);
    f.render_widget(Paragraph::new(Line::from(spans)).style(base_style), area);
}

/// Columns the chip row may occupy: everything after the explorer prefix, minus a reserve so
/// the query keeps a usable strip. Shared by the renderer and the caret-placement math.
fn chip_budget(total_width: usize, prefix_w: usize) -> usize {
    total_width.saturating_sub(prefix_w + 12)
}

/// Build the filter-chip spans for the picker input row and their total width. Chips render
/// compact: bare labels (no padding) on a raised background, one column apart; flag chips'
/// abbreviations are underlined so they read as toggles; the selected chip inverts. Exclude
/// globs (leading `!`) tint red. When the row overflows `max_w`, leftmost chips collapse into
/// a dim `…+N` marker — but never the selected chip, so chip-row navigation always shows what
/// it's acting on.
fn picker_chip_spans(state: &AppState, max_w: usize) -> (Vec<Span<'static>>, usize) {
    let chips = state.picker.chips(&state.project_paths);
    if chips.is_empty() {
        return (Vec::new(), 0);
    }
    let selected = state.picker.chip_selected.map(|s| s.min(chips.len() - 1));
    // Display labels: shrink long values so one chip can't eat the row. Dir chips use the
    // standardised segment elision (keeps the leaf dir); globs and flags middle-truncate —
    // a glob's significant syntax sits at both ends. Width per chip = label + trailing gap.
    let labels: Vec<String> = chips
        .iter()
        .map(|c| match c.id {
            crate::picker::ChipId::Dir(_) => truncate_path_with_indices(&c.label, &[], 24).0,
            _ => truncate_middle(&c.label, 24),
        })
        .collect();
    let chip_w = |label: &String| label.width() + 1;
    let mut width: usize = labels.iter().map(chip_w).sum();
    const MARKER_W: usize = 5; // "…+N " worst-case-ish reserve
    let mut start = 0;
    while start + 1 < chips.len()
        && width + if start > 0 { MARKER_W } else { 0 } > max_w
        && Some(start) != selected
    {
        width -= chip_w(&labels[start]);
        start += 1;
    }

    let chip_style = Style::default().fg(NORD8).bg(NORD2);
    let chip_exclude_style = Style::default().fg(NORD11).bg(NORD2);
    let chip_selected_style = Style::default().fg(NORD0).bg(NORD8);
    let chip_selected_exclude_style = Style::default().fg(NORD0).bg(NORD11);
    let gap_style = Style::default().fg(NORD4).bg(NORD0);
    let marker_style = Style::default().fg(NORD3).bg(NORD0);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut total = 0usize;
    if start > 0 {
        let marker = format!("…+{start} ");
        total += marker.width();
        spans.push(Span::styled(marker, marker_style));
    }
    for (i, label) in labels.iter().enumerate().skip(start) {
        let exclude = label.starts_with('!');
        let mut style = match (Some(i) == selected, exclude) {
            (true, true) => chip_selected_exclude_style,
            (true, false) => chip_selected_style,
            (false, true) => chip_exclude_style,
            (false, false) => chip_style,
        };
        // Only the whole-word chip underlines: "wd" alone reads as a stray token; the other
        // abbreviations (Aa, +ig, Δ, …) carry enough shape on their own.
        if chips[i].id == crate::picker::ChipId::Word {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        total += label.width() + 1;
        spans.push(Span::styled(label.clone(), style));
        spans.push(Span::styled(" ".to_string(), gap_style));
    }
    (spans, total)
}

/// The immutable dir-context prefix for the Explorer picker, split into two segments so the
/// renderer can colour them differently: a `{label}: ` segment (rendered in white, identifies
/// the root in multi-root projects) and a `{relative}/` segment (rendered in blue). Either may
/// be empty: the label segment is empty in single-root projects and at the top of a root with no
/// label; the path segment is empty at the top of any root. Both empty means no prefix at all
/// (Roots mode, or the explorer dir is outside every root).
///
/// Combined width is capped at half the row (and always leaves at least one cell for the query
/// cursor). When the natural prefix overflows, we drop the label and left-truncate the path
/// with a leading `…` — the leaf and trailing slash stay visible.
fn explorer_input_prefix(state: &AppState, available: usize) -> (String, String) {
    if !matches!(
        state.picker.kind,
        Some(aether_protocol::picker::PickerKind::Explorer)
    ) {
        return (String::new(), String::new());
    }
    let Some(dir) = state.picker.explorer_dir.as_deref() else {
        // Roots mode — rows already communicate "picking a root"; no breadcrumb needed.
        return (String::new(), String::new());
    };
    let (label_part, path_part) = match crate::app::strip_longest_root(dir, &state.project_paths) {
        Some((idx, rel)) => {
            let label = state.root_labels.get(idx).map(String::as_str).unwrap_or("");
            let label_part = if label.is_empty() {
                String::new()
            } else {
                format!("{label}: ")
            };
            let path_part = if rel.is_empty() {
                String::new()
            } else {
                format!("{rel}/")
            };
            (label_part, path_part)
        }
        None => return (String::new(), String::new()),
    };
    if available == 0 {
        return (String::new(), String::new());
    }
    // Half the row, but always leave room for the cursor on the typed query side.
    let max = (available / 2).max(1).min(available.saturating_sub(1));
    let total_w = label_part.width() + path_part.width();
    if total_w <= max {
        return (label_part, path_part);
    }
    // Over budget. Sacrifice the label first (the path is more useful), then shrink the path
    // itself via the standardised segment elision (the trailing `/` is re-appended — it's the
    // breadcrumb's "you're inside this dir" cue, not a path segment).
    let path_w = path_part.width();
    if path_w <= max {
        return (String::new(), path_part);
    }
    let bare = path_part.strip_suffix('/').unwrap_or(&path_part);
    let (shrunk, _) = truncate_path_with_indices(bare, &[], max.saturating_sub(1));
    (String::new(), format!("{shrunk}/"))
}

/// Placeholder for the picker's query input: the picker's action, ellipsised. Kept in sync with
/// the web client's `PLACEHOLDER` map (web/src/picker.ts).
fn picker_placeholder(kind: Option<aether_protocol::picker::PickerKind>) -> &'static str {
    match kind {
        Some(aether_protocol::picker::PickerKind::Files) => "Find files…",
        Some(aether_protocol::picker::PickerKind::Buffers) => "Switch buffer…",
        Some(aether_protocol::picker::PickerKind::Grep) => "Grep workspace…",
        Some(aether_protocol::picker::PickerKind::Explorer) => "Explore files…",
        Some(aether_protocol::picker::PickerKind::Projects) => "Select project…",
        Some(aether_protocol::picker::PickerKind::Diagnostics) => "List diagnostics…",
        Some(aether_protocol::picker::PickerKind::LspServers) => "List LSPs…",
        Some(aether_protocol::picker::PickerKind::References) => "List references…",
        None => "Search…",
    }
}

/// Horizontal line under the input. Extends the line *into* the side borders with tee characters
/// so the separator visually ties into the outer block — done by writing directly to the frame
/// buffer because the block has already been rendered.
fn draw_picker_separator(f: &mut Frame, box_area: Rect, area: Rect) {
    if area.height == 0 {
        return; // collapsed empty picker: no separator (its y would sit on the bottom border)
    }
    let line: String = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(NORD4).bg(NORD0)),
        area,
    );
    let buf = f.buffer_mut();
    let style = Style::default().fg(NORD4).bg(NORD0);
    let left_x = box_area.x;
    let right_x = box_area.x + box_area.width.saturating_sub(1);
    if area.y >= buf.area.y && area.y < buf.area.y + buf.area.height {
        buf.set_string(left_x, area.y, "├", style);
        buf.set_string(right_x, area.y, "┤", style);
    }
}

fn draw_picker_results(f: &mut Frame, state: &AppState, area: Rect) {
    // References resolves asynchronously (an LSP round-trip), so it opens empty. A blank pane
    // would read as a broken picker — show progress while it loads, and an explicit "none" once
    // it finishes empty. (The result-set kinds that are never empty-by-design skip this.)
    if state.picker.items.is_empty()
        && state.picker.kind == Some(aether_protocol::picker::PickerKind::References)
    {
        let msg = if state.picker.ticking {
            "Finding references…"
        } else {
            "No references found"
        };
        f.render_widget(
            Paragraph::new(msg).style(
                Style::default()
                    .bg(NORD0)
                    .fg(NORD3)
                    .add_modifier(Modifier::ITALIC),
            ),
            area,
        );
        return;
    }

    // The scrollbar (when the result set overflows) sits in the right-hand padding column —
    // flush against the box's right border — so text fills the full content width right up to
    // it, with no gap on either side. `area` is already inset one column from the border, so its
    // trailing edge (`area.x + area.width`) is that padding column, still inside the frame.
    let needs_scrollbar = state.picker.total_matches as u16 > area.height;
    let text_width = area.width;
    let text_area = Rect {
        x: area.x,
        y: area.y,
        width: text_width,
        height: area.height,
    };

    // Render only the visible slice — `visible_start..visible_start + visible_count`. Items
    // outside that range are part of the over-fetched cache that lets us scroll without an RPC.
    let pane_height = area.height as usize;
    let visible_start = state.picker.visible_start.min(state.picker.items.len());
    let visible_count = picker_visible_item_count_from(
        &state.picker.items,
        visible_start,
        pane_height,
        state.picker.kind,
    );
    let visible_end = (visible_start + visible_count).min(state.picker.items.len());

    let mut lines: Vec<Line> = Vec::with_capacity(visible_count);
    // For Grep, insert a non-selectable file header above the first hit of each new file path.
    // Headers eat into the visible row budget; the visible-count math above already accounts
    // for them, so what we render here will fit in `pane_height` rows.
    let mut prev_grep_key: Option<(u32, &str)> = None;
    for (offset_in_slice, item) in state.picker.items[visible_start..visible_end]
        .iter()
        .enumerate()
    {
        let i = visible_start + offset_in_slice;
        if let PickerItem::GrepHit {
            path_index,
            relative_path,
            ..
        } = item
        {
            let key = (*path_index, relative_path.as_str());
            if prev_grep_key != Some(key) {
                lines.push(Line::from(grep_file_header_spans(
                    *path_index,
                    relative_path,
                    &state.root_labels,
                    text_width as usize,
                )));
                prev_grep_key = Some(key);
            }
        }
        // A staged delete renders its [y/N] confirmation *over* the target row — in the same
        // warning red the settings overlay uses for root removal — replacing the normal spans.
        // Matched by `item_key` (which ignores fuzzy-match highlight offsets) rather than the
        // selected index, so a background re-rank can't smear the prompt onto the wrong row.
        if let Some(pending) = state.picker.pending_delete.as_ref() {
            if crate::picker::item_key(item) == crate::picker::item_key(&pending.item) {
                let prefix = format!("Delete {} \"", pending.noun);
                const SUFFIX: &str = "\"? [y/N]";
                let warn_style = Style::default()
                    .fg(NORD11)
                    .bg(NORD0)
                    .add_modifier(Modifier::BOLD);
                let name_budget =
                    (text_width as usize).saturating_sub(prefix.width() + SUFFIX.width());
                let shown = truncate_middle(&pending.name, name_budget);
                let prompt =
                    truncate_right(&format!("{prefix}{shown}{SUFFIX}"), text_width as usize);
                lines.push(Line::from(Span::styled(prompt, warn_style)));
                continue;
            }
        }
        let highlighted = i == state.picker.selected;
        let mut spans =
            picker_item_spans(item, &state.root_labels, highlighted, text_width as usize);
        // Italicise the synthetic "+ Create …" row so it reads as an action affordance rather
        // than a real entry. Applied uniformly across all spans of the row (including any
        // fuzzy-match-highlight spans), since the synthetic never has match indices anyway.
        if Some(i) == state.picker.synthetic_create_idx {
            for span in spans.iter_mut() {
                span.style = span.style.add_modifier(Modifier::ITALIC);
            }
        }
        // Extend the selection background to the pane's full width — the item spans only carry
        // their text. (Grep hits already pad to the edge for the right-aligned line number, so
        // their pad here is zero.)
        if highlighted {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let pad = (text_width as usize).saturating_sub(used);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), Style::default().bg(NORD2)));
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(NORD0).fg(NORD4)),
        text_area,
    );

    if needs_scrollbar {
        let scrollbar = Rect {
            x: area.x + area.width, // right padding column, flush against the border
            y: area.y,
            width: 1,
            height: area.height,
        };
        draw_picker_scrollbar(f, state, scrollbar);
    }
}

fn draw_picker_scrollbar(f: &mut Frame, state: &AppState, area: Rect) {
    let total = state.picker.total_matches as u64;
    // Use the actual on-screen item count for the thumb size, and the absolute position of the
    // visible window's top item (`offset + visible_start`) for the thumb position. With
    // over-fetch, `items.len()` would oversize the thumb and `offset` alone would peg it.
    let visible_start = state.picker.visible_start.min(state.picker.items.len());
    let window = picker_visible_item_count_from(
        &state.picker.items,
        visible_start,
        area.height as usize,
        state.picker.kind,
    ) as u64;
    let offset = state.picker.offset as u64 + visible_start as u64;
    render_scrollbar(f, area, offset, total, window);
}

fn picker_item_spans(
    item: &PickerItem,
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    if let PickerItem::GrepHit {
        line,
        preview,
        match_indices,
        ..
    } = item
    {
        return grep_hit_spans(*line, preview, match_indices, highlighted, max_width);
    }
    if let PickerItem::DirEntry {
        name,
        is_dir,
        match_indices,
        git_status,
    } = item
    {
        return dir_entry_spans(
            name,
            *is_dir,
            *git_status,
            match_indices,
            highlighted,
            max_width,
        );
    }
    // File rows get a leading dim `{label}: ` prefix; everything else falls through with the
    // legacy single-string display.
    if let PickerItem::File {
        path_index,
        relative_path,
        match_indices,
        git_status,
    } = item
    {
        return file_item_spans(
            *path_index,
            relative_path,
            match_indices,
            *git_status,
            root_labels,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::Root {
        path_index,
        match_indices,
    } = item
    {
        return root_item_spans(
            *path_index,
            match_indices,
            root_labels,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::Diagnostic {
        line,
        col,
        end_line,
        end_col,
        severity,
        message,
        match_indices,
        ..
    } = item
    {
        return diagnostic_item_spans(
            DiagRange {
                line: *line,
                col: *col,
                end_line: *end_line,
                end_col: *end_col,
            },
            *severity,
            message,
            match_indices,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::LspServer {
        name,
        language,
        root_label,
        status,
        progress,
        match_indices,
        ..
    } = item
    {
        return lsp_server_item_spans(
            LspServerRow {
                name,
                language,
                root_label,
                status,
                progress,
            },
            match_indices,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::Reference {
        display_path,
        line,
        preview,
        match_indices,
        ..
    } = item
    {
        return reference_item_spans(
            display_path,
            *line,
            preview,
            match_indices,
            highlighted,
            max_width,
        );
    }

    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);

    // Trailing buffer-state dot — matches the status bar's colour-coded indicator. Goes after the
    // display so it doesn't shift `match_indices` (which index into the display). `None` = clean.
    let (display_raw, match_indices, dot_color, italic) = match item {
        PickerItem::Buffer {
            display,
            status,
            match_indices,
            transient,
            ..
        } => (
            display.as_str(),
            match_indices.as_slice(),
            buffer_dirty_dot_color(*status),
            // Transient buffers slant, like the status-bar label.
            *transient,
        ),
        PickerItem::Project {
            name,
            match_indices,
        } => (name.as_str(), match_indices.as_slice(), None, false),
        PickerItem::File { .. }
        | PickerItem::GrepHit { .. }
        | PickerItem::DirEntry { .. }
        | PickerItem::Root { .. }
        | PickerItem::Diagnostic { .. }
        | PickerItem::LspServer { .. }
        | PickerItem::Reference { .. } => unreachable!("handled above"),
    };
    let (base, match_style) = if italic {
        (
            base.add_modifier(Modifier::ITALIC),
            match_style.add_modifier(Modifier::ITALIC),
        )
    } else {
        (base, match_style)
    };

    // The dot renders as ` •` (leading space + glyph) — reserve its width so the path truncates
    // to leave room for it.
    let dot_w = if dot_color.is_some() { 2 } else { 0 };
    let text_budget = max_width.saturating_sub(dot_w);
    let (display, indices) = truncate_path_with_indices(display_raw, match_indices, text_budget);

    let mut spans: Vec<Span<'static>> = Vec::new();
    if indices.is_empty() {
        spans.push(Span::styled(display, base));
    } else {
        // Walk char-by-char emitting spans where matched/unmatched runs alternate. `indices`
        // are char offsets into `display`, sorted ascending.
        let mut current = String::new();
        let mut current_is_match = false;
        let mut idx_iter = indices.iter().copied().peekable();
        for (ci, ch) in display.chars().enumerate() {
            let is_match = idx_iter.peek().copied() == Some(ci as u32);
            if is_match {
                idx_iter.next();
            }
            if is_match != current_is_match && !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    if current_is_match { match_style } else { base },
                ));
            }
            current_is_match = is_match;
            current.push(ch);
        }
        if !current.is_empty() {
            spans.push(Span::styled(
                current,
                if current_is_match { match_style } else { base },
            ));
        }
    }
    if let Some(color) = dot_color {
        spans.push(Span::styled(" ".to_string(), base));
        spans.push(Span::styled(BUFFER_STATUS_DOT.to_string(), base.fg(color)));
    }
    spans
}

/// Buffer-state dot colour for a picker row, matching the editor status bar / web favicon.
/// `None` for a clean buffer (no dot).
fn buffer_dirty_dot_color(status: BufferDirtyState) -> Option<Color> {
    match status {
        BufferDirtyState::Clean => None,
        BufferDirtyState::Unsaved => Some(NORD9), // frost blue — unsaved edits
        BufferDirtyState::ExternallyModified => Some(NORD12), // aurora orange — changed on disk
        BufferDirtyState::ExternallyDeleted => Some(NORD11), // aurora red — gone on disk
    }
}

/// Header row above each file's hits in the Grep picker: `{label}: {relative}` (the label only
/// for multi-root projects), all in NORD8 (frost blue), bold. Non-selectable; the picker cursor
/// lives on the GrepHit rows below.
fn grep_file_header_spans(
    path_index: u32,
    relative_path: &str,
    root_labels: &[String],
    max_width: usize,
) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(NORD8)
        .bg(NORD0)
        .add_modifier(Modifier::BOLD);
    let label = root_label_or_blank(root_labels, path_index);
    let combined = if label.is_empty() {
        relative_path.to_string()
    } else {
        format!("{label}: {relative_path}")
    };
    let (display, _) = truncate_path_with_indices(&combined, &[], max_width);
    vec![Span::styled(display, style)]
}

/// Dim foreground for secondary text on a picker row (root labels, line numbers, locations,
/// metadata tails). NORD3 is a neighbouring Polar Night shade to the NORD2 selection background
/// and all but vanishes on it, so the highlighted row brightens its dim spans to NORD4 — the
/// same treatment the web client gives `.picker-row.selected` metadata.
fn picker_dim_fg(highlighted: bool) -> Color {
    if highlighted {
        NORD4
    } else {
        NORD3
    }
}

/// File picker row: `{relative}  {label}` — the relative path styled like other picker items
/// (fuzzy-match highlight included), then for multi-root projects the root's label in a dim
/// foreground (NORD3) after it. The label is plain text — match indices in the protocol always
/// index into `relative_path` only.
fn file_item_spans(
    path_index: u32,
    relative_path: &str,
    match_indices: &[u32],
    git_status: Option<GitStatus>,
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(picker_dim_fg(highlighted)).bg(bg);
    let label = root_label_or_blank(root_labels, path_index);
    let suffix = if label.is_empty() {
        String::new()
    } else {
        format!("  {label}")
    };
    // Two-col leading status bullet, like the explorer; subtract it (and the suffix) from the budget.
    let relative_budget = max_width.saturating_sub(2).saturating_sub(suffix.width());
    let (display, indices) =
        truncate_path_with_indices(relative_path, match_indices, relative_budget);
    let mut spans: Vec<Span<'static>> = vec![git_status_bullet_span(git_status, bg)];
    push_styled_with_match_indices(&mut spans, &display, &indices, base, match_style);
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, label_style));
    }
    spans
}

/// Root row in the Explorer's Roots mode. Renders the disambiguated label as a single span;
/// match indices from the server index into the root's *basename* — which is always the start
/// of the label under option-B disambiguation — so we can apply them directly to the label
/// string. Selected row gets the standard NORD2 background, like other pickers.
fn root_item_spans(
    path_index: u32,
    match_indices: &[u32],
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let label = root_label_or_blank(root_labels, path_index).to_string();
    let (display, indices) = truncate_path_with_indices(&label, match_indices, max_width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    push_styled_with_match_indices(&mut spans, &display, &indices, base, match_style);
    spans
}

/// Lookup helper: returns `root_labels[idx]` or an empty string when the index is out of bounds
/// (defensive — shouldn't happen in normal flow but degrades gracefully if the labels lag a
/// freshly-pushed picker frame).
fn root_label_or_blank(root_labels: &[String], idx: u32) -> &str {
    root_labels
        .get(idx as usize)
        .map(String::as_str)
        .unwrap_or("")
}

/// Push `display` into `spans`, breaking it where `match_indices` (char offsets into `display`)
/// indicate a match so those chars get `match_style` and everything else gets `base`. Factored
/// out so the file picker and any future highlighted single-string row can share the same
/// rendering loop.
fn push_styled_with_match_indices(
    spans: &mut Vec<Span<'static>>,
    display: &str,
    match_indices: &[u32],
    base: Style,
    match_style: Style,
) {
    let mut idx_iter = match_indices.iter().copied().peekable();
    let mut current = String::new();
    let mut current_is_match = false;
    for (ci, c) in display.chars().enumerate() {
        let is_match = idx_iter.peek().copied() == Some(ci as u32);
        if is_match {
            idx_iter.next();
        }
        if is_match != current_is_match && !current.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut current),
                if current_is_match { match_style } else { base },
            ));
        }
        current.push(c);
        current_is_match = is_match;
    }
    if !current.is_empty() {
        spans.push(Span::styled(
            current,
            if current_is_match { match_style } else { base },
        ));
    }
}

/// One Grep hit row: the preview (leading whitespace stripped) with `match_indices` highlighted,
/// then the line number right-aligned at the row's edge in a dim colour — mirroring the web
/// client's layout. An overflowing preview is cut with a dim `…` so the line number (plus at
/// least a 2-col gap) always stays visible, whatever its digit count.
fn grep_hit_spans(
    line: u32,
    preview: &str,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let dim_style = base.fg(picker_dim_fg(highlighted));
    let gap = 2; // minimum gap between the preview and the line number

    // Indentation is noise in a flat hit list — strip it. `match_indices` are char offsets into
    // the untrimmed preview, so shift them down by the stripped char count (indices that fall
    // inside the stripped whitespace itself drop out).
    let trimmed = preview.trim_start();
    let lead_chars = (preview.chars().count() - trimmed.chars().count()) as u32;
    let shifted: Vec<u32> = match_indices
        .iter()
        .filter_map(|i| i.checked_sub(lead_chars))
        .collect();

    let line_str = (line + 1).to_string();
    let preview_budget = max_width.saturating_sub(gap + line_str.width());

    // Truncate the preview from the right when it overflows, marking the cut with a `…` (which
    // takes one of the budget's columns); drop match indices that fall past the cut.
    let (shown, ellipsis) = if trimmed.width() <= preview_budget {
        (trimmed.to_string(), false)
    } else {
        let text_budget = preview_budget.saturating_sub(1);
        let cut: String = trimmed
            .chars()
            .scan(0usize, |w, c| {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if *w + cw > text_budget {
                    None
                } else {
                    *w += cw;
                    Some(c)
                }
            })
            .collect();
        (cut, true)
    };
    let kept_char_count = shown.chars().count() as u32;
    let kept_indices: Vec<u32> = shifted
        .into_iter()
        .filter(|&i| i < kept_char_count)
        .collect();

    let mut spans: Vec<Span<'static>> = Vec::new();
    push_styled_with_match_indices(&mut spans, &shown, &kept_indices, base, match_style);
    if ellipsis {
        spans.push(Span::styled("…".to_string(), dim_style));
    }
    // Pad out to the right edge (≥ the gap by construction), so the numbers' last digits align
    // down the file group. The pad carries the row background, like the text.
    let used = shown.width() + usize::from(ellipsis) + line_str.width();
    spans.push(Span::styled(
        " ".repeat(max_width.saturating_sub(used)),
        base,
    ));
    spans.push(Span::styled(line_str, dim_style));
    spans
}

/// A diagnostic's start/end buffer position (0-based), as carried flattened on
/// [`PickerItem::Diagnostic`]; rendered via [`diag_range_label`].
#[derive(Clone, Copy)]
struct DiagRange {
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
}

/// Diagnostics-picker row: `• {line} {message}`, the dot colored by severity (matching the gutter)
/// and the line number dim; fuzzy matches in the message are highlighted.
fn diagnostic_item_spans(
    range: DiagRange,
    severity: DiagnosticSeverity,
    message: &str,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    // The message itself is colored by severity (matching the squiggle/popup); fuzzy matches stay
    // the bright accent so they remain visible. The range trails in gray parentheses.
    let base = Style::default().fg(diag_color(severity)).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let line_suffix = format!(" ({})", diag_range_label(range));
    // Leading severity icon, matching the status-bar count.
    let icon = format!("{} ", diag_glyph(severity));
    let msg_budget = max_width
        .saturating_sub(line_suffix.width())
        .saturating_sub(icon.width());

    let truncated: String = message
        .chars()
        .scan(0usize, |w, c| {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if *w + cw > msg_budget {
                None
            } else {
                *w += cw;
                Some(c)
            }
        })
        .collect();
    let kept = truncated.chars().count() as u32;
    let kept_indices: Vec<u32> = match_indices
        .iter()
        .copied()
        .filter(|&i| i < kept)
        .collect();

    let mut spans: Vec<Span<'static>> = vec![Span::styled(icon, base)];
    if kept_indices.is_empty() {
        spans.push(Span::styled(truncated, base));
    } else {
        let mut current = String::new();
        let mut current_is_match = false;
        let mut idx_iter = kept_indices.iter().copied().peekable();
        for (ci, ch) in truncated.chars().enumerate() {
            let is_match = idx_iter.peek().copied() == Some(ci as u32);
            if is_match {
                idx_iter.next();
            }
            if is_match != current_is_match && !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    if current_is_match { match_style } else { base },
                ));
            }
            current_is_match = is_match;
            current.push(ch);
        }
        if !current.is_empty() {
            spans.push(Span::styled(
                current,
                if current_is_match { match_style } else { base },
            ));
        }
    }
    spans.push(Span::styled(
        line_suffix,
        Style::default().fg(picker_dim_fg(highlighted)).bg(bg),
    ));
    spans
}

/// A diagnostic's range as a compact `line:col` label (1-based), collapsing to `line:col-endcol`
/// when start and end share a line and to a single `line:col` for a zero-width point.
fn diag_range_label(r: DiagRange) -> String {
    if r.line == r.end_line && r.col == r.end_col {
        format!("{}:{}", r.line + 1, r.col + 1)
    } else if r.line == r.end_line {
        format!("{}:{}-{}", r.line + 1, r.col + 1, r.end_col + 1)
    } else {
        format!(
            "{}:{}-{}:{}",
            r.line + 1,
            r.col + 1,
            r.end_line + 1,
            r.end_col + 1
        )
    }
}

/// One references-picker row: a dim `path:line` location prefix (path middle-truncated when long,
/// so the filename + line stay visible), then the referenced line's preview with `match_indices`
/// highlighted — the same fuzzy-match tinting the grep/diagnostics rows use.
fn reference_item_spans(
    display_path: &str,
    line: u32,
    preview: &str,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let loc_style = base.fg(picker_dim_fg(highlighted));

    // Reserve up to half the row for the location prefix; the path truncates (segment-elided)
    // to fit so the filename and line number — the bits that identify the reference — survive.
    let line_part = format!(":{} ", line + 1);
    let prefix_budget = max_width / 2;
    let path_budget = prefix_budget.saturating_sub(line_part.width());
    let (path_shown, _) = truncate_path_with_indices(display_path, &[], path_budget);
    let prefix = format!("{path_shown}{line_part}");
    let preview_budget = max_width.saturating_sub(prefix.width());

    let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix, loc_style)];

    // Truncate the preview from the right when it overflows; drop match indices past the cut
    // (same approach as the grep row).
    let truncated: String = preview
        .chars()
        .scan(0usize, |w, c| {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if *w + cw > preview_budget {
                None
            } else {
                *w += cw;
                Some(c)
            }
        })
        .collect();
    let kept_char_count = truncated.chars().count() as u32;
    let kept_indices: Vec<u32> = match_indices
        .iter()
        .copied()
        .filter(|&i| i < kept_char_count)
        .collect();

    if kept_indices.is_empty() {
        spans.push(Span::styled(truncated, base));
    } else {
        let mut current = String::new();
        let mut current_is_match = false;
        let mut idx_iter = kept_indices.iter().copied().peekable();
        for (ci, ch) in truncated.chars().enumerate() {
            let is_match = idx_iter.peek().copied() == Some(ci as u32);
            if is_match {
                idx_iter.next();
            }
            if is_match != current_is_match && !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    if current_is_match { match_style } else { base },
                ));
            }
            current_is_match = is_match;
            current.push(ch);
        }
        if !current.is_empty() {
            spans.push(Span::styled(
                current,
                if current_is_match { match_style } else { base },
            ));
        }
    }
    spans
}

/// The identity-and-state fields of one LSP server, borrowed from [`PickerItem::LspServer`].
struct LspServerRow<'a> {
    name: &'a str,
    language: &'a str,
    root_label: &'a str,
    status: &'a LspStatus,
    progress: &'a [LspProgress],
}

/// One LSP-servers picker row: a status dot (the same medium `•` cell the file pickers use for
/// git status, coloured like the status-bar indicator), the server name with fuzzy-match
/// highlights, and a dim `language · root` tail. The dot re-renders live as
/// `lsp/status_changed` re-pushes the picker.
fn lsp_server_item_spans(
    server: LspServerRow<'_>,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let LspServerRow {
        name,
        language,
        root_label,
        status,
        progress,
    } = server;
    let bg = if highlighted { NORD2 } else { NORD0 };
    // A ready server with active `$/progress` work shows the busy colour (same as the status bar).
    let busy = matches!(status, LspStatus::Ready) && !progress.is_empty();
    let dot_color = if busy {
        NORD13
    } else {
        lsp_status_color(status)
    };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    // Dim tail: `language · root`, the root only when the server isn't at the project root
    // (empty `root_label` → omitted, so single-root projects show just the language).
    let tail = if root_label.is_empty() {
        format!("  {language}")
    } else {
        format!("  {language} · {root_label}")
    };
    // Live progress hint (e.g. "  cargo check 28% +1"), rendered in the activity color after the tail.
    let hint = lsp_progress_hint(progress);
    // Status-dot cell (two cols, like the git bullets), then the name fills the budget left
    // after the tail and hint.
    let name_budget = max_width
        .saturating_sub(2)
        .saturating_sub(tail.width())
        .saturating_sub(hint.width());

    let truncated: String = name
        .chars()
        .scan(0usize, |w, c| {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if *w + cw > name_budget {
                None
            } else {
                *w += cw;
                Some(c)
            }
        })
        .collect();
    let kept = truncated.chars().count() as u32;
    let kept_indices: Vec<u32> = match_indices
        .iter()
        .copied()
        .filter(|&i| i < kept)
        .collect();

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        "• ".to_string(),
        Style::default().fg(dot_color).bg(bg),
    ));
    if kept_indices.is_empty() {
        spans.push(Span::styled(truncated, base));
    } else {
        let mut current = String::new();
        let mut current_is_match = false;
        let mut idx_iter = kept_indices.iter().copied().peekable();
        for (ci, ch) in truncated.chars().enumerate() {
            let is_match = idx_iter.peek().copied() == Some(ci as u32);
            if is_match {
                idx_iter.next();
            }
            if is_match != current_is_match && !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    if current_is_match { match_style } else { base },
                ));
            }
            current_is_match = is_match;
            current.push(ch);
        }
        if !current.is_empty() {
            spans.push(Span::styled(
                current,
                if current_is_match { match_style } else { base },
            ));
        }
    }
    spans.push(Span::styled(
        tail,
        Style::default().fg(picker_dim_fg(highlighted)).bg(bg),
    ));
    if !hint.is_empty() {
        spans.push(Span::styled(hint, Style::default().fg(NORD13).bg(bg)));
    }
    spans
}

/// A compact one-line summary of a server's active `$/progress` work for a picker row: the
/// (alphabetically first) operation's title, its percentage when known, and `+N` when more are
/// running. Empty when the server is idle.
fn lsp_progress_hint(progress: &[LspProgress]) -> String {
    let Some(first) = progress.first() else {
        return String::new();
    };
    let mut s = format!("  {}", first.title);
    if let Some(pct) = first.percentage {
        s.push_str(&format!(" {pct}%"));
    }
    if progress.len() > 1 {
        s.push_str(&format!(" +{}", progress.len() - 1));
    }
    s
}

/// One Explorer entry row: leaf name with a trailing `/` for directories, NORD8 (frost blue)
/// for directories, fuzzy-match highlights overlaid the same way the Files picker does. The
/// `/` suffix is appended *after* the name proper so `match_indices` (which index into the
/// name) don't have to know about it.
/// Status-bullet colour for a Git status: green for new, yellow for modified, red for
/// removed/conflict. `None` for ignored (and clean) entries — they carry no bullet (ignored is
/// dimmed via its text colour instead).
fn git_status_bullet_color(s: GitStatus) -> Option<Color> {
    match s {
        GitStatus::Added | GitStatus::Untracked => Some(NORD14),
        GitStatus::Modified => Some(NORD13),
        GitStatus::Deleted | GitStatus::Conflicted => Some(NORD11),
        GitStatus::Ignored => None,
    }
}

/// The leading status-indicator cell shared by explorer entries and file-picker rows: a coloured
/// `•` for a change, or two blank columns otherwise (fixed width so row text stays aligned).
fn git_status_bullet_span(git_status: Option<GitStatus>, bg: Color) -> Span<'static> {
    match git_status.and_then(git_status_bullet_color) {
        Some(color) => Span::styled("• ".to_string(), Style::default().fg(color).bg(bg)),
        None => Span::styled("  ".to_string(), Style::default().bg(bg)),
    }
}

fn dir_entry_spans(
    name: &str,
    is_dir: bool,
    git_status: Option<GitStatus>,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    // Leading status indicator: a coloured `•` for a changed entry, a blank cell otherwise so every
    // row's text stays column-aligned. Two cols wide (bullet + space).
    let bullet_span = git_status_bullet_span(git_status, bg);
    // Text colour keeps the frost-blue dir / snow-white file scheme; ignored entries dim to a
    // lighter gray (legible on both the normal and selected backgrounds).
    let fg = match git_status {
        Some(GitStatus::Ignored) => NORD3_BRIGHT,
        _ if is_dir => NORD8,
        _ => NORD4,
    };
    let base = Style::default().fg(fg).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let suffix = if is_dir { "/" } else { "" };
    // The bullet cell takes two columns off the budget; the rest is text + the `/` suffix.
    let text_budget = max_width.saturating_sub(2).saturating_sub(suffix.len());
    let (display, indices) = truncate_path_with_indices(name, match_indices, text_budget);

    let mut spans: Vec<Span<'static>> = vec![bullet_span];
    if indices.is_empty() {
        spans.push(Span::styled(display, base));
    } else {
        let mut current = String::new();
        let mut current_is_match = false;
        let mut idx_iter = indices.iter().copied().peekable();
        for (ci, ch) in display.chars().enumerate() {
            let is_match = idx_iter.peek().copied() == Some(ci as u32);
            if is_match {
                idx_iter.next();
            }
            if is_match != current_is_match && !current.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut current),
                    if current_is_match { match_style } else { base },
                ));
            }
            current_is_match = is_match;
            current.push(ch);
        }
        if !current.is_empty() {
            spans.push(Span::styled(
                current,
                if current_is_match { match_style } else { base },
            ));
        }
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix.to_string(), base));
    }
    spans
}

/// Trim `path` from the left (preserving the filename) when it overflows `max_width`, prefixing
/// the trimmed result with `…`. Match indices that fall inside the dropped prefix are removed;
/// surviving ones are shifted to reflect their new position in the displayed string.
/// The standardised path truncation (shared shape with the web client's `truncatePath`).
/// Shrinks `path` into `max_width` cells through a segment-aware ladder:
///
///  1. Fits → unchanged.
///  2. Elide whole *middle* segments to a single `…` (`crates/…/src/handlers.rs`): the last
///     segment (the filename) always survives, and among the candidates that fit we keep as
///     many segments as possible, ties broken toward the tail — the file's parents identify
///     it better than leading dirs do.
///  3. Floor: char-level left-cut with a leading `…`, keeping the end of the string — the
///     filename's tail is the last thing to go.
///
/// `match_indices` (char offsets into `path`) are remapped into the display; indices falling
/// inside an elided span drop out. Strings without `/` skip straight to the floor, so any
/// single-line label can pass through here safely.
fn truncate_path_with_indices(
    path: &str,
    match_indices: &[u32],
    max_width: usize,
) -> (String, Vec<u32>) {
    if max_width == 0 {
        return (String::new(), Vec::new());
    }
    if path.width() <= max_width {
        return (path.to_string(), match_indices.to_vec());
    }

    // Rung 2: segment elision. Candidates keep the first `l` and last `t` segments around one
    // `…` part; pick the fitting candidate with the most segments, preferring tail on ties.
    let segs: Vec<&str> = path.split('/').collect();
    let n = segs.len();
    if n >= 2 {
        let seg_w: Vec<usize> = segs.iter().map(|s| s.width()).collect();
        let mut best: Option<(usize, usize)> = None; // (lead, tail), tail ≥ 1
        for t in 1..n {
            for l in 0..=(n - 1 - t) {
                let w: usize = seg_w[..l].iter().sum::<usize>()
                    + seg_w[n - t..].iter().sum::<usize>()
                    + (l + t) // one `/` per kept segment (around the `…` part)
                    + 1; // the `…` itself
                if w <= max_width && best.is_none_or(|(bl, bt)| (l + t, t) > (bl + bt, bt)) {
                    best = Some((l, t));
                }
            }
        }
        if let Some((l, t)) = best {
            let lead = segs[..l].join("/");
            let tail = segs[n - t..].join("/");
            let display = if l == 0 {
                format!("…/{tail}")
            } else {
                format!("{lead}/…/{tail}")
            };
            // Remap: the kept lead is an exact prefix of the original, the kept tail an exact
            // suffix; everything between (the elided span and its separators) drops out.
            let lead_chars = lead.chars().count();
            let orig_tail_start = path.chars().count() - tail.chars().count();
            let display_tail_start = if l == 0 { 2 } else { lead_chars + 3 }; // past `…/` / `/…/`
            let new_indices: Vec<u32> = match_indices
                .iter()
                .filter_map(|&i| {
                    let i = i as usize;
                    if l > 0 && i < lead_chars {
                        Some(i as u32)
                    } else if i >= orig_tail_start {
                        Some((i - orig_tail_start + display_tail_start) as u32)
                    } else {
                        None
                    }
                })
                .collect();
            return (display, new_indices);
        }
    }

    // Rung 3 (floor): keep characters from the end until we've filled max_width - 1 (one cell
    // for the `…`).
    let chars: Vec<char> = path.chars().collect();
    let budget = max_width.saturating_sub(1);
    let mut kept_w = 0;
    let mut kept_start_char: usize = chars.len();
    for (i, c) in chars.iter().enumerate().rev() {
        let w = UnicodeWidthChar::width(*c).unwrap_or(0);
        if kept_w + w > budget {
            break;
        }
        kept_w += w;
        kept_start_char = i;
    }
    let kept: String = chars[kept_start_char..].iter().collect();
    let truncated = format!("…{kept}");
    // Shift indices: drop those falling before `kept_start_char`; the rest are offset by
    // `-(kept_start_char) + 1` (the `…` prefix is char 0).
    let new_indices: Vec<u32> = match_indices
        .iter()
        .copied()
        .filter(|&i| (i as usize) >= kept_start_char)
        .map(|i| ((i as usize - kept_start_char) + 1) as u32)
        .collect();
    (truncated, new_indices)
}

fn draw_buffer(f: &mut Frame, state: &AppState, area: Rect) {
    // When the buffer is taller than the viewport, carve the rightmost column for a scrollbar
    // (drawn last, below). The decision uses the whole-buffer `total_visual_rows` from the
    // server's window, which is independent of this 1-col narrowing — so it can't flicker. The
    // narrowing clips content by one column rather than reflowing it (the server wrapped to the
    // full width); acceptable, and only while the bar is shown.
    let total_visual_rows = state.ed().total_visual_rows;
    let needs_scrollbar = total_visual_rows as usize > area.height as usize;
    let area = if needs_scrollbar {
        Rect {
            width: area.width.saturating_sub(1),
            ..area
        }
    } else {
        area
    };

    let top = state.ed().scroll_logical_line;
    let selection = ordered_selection(&state.ed().cursor, state.ed().mode);
    let viewport_rows = area.height as usize;
    // The leftmost `GUTTER_WIDTH` cols are the change-bar gutter; content fills the rest. The
    // server already wrapped to this reduced width (the client reports it as `cols`).
    let viewport_cols = area.width.saturating_sub(GUTTER_WIDTH);
    let diff_view = state.ed().diff_view;
    // Horizontal scroll only kicks in for wrap-off; soft-wrapped content always fits horizontally.
    let scroll_col = if matches!(state.ed().wrap, WrapMode::None) {
        state.ed().scroll_col
    } else {
        0
    };

    // Blame for the cursor line, rendered as dim end-of-line virtual text. Only in Normal mode,
    // and only when the cached blame was fetched for the line the cursor is actually on (guards
    // against a one-frame mismatch right after the cursor moves).
    let cursor_line = state.ed().cursor.position.line;
    let blame_text: Option<String> = if matches!(state.ed().mode, EditorMode::Normal)
        && state.ed().blame.key.map(|(l, _)| l) == Some(cursor_line)
    {
        state.ed().blame.info.as_ref().map(format_blame)
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_rows);
    let mut logical_line = top;

    // Visual rows of the top logical line hidden above the viewport (sub-line scroll offset).
    // Clamp to the top line's height so it can only ever skip into that line, never bleed onto
    // the next — keeps scrolling robust if heights shift between a scroll and the next frame.
    let mut skip_rows = {
        let local = (top as i64) - (state.ed().window_first_logical_line as i64);
        if local >= 0 && (local as usize) < state.ed().lines.len() {
            let r = &state.ed().lines[local as usize];
            let h = (r.virtual_rows_above.len() + r.visual_rows.len().max(1)) as u32;
            state.ed().scroll_skip_rows.min(h.saturating_sub(1))
        } else {
            0
        }
    };

    'outer: loop {
        if lines.len() >= viewport_rows {
            break;
        }
        let local_idx = (logical_line as i64) - (state.ed().window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.ed().lines.len() as i64 {
            break;
        }
        let render = &state.ed().lines[local_idx as usize];

        // Inline diff: phantom "deleted" rows render above the line's real content. They occupy
        // screen rows (and so are counted here) but carry no cursor position. Each band is a
        // visible change, so it gets a red change-*bar* in the gutter (matching add/modify),
        // rather than the compact `▔` top-marker used when there's no band.
        for vrow in &render.virtual_rows_above {
            if skip_rows > 0 {
                skip_rows -= 1;
                continue;
            }
            if lines.len() >= viewport_rows {
                break 'outer;
            }
            let mut spans = deleted_virtual_row_spans(&vrow.text, viewport_cols, vrow.stage);
            // Deletion bar in the git gutter column: bright red unstaged, dimmed red staged.
            spans.insert(
                0,
                gutter_bar(stage_color(vrow.stage, NORD11, GIT_STAGED_DELETED)),
            );
            lines.push(Line::from(spans));
        }
        // The gutter change-bar reflects this line's marker (always on). With the diff view on, a
        // pure-deletion anchor's `▔` is redundant (the band above already shows it), so suppress
        // it. The diff-view background tint is separate and only applies while the view is on.
        let gutter_mark = match render.diff_marker {
            Some(DiffMarker::Deleted) if diff_view => None,
            other => other,
        };
        // The cursor's line gets a subtle current-line tint that applies to every visual row of the
        // logical line (so it stays whole under soft wrap). On a changed line under the diff view it
        // uses a green/olive cursorline variant so the diff colour isn't lost — the gutter change-bar
        // still marks it too. Selection and search keep their own span backgrounds, so they paint
        // over the tint via `apply_line_tint`.
        let line_tint = if logical_line == cursor_line {
            let marker = if diff_view { render.diff_marker } else { None };
            Some(cursor_line_bg(marker, render.diff_stage))
        } else if diff_view {
            render
                .diff_marker
                .and_then(|m| diff_marker_bg(m, render.diff_stage))
        } else {
            None
        };

        let last_vrow_idx = render.visual_rows.len().saturating_sub(1);
        // A diagnostic clamped to the line end (e.g. "expected ;") sits at byte `line_end` with no
        // real char to underline — its worst severity, so we can mark the EOL cell (where the
        // newline glyph sits) instead. `None` when no diagnostic reaches the line end.
        let eol_diag_at = |line_end: u32| -> Option<DiagnosticSeverity> {
            render
                .diagnostics
                .iter()
                .filter(|d| d.start >= line_end)
                .map(|d| d.severity)
                .max_by_key(|s| severity_rank(*s))
        };
        for (vrow_idx, vrow) in render.visual_rows.iter().enumerate() {
            if skip_rows > 0 {
                skip_rows -= 1;
                continue; // hidden above the viewport by the sub-line scroll offset
            }
            if lines.len() >= viewport_rows {
                break 'outer;
            }
            let is_last_vrow_of_line = vrow_idx == last_vrow_idx;
            let segment = match vrow.segments.first() {
                Some(s) => s,
                None => {
                    // Empty line — paint a trailing cell when the line's newline (at col 0) falls
                    // in the selection: the range starts at/before this line and ends at/after it.
                    // `>=` (not `>`) so a selection ending *on* the empty line — including a point
                    // cursor parked there — still highlights its newline.
                    let empty_newline_selected = is_last_vrow_of_line
                        && selection
                            .is_some_and(|(s, e)| s.line <= logical_line && e.line >= logical_line);
                    // An empty line's newline is at byte 0; a diagnostic there underlines the cell.
                    let eol_diag = is_last_vrow_of_line
                        .then(|| eol_diag_at(vrow.byte_offset))
                        .flatten();
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if empty_newline_selected || eol_diag.is_some() {
                        let mut style = if empty_newline_selected {
                            Style::default().bg(NORD10).fg(NORD3)
                        } else {
                            Style::default()
                        };
                        if let Some(sev) = eol_diag {
                            style = style
                                .add_modifier(Modifier::UNDERLINED)
                                .underline_color(diag_color(sev));
                        }
                        spans.push(Span::styled(
                            if empty_newline_selected { "↵" } else { " " },
                            style,
                        ));
                    }
                    let show_blame = logical_line == cursor_line && is_last_vrow_of_line;
                    append_eol_blame(
                        &mut spans,
                        show_blame.then_some(blame_text.as_deref()).flatten(),
                    );
                    apply_line_tint(&mut spans, line_tint, viewport_cols);
                    lines.push(prepend_gutter(gutter_mark, render.diff_stage, spans));
                    continue;
                }
            };
            let row_text_len = segment.text.len() as u32;
            // The trailing "newline cell" represents the line's implicit `\n` and is painted
            // when that `\n` falls inside the selection. The `\n` is at byte col
            // `line_text_len` (just past the last char); the selection covers it when either:
            //   - the selection continues past this whole line (`e.line > logical_line`), or
            //   - the cursor / anchor sits *on* the `\n` cell (`e.col >= line_text_len`) —
            //     not merely on the last real char.
            let highlight_trailing_newline = is_last_vrow_of_line
                && selection.is_some_and(|(s, e)| {
                    s.line <= logical_line
                        && (e.line > logical_line
                            || (e.line == logical_line && e.col >= vrow.byte_offset + row_text_len))
                });
            let sel_on_row = selection.and_then(|(s, e)| {
                selection_on_visual_row(logical_line, vrow.byte_offset, row_text_len, s, e)
            });
            let matches_on_row =
                matches_on_visual_row(vrow.byte_offset, row_text_len, &render.search_matches);
            let diags_on_row =
                diagnostics_on_visual_row(vrow.byte_offset, row_text_len, &render.diagnostics);
            let brackets_on_row = bracket_positions_on_visual_row(
                logical_line,
                vrow.byte_offset,
                row_text_len,
                state.ed().cursor.match_bracket,
            );

            // Apply horizontal scroll to the row's text + highlights + selection. Skips zero
            // bytes when scroll_col == 0 (the common case), so this is a no-op under soft wrap.
            let (clipped_text, clipped_highlights, clipped_sel, clipped_matches, clipped_diags) =
                clip_horizontal(
                    &segment.text,
                    &segment.highlights,
                    sel_on_row,
                    &matches_on_row,
                    &diags_on_row,
                    scroll_col,
                );
            let clipped_brackets: Vec<u32> = brackets_on_row
                .iter()
                .filter(|b| **b >= scroll_col)
                .map(|b| b - scroll_col)
                .collect();

            // Continuation row when byte_offset > 0. Prepend the marker; the server already
            // reserved this width when wrapping.
            let is_continuation = vrow.byte_offset > 0;
            let marker_width = if is_continuation {
                CONTINUATION_MARKER_WIDTH
            } else {
                0
            };
            let indent = vrow.continuation_indent;
            let prefix_width = marker_width
                .saturating_add(indent)
                .min(viewport_cols as u32) as u16;
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
            spans.extend(build_spans(
                &clipped_text,
                &clipped_highlights,
                clipped_sel,
                &clipped_matches,
                &clipped_brackets,
                &clipped_diags,
                body_width,
            ));
            // The EOL cell after the last char: the newline glyph when selected, and/or a
            // diagnostic underline when one is clamped to the line end (it has no real char to
            // mark). When neither applies, nothing is drawn here.
            let eol_diag = is_last_vrow_of_line
                .then(|| eol_diag_at(vrow.byte_offset + row_text_len))
                .flatten();
            if highlight_trailing_newline || eol_diag.is_some() {
                let mut style = if highlight_trailing_newline {
                    Style::default().bg(NORD10).fg(NORD3)
                } else {
                    Style::default()
                };
                if let Some(sev) = eol_diag {
                    style = style
                        .add_modifier(Modifier::UNDERLINED)
                        .underline_color(diag_color(sev));
                }
                spans.push(Span::styled(
                    if highlight_trailing_newline {
                        "↵"
                    } else {
                        " "
                    },
                    style,
                ));
            }
            let show_blame = logical_line == cursor_line && is_last_vrow_of_line;
            append_eol_blame(
                &mut spans,
                show_blame.then_some(blame_text.as_deref()).flatten(),
            );
            apply_line_tint(&mut spans, line_tint, viewport_cols);
            lines.push(prepend_gutter(gutter_mark, render.diff_stage, spans));
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

    // The editor scrollbar, in the column reserved above. Same glyphs/colours as the picker
    // and overlays. `top_visual_row` is the absolute viewport-top row; the thumb reflects how
    // far through the whole buffer that is.
    if needs_scrollbar {
        let scrollbar = Rect {
            x: area.x + area.width,
            y: area.y,
            width: 1,
            height: area.height,
        };
        render_scrollbar(
            f,
            scrollbar,
            u64::from(state.ed().top_visual_row),
            u64::from(total_visual_rows),
            area.height as u64,
        );
    }
}

/// The content spans of one inline-diff phantom row: the removed baseline line, red on a dark-red
/// fill that spans the content width so the deletion reads as a distinct band. Tabs expand to
/// spaces for stable width; content wider than the viewport is clipped. The gutter cell is added
/// separately by [`prepend_gutter`].
fn deleted_virtual_row_spans(text: &str, width: u16, stage: DiffStage) -> Vec<Span<'static>> {
    let expanded = text.replace('\t', &" ".repeat(TAB_WIDTH as usize));
    let mut shown: String = expanded.chars().take(width as usize).collect();
    let used = shown.chars().count();
    shown.push_str(&" ".repeat((width as usize).saturating_sub(used)));
    let style = if stage == DiffStage::Staged {
        Style::default()
            .fg(GIT_STAGED_DELETED)
            .bg(GIT_STAGED_DELETED_BG)
    } else {
        Style::default().fg(NORD11).bg(GIT_DELETED_BG)
    };
    vec![Span::styled(shown, style)]
}

/// A solid change-bar cell in the given color (`GUTTER_WIDTH` cols).
fn gutter_bar(color: Color) -> Span<'static> {
    Span::styled("▎".to_string(), Style::default().fg(color))
}

/// Colour for a change-bar / marker: hue follows the change kind (`bright` and `dim` are the
/// unstaged/staged variants of the same hue), brightness follows the stage — bright still needs
/// staging, dim is in the index.
fn stage_color(stage: DiffStage, bright: Color, dim: Color) -> Color {
    match stage {
        DiffStage::Unstaged => bright,
        DiffStage::Staged => dim,
    }
}

/// The git column of the gutter: a colored bar for added/modified lines, a top marker for a line
/// with deletions just above it, or blank. One col wide. The stage dims the kind colour when the
/// change is staged.
fn git_gutter_cell(mark: Option<DiffMarker>, stage: DiffStage) -> Span<'static> {
    match mark {
        Some(DiffMarker::Added) => gutter_bar(stage_color(stage, NORD14, GIT_STAGED_ADDED)),
        Some(DiffMarker::Modified) => gutter_bar(stage_color(stage, NORD13, GIT_STAGED_MODIFIED)),
        Some(DiffMarker::Deleted) => {
            // "removed above" top marker
            Span::styled(
                "▔".to_string(),
                Style::default().fg(stage_color(stage, NORD11, GIT_STAGED_DELETED)),
            )
        }
        None => Span::styled(" ".to_string(), Style::default().fg(NORD0)), // unchanged → blank
    }
}

/// Prepend the gutter cell (git change column) to a row's content spans, producing the final `Line`.
fn prepend_gutter(
    mark: Option<DiffMarker>,
    stage: DiffStage,
    mut spans: Vec<Span<'static>>,
) -> Line<'static> {
    spans.insert(0, git_gutter_cell(mark, stage));
    Line::from(spans)
}

/// The background tint for an inline-diff line: added/modified get a tint, deleted-anchor lines
/// (unchanged content) get none. A staged line gets the dimmer variant of its kind tint.
fn diff_marker_bg(marker: DiffMarker, stage: DiffStage) -> Option<Color> {
    match (marker, stage) {
        (DiffMarker::Deleted, _) => None,
        (DiffMarker::Added, DiffStage::Staged) => Some(GIT_STAGED_ADDED_BG),
        (DiffMarker::Modified, DiffStage::Staged) => Some(GIT_STAGED_MODIFIED_BG),
        (DiffMarker::Added, _) => Some(GIT_ADDED_BG),
        (DiffMarker::Modified, _) => Some(GIT_MODIFIED_BG),
    }
}

/// Background tint for the cursor's current line. On an added/modified line (diff view on) it's a
/// green/olive cursorline variant so the line still reads as changed — dimmed further when the
/// change is staged, matching the tint scheme; otherwise the plain cursorline.
fn cursor_line_bg(diff_marker: Option<DiffMarker>, stage: DiffStage) -> Color {
    match (diff_marker, stage) {
        (Some(DiffMarker::Added), DiffStage::Staged) => CURSOR_LINE_STAGED_ADDED_BG,
        (Some(DiffMarker::Modified), DiffStage::Staged) => CURSOR_LINE_STAGED_MODIFIED_BG,
        (Some(DiffMarker::Added), _) => CURSOR_LINE_ADDED_BG,
        (Some(DiffMarker::Modified), _) => CURSOR_LINE_MODIFIED_BG,
        _ => CURSOR_LINE_BG,
    }
}

/// Tint a real line's row with its diff-marker background: set the tint behind every span that
/// doesn't already carry its own background (so syntax fg shows through, but selection/search
/// highlights keep their backgrounds), then fill to the right edge so the tint spans the row.
/// No-op when `tint` is `None`.
fn apply_line_tint(spans: &mut Vec<Span<'static>>, tint: Option<Color>, width: u16) {
    let Some(bg) = tint else { return };
    for span in spans.iter_mut() {
        if span.style.bg.is_none() {
            span.style = span.style.bg(bg);
        }
    }
    // Over-long fill is clipped by the Paragraph; this just guarantees we reach the right edge.
    spans.push(Span::styled(
        " ".repeat(width as usize),
        Style::default().bg(bg),
    ));
}

/// Append `blame` as dim, italic end-of-line virtual text with a few cols of lead-in. The
/// Paragraph clips to the viewport width, so on a line that already fills the screen the blame
/// simply shows less (or nothing) — no wrapping, no overwriting code.
fn append_eol_blame(spans: &mut Vec<Span<'static>>, blame: Option<&str>) {
    if let Some(text) = blame {
        spans.push(Span::styled(
            format!("    {text}"),
            Style::default().fg(NORD3).add_modifier(Modifier::ITALIC),
        ));
    }
}

/// One-line blame label: `author · 3 days ago`, or a plain marker for a line the user has edited
/// but not committed. The commit message lives in the `Space o` details popover, not inline.
fn format_blame(info: &BlameInfo) -> String {
    if info.is_uncommitted {
        return "You · Uncommitted".to_string();
    }
    format!("{} · {}", info.author, relative_time(info.timestamp))
}

/// Coarse "N units ago" rendering of a Unix timestamp against the wall clock. Future timestamps
/// (clock skew) and the last minute both read as "just now".
fn relative_time(timestamp: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - timestamp).max(0);

    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    let (n, unit) = if secs < MIN {
        return "just now".to_string();
    } else if secs < HOUR {
        (secs / MIN, "minute")
    } else if secs < DAY {
        (secs / HOUR, "hour")
    } else if secs < WEEK {
        (secs / DAY, "day")
    } else if secs < MONTH {
        (secs / WEEK, "week")
    } else if secs < YEAR {
        (secs / MONTH, "month")
    } else {
        (secs / YEAR, "year")
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Drop the first `scroll_col` bytes of the row's text, then shift highlight + selection + match
/// ranges to match the new origin. Anything fully scrolled off the left is filtered out.
#[allow(clippy::type_complexity)]
fn clip_horizontal(
    text: &str,
    highlights: &[Highlight],
    sel: Option<(u32, u32)>,
    matches: &[(u32, u32)],
    diags: &[(u32, u32, DiagnosticSeverity)],
    scroll_col: u32,
) -> (
    String,
    Vec<Highlight>,
    Option<(u32, u32)>,
    Vec<(u32, u32)>,
    Vec<(u32, u32, DiagnosticSeverity)>,
) {
    if scroll_col == 0 {
        return (
            text.to_string(),
            highlights.to_vec(),
            sel,
            matches.to_vec(),
            diags.to_vec(),
        );
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
            Some(Highlight {
                start: start as u32,
                end: end as u32,
                kind: h.kind.clone(),
            })
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
    let new_diags = diags
        .iter()
        .filter_map(|(s, e, sev)| shift_range((*s, *e)).map(|(s, e)| (s, e, *sev)))
        .collect();
    (
        clipped_text,
        new_highlights,
        new_sel,
        new_matches,
        new_diags,
    )
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

/// Clip per-logical-line diagnostic spans to this visual row's byte range, returning row-relative
/// `(start, end, severity)`. A zero-width diagnostic within the row is widened to one cell so it's
/// visible; a diagnostic ending exactly at the row's end (its `\n`) is dropped (nothing to draw).
fn diagnostics_on_visual_row(
    row_byte_offset: u32,
    row_text_len: u32,
    diags: &[DiagnosticSpan],
) -> Vec<(u32, u32, DiagnosticSeverity)> {
    if row_text_len == 0 {
        return Vec::new();
    }
    let row_end = row_byte_offset + row_text_len;
    diags
        .iter()
        .filter_map(|d| {
            let s = d.start.max(row_byte_offset);
            let e = d.end.min(row_end);
            if e > s {
                Some((s - row_byte_offset, e - row_byte_offset, d.severity))
            } else if d.start == d.end && d.start >= row_byte_offset && d.start < row_end {
                // Zero-width (point) diagnostic: underline the single cell at its position.
                let p = d.start - row_byte_offset;
                Some((p, p + 1, d.severity))
            } else {
                None
            }
        })
        .collect()
}

/// The underline / message color for a diagnostic severity.
fn diag_color(severity: DiagnosticSeverity) -> Color {
    match severity {
        DiagnosticSeverity::Error => NORD11,      // red
        DiagnosticSeverity::Warning => NORD13,    // yellow
        DiagnosticSeverity::Information => NORD8, // frost blue
        // Near-white: readable on the status/popover backgrounds and distinct from the coloured
        // severities (was NORD3 dim gray, which was hard to read).
        DiagnosticSeverity::Hint => NORD4,
    }
}

/// Severity glyph, shared by the status-bar count, the diagnostics picker, and the hover popover so
/// all three match within the terminal client. Hint uses a hollow circle `○`.
fn diag_glyph(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "⊗",
        DiagnosticSeverity::Warning => "⚠",
        DiagnosticSeverity::Information => "ⓘ",
        DiagnosticSeverity::Hint => "○",
    }
}

/// Ordering for "most important" severity (Error highest), so a line with several diagnostics shows
/// its worst one's message and a cell underneath several picks the worst color.
fn severity_rank(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 3,
        DiagnosticSeverity::Warning => 2,
        DiagnosticSeverity::Information => 1,
        DiagnosticSeverity::Hint => 0,
    }
}

/// For the visual row at `(logical_line, row_byte_offset..row_byte_offset+row_text_len)`,
/// return the row-relative byte offsets of any match-bracket positions on it. Used to overlay
/// the bracket-pair highlight on whichever rows actually contain the brackets.
fn bracket_positions_on_visual_row(
    logical_line: u32,
    row_byte_offset: u32,
    row_text_len: u32,
    pair: Option<(LogicalPosition, LogicalPosition)>,
) -> Vec<u32> {
    let Some((a, b)) = pair else {
        return Vec::new();
    };
    let row_end = row_byte_offset + row_text_len;
    [a, b]
        .iter()
        .filter_map(|p| {
            if p.line == logical_line && p.col >= row_byte_offset && p.col < row_end {
                Some(p.col - row_byte_offset)
            } else {
                None
            }
        })
        .collect()
}

/// The cursor's selection as an inclusive `(lo, hi)` range. A range cursor (anchor != position)
/// always yields one. A *point* cursor is the 1-char selection of the char under it — yielded only
/// in Normal mode, where the block cursor represents exactly that span, so the char's selection
/// highlight + whitespace/newline indicator (`→`/`·`/`↵`) render the same as inside a multi-char
/// selection. In Insert/Search the cursor is a bar (a gap between chars), not a selection, so a
/// point yields `None`.
fn ordered_selection(
    cursor: &CursorState,
    mode: EditorMode,
) -> Option<(LogicalPosition, LogicalPosition)> {
    let p = cursor.position;
    if cursor.is_point() {
        // A point is a single-char selection (Helix-style). Render it in Normal mode, and also in
        // Search mode so a one-char selection stays visible while the search input has focus —
        // multi-char ranges already show there (the range path below ignores mode), and a point
        // shouldn't be the exception. Insert mode stays caret-only (no selection block).
        return matches!(mode, EditorMode::Normal | EditorMode::Search).then_some((p, p));
    }
    let anchor = cursor.anchor;
    if (p.line, p.col) <= (anchor.line, anchor.col) {
        Some((p, anchor))
    } else {
        Some((anchor, p))
    }
}

/// Intersect the selection with the byte range covered by `[row_byte_offset, +row_text_len)` on
/// `logical_line`. Returns row-relative offsets. The selection is inclusive on both endpoints
/// (per the protocol), so the returned range's exclusive end is `sel_end.col + 1` — meaning the
/// last selected char is included in the paint. The block cursor is later overlaid by the
/// terminal on whichever cell its position lands on.
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
    let line_sel_start = if logical_line == sel_start.line {
        sel_start.col
    } else {
        0
    };
    let line_sel_end_excl = if logical_line == sel_end.line {
        sel_end.col + 1
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
    match_brackets: &[u32],
    diagnostics: &[(u32, u32, DiagnosticSeverity)],
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
        for kind in &mut byte_kind[s..e] {
            *kind = Some(h.kind.as_str());
        }
    }

    let mut byte_in_match: Vec<bool> = vec![false; trunc_len];
    for (s, e) in matches {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        for in_match in &mut byte_in_match[s..e] {
            *in_match = true;
        }
    }

    let mut byte_is_match_bracket: Vec<bool> = vec![false; trunc_len];
    for &b in match_brackets {
        let idx = (b as usize).min(trunc_len);
        if idx < trunc_len {
            byte_is_match_bracket[idx] = true;
        }
    }

    // Per-byte diagnostic severity (worst wins where they overlap), so we can underline each cell in
    // its severity color.
    let mut byte_diag: Vec<Option<DiagnosticSeverity>> = vec![None; trunc_len];
    for (s, e, sev) in diagnostics {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        for slot in byte_diag.iter_mut().take(e).skip(s) {
            if slot.is_none_or(|cur| severity_rank(*sev) > severity_rank(cur)) {
                *slot = Some(*sev);
            }
        }
    }

    let style_at = |byte_idx: usize| -> Style {
        let mut style = byte_kind[byte_idx].map(theme_for).unwrap_or_default();
        // Match-bracket overlay: bold + NORD12 (Aurora orange). The only warm tone in our
        // palette, so it reads as a distinct "this bracket pairs with the cursor" signal
        // without colliding with the frost-blue accents used elsewhere. Painted before search
        // and selection so those (which use bg) still win when stacked.
        if byte_is_match_bracket[byte_idx] {
            style = style.fg(NORD12).add_modifier(Modifier::BOLD);
        }
        // Search match: a quiet NORD3 fill behind the normal syntax text. NORD3 is the brightest of
        // Nord's Polar Night shades, so it stays visible on the NORD1 current-line tint while still
        // sitting clearly below the more saturated NORD10 selection, which paints over it.
        if byte_in_match[byte_idx] {
            style = style.bg(NORD3);
            // Comments are themed NORD3 too, so a match inside one would be invisible (same fg/bg).
            // Lift just that text to the normal foreground; every other syntax color reads fine.
            if style.fg == Some(NORD3) {
                style = style.fg(NORD4);
            }
        }
        if let Some((s, e)) = sel {
            if byte_idx >= s as usize && byte_idx < e as usize {
                style = style.bg(NORD10);
            }
        }
        // Diagnostic underline, colored by severity. Drawn last so it layers over selection/match
        // backgrounds without disturbing the foreground syntax color. Terminals without colored
        // underlines fall back to a plain underline.
        if let Some(sev) = byte_diag[byte_idx] {
            style = style
                .add_modifier(Modifier::UNDERLINED)
                .underline_color(diag_color(sev));
        }
        style
    };

    // Byte offset at which trailing whitespace starts on the row. If the row is all
    // whitespace this is 0; if there's no trailing whitespace it's the row length.
    let trailing_ws_start = {
        let bytes = truncated.as_bytes();
        let mut i = bytes.len();
        while i > 0 && matches!(bytes[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        i
    };

    // Walk char-by-char so we can substitute tabs with the right number of spaces — ratatui
    // would render a raw `\t` as a single zero-width control glyph and the rest of the line
    // would visually collapse. Track `display_col` to size each tab to the next tab stop;
    // highlight/selection byte ranges still apply to the *original* byte positions so they
    // keep working untouched. Selected whitespace (tabs, trailing spaces) gets a muted
    // indicator glyph (NORD3) overlaid on the selection bg — `→` for tabs, `·` for trailing
    // spaces — so the user can see the structure of what they've selected.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;
    let mut display_col: u32 = 0;
    for (byte_idx, c) in truncated.char_indices() {
        let style = style_at(byte_idx);
        let in_sel = sel.is_some_and(|(s, e)| byte_idx >= s as usize && byte_idx < e as usize);
        let pad = if c == '\t' {
            TAB_WIDTH - (display_col % TAB_WIDTH)
        } else {
            0
        };
        display_col += char_display_width(c, display_col);
        if c == '\t' {
            if in_sel {
                push_text(
                    &mut spans,
                    &mut current_text,
                    &mut current_style,
                    "→",
                    style.fg(NORD3),
                );
                if pad > 1 {
                    let pad_str = " ".repeat((pad - 1) as usize);
                    push_text(
                        &mut spans,
                        &mut current_text,
                        &mut current_style,
                        &pad_str,
                        style,
                    );
                }
            } else {
                let pad_str = " ".repeat(pad as usize);
                push_text(
                    &mut spans,
                    &mut current_text,
                    &mut current_style,
                    &pad_str,
                    style,
                );
            }
        } else if c == ' ' && in_sel && byte_idx >= trailing_ws_start {
            push_text(
                &mut spans,
                &mut current_text,
                &mut current_style,
                "·",
                style.fg(NORD3),
            );
        } else {
            let rendered = &truncated[byte_idx..byte_idx + c.len_utf8()];
            push_text(
                &mut spans,
                &mut current_text,
                &mut current_style,
                rendered,
                style,
            );
        }
    }
    if let Some(s) = current_style {
        spans.push(Span::styled(current_text, s));
    }
    spans
}

/// Append `text` to the running span, flushing the previous span if `style` differs from the
/// current accumulated style. Keeps adjacent chars of the same style in one span so ratatui
/// doesn't waste cells on style transitions.
fn push_text(
    spans: &mut Vec<Span<'static>>,
    current_text: &mut String,
    current_style: &mut Option<Style>,
    text: &str,
    style: Style,
) {
    match *current_style {
        Some(s) if s != style => {
            spans.push(Span::styled(std::mem::take(current_text), s));
            *current_style = Some(style);
        }
        None => *current_style = Some(style),
        _ => {}
    }
    current_text.push_str(text);
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
    let line = if let Some(confirm) = state.confirm_prompt.as_ref() {
        // Confirm prompt always wins the status row — it can layer over save_prompt.
        Line::from(vec![Span::raw(format!(" {}? [y/N]", confirm.message))])
    } else if let Some(prompt) = state.save_prompt.as_ref() {
        // Save-prompt overlay: status row hosts the prompt regardless of underlying screen.
        Line::from(draw_save_prompt_spans(prompt, state, area.width as usize))
    } else if !state.has_editor() {
        // No editor: status row only shows transient feedback (project activation, errors).
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                state.status.text.clone(),
                status_message_style(&state.status),
            ),
        ])
    } else if matches!(state.ed().mode, EditorMode::Search) {
        let prompt = format!("/{}", state.ed().search.query.text);
        let text = match search_match_count_label(state) {
            Some(count) => format!("{prompt}    {count}"),
            None => prompt,
        };
        Line::from(vec![Span::raw(text)])
    } else {
        // Project / file / dirty-dot / transient status sit on the left; counter (search and/or
        // grep, in that order) and cursor position sit on the right, with the counter to the
        // left of the position. When the row is narrow we truncate the right edge of the left
        // segment with `…` so the right segment stays whole and the position never gets
        // painted over.
        let project_prefix = format!("[{}] ", state.project_name);
        // Buffer-state dot just after the file label — colour-coded (unsaved / changed / deleted
        // on disk), matching the web client's favicon colours.
        let status_dot = state.buffer_status().map(|kind| {
            Span::styled(
                BUFFER_STATUS_DOT.to_string(),
                Style::default().bg(NORD1).fg(buffer_status_color(kind)),
            )
        });

        // Left: the Git change counts sit next to the file label (they're about the file's VCS
        // state). Diagnostics moved to the right segment, by the position indicator.
        let git_spans = git_status_spans(state);

        // Right segment, left→right: search/grep counters, diagnostic counts, the position /
        // selection indicator, then the LSP glyph pinned to the far edge. A double space precedes
        // each group so they don't run together.
        let base = Style::default().bg(NORD1).fg(NORD4);
        let mut right_spans: Vec<Span<'static>> = Vec::new();
        let gap = |spans: &mut Vec<Span<'static>>| {
            if !spans.is_empty() {
                spans.push(Span::styled("  ".to_string(), base));
            }
        };
        let counter_parts: Vec<String> = [search_counter_label(state), grep_counter_label(state)]
            .into_iter()
            .flatten()
            .collect();
        if !counter_parts.is_empty() {
            right_spans.push(Span::styled(counter_parts.join(" "), base));
        }
        let diag_spans = diagnostic_count_spans(state);
        if !diag_spans.is_empty() {
            gap(&mut right_spans);
            right_spans.extend(diag_spans);
        }
        gap(&mut right_spans);
        right_spans.push(Span::styled(format_position(state), base));
        if let Some(glyph) = lsp_indicator_span(state) {
            // Leading gap + trailing space give the fat `●` room at the screen edge.
            right_spans.push(Span::styled(" ".to_string(), base));
            right_spans.push(glyph);
            right_spans.push(Span::styled(" ".to_string(), base));
        }

        Line::from(build_editor_status_spans(
            StatusLabel {
                project_prefix: &project_prefix,
                file_label: &state.ed().file_label,
                transient: state.ed().transient,
            },
            status_dot,
            git_spans,
            // The transient message now floats as a toast (see `draw_toast_overlay`), so it's kept
            // out of the status row — the bar shows only the persistent project / file / git info.
            &crate::app::StatusMessage::default(),
            right_spans,
            area.width as usize,
        ))
    };
    let p = Paragraph::new(line).style(Style::default().bg(NORD1).fg(NORD4));
    f.render_widget(p, area);
}

/// Accent colour for a toast's left bar — matches the web/native toast border colours
/// (info → frost blue, success → green, warning → yellow, error → red).
fn toast_accent_color(kind: crate::app::StatusKind) -> Color {
    use crate::app::StatusKind;
    match kind {
        StatusKind::Info => NORD8,
        StatusKind::Success => NORD14,
        StatusKind::Warning => NORD13,
        StatusKind::Error => NORD11,
    }
}

/// Floating toasts stacked in the bottom-right of `area`: each is a fat status-coloured left bar
/// followed by its message on a tinted background — deliberately subtle (no full outline), mirroring
/// the web/native transient toasts. The newest sits at the bottom; older ones stack upward with a
/// blank gap row between them (until they run out of vertical room). The shell expires each on a TTL
/// timer, so they auto-dismiss.
fn draw_toast_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    const BAR_W: u16 = 1; // a solid accent-coloured cell — the "fat" left bar
    const PAD: u16 = 1; // one space between the bar and the text, and after the text
    const MARGIN_X: u16 = 2;
    const MARGIN_Y: u16 = 1;
    const GAP: u16 = 1; // blank row between stacked toasts
    if state.toasts.is_empty() || area.height <= MARGIN_Y {
        return;
    }
    let max_text =
        (area.width as usize).saturating_sub((BAR_W + PAD * 2 + MARGIN_X * 2) as usize);
    if max_text == 0 {
        return;
    }
    // Newest toast hugs the bottom; older ones march upward a row + gap at a time.
    let mut y = area.y + area.height.saturating_sub(1 + MARGIN_Y);
    for toast in state.toasts.iter().rev() {
        let text = if toast.text.width() <= max_text {
            toast.text.clone()
        } else {
            truncate_to_width(&toast.text, max_text)
        };
        let box_w = BAR_W + PAD + text.width() as u16 + PAD;
        let rect = Rect {
            x: area.x + area.width.saturating_sub(box_w + MARGIN_X),
            y,
            width: box_w,
            height: 1,
        };
        f.render_widget(Clear, rect);
        let tint = Style::default().bg(NORD2).fg(NORD6);
        let spans = vec![
            Span::styled(
                " ".to_string(),
                Style::default().bg(toast_accent_color(toast.kind)),
            ),
            Span::styled(" ".to_string(), tint),
            Span::styled(text, tint),
            Span::styled(" ".to_string(), tint),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)).style(tint), rect);
        // Step up for the next (older) toast; stop once there's no room left in the area.
        if y < area.y + 1 + GAP {
            break;
        }
        y -= 1 + GAP;
    }
}

/// Display width of the save-prompt's committed root prefix. Only non-zero in multi-root
/// `Editing` mode (where we render `{label}: ` before the input). In SelectingRoot we show no
/// label — the input itself carries the typed root filter / cycled-root suggestion. Used by
/// the terminal-cursor placement to land in sync with the rendered text.
fn save_prompt_prefix_width(
    prompt: &crate::save_prompt::SavePromptState,
    state: &AppState,
) -> usize {
    use crate::save_prompt::PromptMode;
    match &prompt.mode {
        PromptMode::Editing(e) => {
            let label = state
                .root_labels
                .get(e.path_index as usize)
                .map(String::as_str)
                .unwrap_or("");
            if label.is_empty() {
                0
            } else {
                label.width() + ": ".width()
            }
        }
        PromptMode::SelectingRoot(_) => 0,
    }
}

/// Build the save-prompt's status-row spans. In multi-root projects we render a blue committed
/// root label to the left of the input (e.g. `proj_a: `); in single-root we skip it. After the
/// input, when the prompt has a ghost suggestion to offer (cursor at end, at least one match),
/// the dim suffix completing the user's partial leaf is appended in gray. A right-aligned
/// `[N/M]` counter appears when the filtered match set has more than one entry.
fn draw_save_prompt_spans(
    prompt: &crate::save_prompt::SavePromptState,
    state: &AppState,
    total_width: usize,
) -> Vec<Span<'static>> {
    use crate::save_prompt::PromptMode;
    let base_style = Style::default().bg(NORD1).fg(NORD4);
    // The committed root prefix (multi-root only) shares the explorer's blue treatment.
    let prefix_style = Style::default().bg(NORD1).fg(NORD8);
    // Ghost / suggestion text. We can't use NORD3 — it's only ~17 brightness off NORD1 and
    // reads as invisible on the status bar. We also can't rely on the `DIM` modifier — some
    // terminals ignore it for bright foregrounds. So we explicitly pick a mid-tone that's
    // clearly readable on NORD1 yet plainly dimmer than NORD4.
    let dim_style = Style::default().bg(NORD1).fg(Color::Rgb(140, 150, 165));

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(" save as: ".to_string(), base_style));

    // Root label, in multi-root projects only. In SelectingRoot mode the label has been peeled
    // — we show nothing to the left of the input; the input itself carries the typed root-
    // filter or the cycled-root suggestion.
    let label_text = match &prompt.mode {
        PromptMode::Editing(e) => {
            let label = state
                .root_labels
                .get(e.path_index as usize)
                .map(String::as_str)
                .unwrap_or("");
            if label.is_empty() {
                String::new()
            } else {
                format!("{label}: ")
            }
        }
        PromptMode::SelectingRoot(_) => String::new(),
    };
    let label_w = label_text.width();
    if !label_text.is_empty() {
        spans.push(Span::styled(label_text, prefix_style));
    }

    // The input itself, always in the base (white) style — the user's typed text never gets
    // dimmed. The committed/uncommitted contrast lives in the prefix (blue) and the ghost
    // (dim) instead.
    spans.push(Span::styled(prompt.input.text.clone(), base_style));

    // Ghost suggestion: gray suffix after the cursor when one's available.
    let ghost = prompt
        .ghost_suffix(&state.project_paths)
        .unwrap_or_default();
    let ghost_w = ghost.width();
    if !ghost.is_empty() {
        spans.push(Span::styled(ghost, dim_style));
    }

    // Right-aligned cycle counter (`N/M`) — only meaningful when the user has more than one
    // candidate to choose between.
    if let Some((pos, total)) = prompt.cycle_position(&state.project_paths) {
        let counter = format!("[{pos}/{total}]");
        let counter_w = counter.width();
        let used = " save as: ".width() + label_w + prompt.input.text.width() + ghost_w;
        if used + counter_w < total_width {
            let pad = total_width.saturating_sub(used + counter_w);
            spans.push(Span::styled(" ".repeat(pad), base_style));
            spans.push(Span::styled(counter, base_style));
        }
    }
    spans
}

/// Style for a `StatusMessage` based on its kind: success → blue (matches the committed-prefix
/// blue elsewhere in the UI), warning → yellow, error → red, info → default white. Background
/// stays NORD1 to blend with the surrounding status bar.
fn status_message_style(msg: &crate::app::StatusMessage) -> Style {
    use crate::app::StatusKind;
    let fg = match msg.kind {
        StatusKind::Info => NORD4,
        StatusKind::Success => NORD8,
        StatusKind::Warning => NORD13,
        StatusKind::Error => NORD11,
    };
    Style::default().bg(NORD1).fg(fg)
}

/// The status row's leading label: an optional `[project] ` prefix, the file label, and whether
/// the buffer is transient (which italicises the label).
struct StatusLabel<'a> {
    project_prefix: &'a str,
    file_label: &'a str,
    transient: bool,
}

/// Build the spans for the default editor status row: an optional leading buffer-state dot, then
/// `left_pre` (project/file) in the base style, an optional colored status message after a `    `
/// separator, then padding pushing the right segment flush to the row edge. When the row is too
/// narrow:
/// - the status text truncates first (`…`), preserving the dot and project/file;
/// - if even `left_pre` can't fit, that gets truncated and the status is dropped entirely.
///
/// The right segment is never truncated — the cursor position is more useful than the message.
fn build_editor_status_spans(
    label: StatusLabel<'_>,
    status_dot: Option<Span<'static>>,
    left_badges: Vec<Span<'static>>,
    status: &crate::app::StatusMessage,
    right_spans: Vec<Span<'static>>,
    total_width: usize,
) -> Vec<Span<'static>> {
    let StatusLabel {
        project_prefix,
        file_label,
        transient,
    } = label;
    let base_style = Style::default().bg(NORD1).fg(NORD4);
    // A transient (preview) buffer slants the file label (root + path — not the project name)
    // instead of spending row width on an explicit marker. Terminals without italic support
    // just show it upright.
    let label_style = if transient {
        base_style.add_modifier(Modifier::ITALIC)
    } else {
        base_style
    };
    // The right segment (counters / diagnostics / position / LSP glyph) is pre-built by the caller,
    // already including its internal gaps and the glyph's edge padding.
    let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
    // Always keep at least one cell of gap between the left content and the right segment.
    let left_max = total_width.saturating_sub(right_w + 1);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    // Buffer-state dot leads the row, before the project name — matching the terminal title and
    // the web favicon. Reserve its width (glyph + a trailing space) before laying out the rest.
    if let Some(dot) = status_dot {
        let dot_w = dot.content.width();
        if dot_w < left_max {
            spans.push(dot);
            spans.push(Span::styled(" ".to_string(), base_style));
            used += dot_w + 1;
        }
    }
    let pre_budget = left_max.saturating_sub(used);
    if project_prefix.width() + file_label.width() >= pre_budget {
        // Even the project/file segment overflows. The file label is the informative part, so
        // it gets the budget first (segment elision keeps the filename end visible); the
        // project prefix is shown only if it still fits whole — a partially-cut `[pr…` is
        // noise. The rest (badges, status) is dropped.
        let (t, _) = truncate_path_with_indices(file_label, &[], pre_budget);
        let prefix = if project_prefix.width() + t.width() <= pre_budget {
            project_prefix.to_string()
        } else {
            String::new()
        };
        used += prefix.width() + t.width();
        spans.push(Span::styled(prefix, base_style));
        spans.push(Span::styled(t, label_style));
    } else {
        spans.push(Span::styled(project_prefix.to_string(), base_style));
        spans.push(Span::styled(file_label.to_string(), label_style));
        used += project_prefix.width() + file_label.width();
        // Git cluster sits after the file label, set off by a 3-space gap.
        let badge_w: usize = left_badges.iter().map(|s| s.content.width()).sum();
        if badge_w > 0 && used + 3 + badge_w <= left_max {
            spans.push(Span::styled("   ".to_string(), base_style));
            used += 3;
            for s in left_badges {
                used += s.content.width();
                spans.push(s);
            }
        }
        // Status message after a separator, truncated to whatever's left.
        if !status.is_empty() {
            let separator = "    ";
            let remaining = left_max.saturating_sub(used + separator.width());
            if remaining > 0 {
                let text = if status.text.width() <= remaining {
                    status.text.clone()
                } else {
                    truncate_to_width(&status.text, remaining)
                };
                used += separator.width() + text.width();
                spans.push(Span::styled(separator.to_string(), base_style));
                spans.push(Span::styled(text, status_message_style(status)));
            }
        }
    }

    let pad_w = total_width.saturating_sub(used + right_w);
    spans.push(Span::styled(" ".repeat(pad_w), base_style));
    spans.extend(right_spans);
    spans
}

/// Accent colour for the buffer-state dot, matching the web client's favicon palette.
fn buffer_status_color(kind: BufferStatusKind) -> Color {
    match kind {
        BufferStatusKind::ExternallyDeleted => NORD11, // aurora red — gone on disk
        BufferStatusKind::ExternallyModified => NORD12, // aurora orange — changed on disk
        BufferStatusKind::Unsaved => NORD9,            // frost blue — unsaved edits
    }
}

/// Git change counts for the current buffer as colored spans (`+N` added / `~N` modified / `-N`
/// deleted, vs HEAD), matching the gutter change-bar colors. Empty when the buffer is clean,
/// untracked, or outside a repo. Segments are separated by a space; a class is shown only when its
/// count is non-zero.
/// The status-bar Git cluster for a tracked file: `⎇  branch  +u(s) ~u(s) -u(s)`. The branch is a
/// light, legible grey; each per-class count combines unstaged and staged as `+u(s)` — the
/// unstaged count then the staged count in parentheses, each omitted when zero (so `+1(2)` is
/// one unstaged + two staged additions, `+3` three unstaged, `+(3)` three staged). Empty classes
/// are skipped; the whole cluster is empty for files outside a repo. Reads `git_status`
/// (server-computed).
fn git_status_spans(state: &AppState) -> Vec<Span<'static>> {
    let bg = Style::default().bg(NORD1);
    let meta = bg.fg(NORD9); // branch / base: Frost blue — secondary, distinct from the nord4 path
    let mut parts: Vec<Span<'static>> = Vec::new();
    let Some(ed) = state.editor.as_ref() else {
        return parts;
    };
    let Some(status) = ed.git_status.as_ref() else {
        return parts;
    };
    if let Some(branch) = &status.branch {
        parts.push(Span::styled(format!("⎇  {branch}"), meta));
    }
    // Combined per-class counts: unstaged then `(staged)`.
    for (sigil, color, unstaged, staged) in [
        ('+', NORD14, status.unstaged.added, status.staged.added),
        (
            '~',
            NORD13,
            status.unstaged.modified,
            status.staged.modified,
        ),
        ('-', NORD11, status.unstaged.deleted, status.staged.deleted),
    ] {
        if unstaged == 0 && staged == 0 {
            continue;
        }
        let mut tok = sigil.to_string();
        if unstaged > 0 {
            tok.push_str(&unstaged.to_string());
        }
        if staged > 0 {
            tok.push_str(&format!("({staged})"));
        }
        parts.push(Span::styled(" ".to_string(), bg));
        parts.push(Span::styled(tok, bg.fg(color)));
    }
    parts
}

/// Diagnostic severity counts for the current buffer, worst-first, as colored spans (e.g. a red
/// `✗ 2`). Empty when the buffer has none. A space sits between each glyph and its count (the
/// `✗`/`⚠` glyphs read wide), and the severity segments are separated by a space.
fn diagnostic_count_spans(state: &AppState) -> Vec<Span<'static>> {
    let bg = Style::default().bg(NORD1);
    let mut parts: Vec<Span<'static>> = Vec::new();
    let Some(counts) = state
        .editor
        .as_ref()
        .and_then(|ed| state.diagnostic_counts.get(&ed.buffer_id))
    else {
        return parts;
    };
    for (n, severity) in [
        (counts.errors, DiagnosticSeverity::Error),
        (counts.warnings, DiagnosticSeverity::Warning),
        (counts.infos, DiagnosticSeverity::Information),
        (counts.hints, DiagnosticSeverity::Hint),
    ] {
        if n > 0 {
            if !parts.is_empty() {
                parts.push(Span::styled(" ".to_string(), bg));
            }
            parts.push(Span::styled(
                format!("{} {n}", diag_glyph(severity)),
                bg.fg(diag_color(severity)),
            ));
        }
    }
    parts
}

/// The far-right LSP health dot for the buffer's own server — the same state-coloured `•` the
/// LSP picker rows and detail title use. `None` when the buffer has no attached server or no
/// status yet. Keyed by the buffer's `(language, workspace_root)` so it's correct even when
/// several same-language servers run.
fn lsp_indicator_span(state: &AppState) -> Option<Span<'static>> {
    let server = state.editor.as_ref()?.lsp_server.as_ref()?;
    let status = state
        .lsp_status
        .get(&(server.language.clone(), server.workspace_root.clone()))?;
    // A ready server doing background work (`$/progress` — indexing, `cargo check`) shows the
    // busy colour, so the bar reflects that diagnostics/results may still land.
    let color = if matches!(status.status, LspStatus::Ready) && !status.progress.is_empty() {
        NORD13
    } else {
        lsp_status_color(&status.status)
    };
    Some(Span::styled(
        "•".to_string(),
        Style::default().bg(NORD1).fg(color),
    ))
}

/// State colour for a language-server's status dot (`•`) — shared by the status bar, the LSP
/// picker rows, and the detail title. The transitional states read as "busy" (the loop is
/// event-driven, so the colour changes when a `lsp/status_changed` arrives rather than animating).
fn lsp_status_color(status: &LspStatus) -> Color {
    match status {
        LspStatus::Ready => NORD14,
        LspStatus::Starting | LspStatus::Initializing | LspStatus::Restarting => NORD13,
        LspStatus::Crashed { .. } => NORD11,
        LspStatus::Stopped => NORD3,
    }
}

/// Truncate `s` so its display width is at most `max`, appending `…` when the input was longer.
/// Width-aware: handles double-wide CJK / emoji glyphs by skipping any char that wouldn't fit.
/// When `max` is too small to hold the ellipsis itself, falls back to a bare ellipsis (truncating
/// past the budget); when `max == 0`, returns empty.
fn truncate_to_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let ellipsis = "…";
    let ellipsis_w = ellipsis.width();
    if max <= ellipsis_w {
        return ellipsis.to_string();
    }
    let budget = max - ellipsis_w;
    let mut out = String::new();
    let mut acc = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > budget {
            break;
        }
        out.push(c);
        acc += cw;
    }
    out.push_str(ellipsis);
    out
}

/// In insert mode: `A:B` (just the cursor). In normal mode: `A:B-C:D` (half-open) — A:B is the
/// first byte of the selection, C:D is one byte past the last selected char. When the cursor /
/// anchor is *on the newline cell* of a line (col == line text length), the exclusive end
/// wraps to the next line's col 0 — matching the conceptual "the \n is the last selected
/// position". With no explicit anchor the selection is the implicit 1-char range at the
/// cursor.
fn format_position(state: &AppState) -> String {
    // Only called from the default status-bar branch which already guarantees Editing screen
    // with no save_prompt active.
    let ed = state.ed();
    let pos = ed.cursor.position;
    match ed.mode {
        EditorMode::Insert => format!("{}:{}", pos.line + 1, pos.col + 1),
        EditorMode::Normal | EditorMode::Search => {
            let anchor = state.ed().cursor.anchor;
            let (start, end_inclusive) = if (pos.line, pos.col) <= (anchor.line, anchor.col) {
                (pos, anchor)
            } else {
                (anchor, pos)
            };
            let excl = exclusive_end_of(state, end_inclusive);
            if start.line == excl.line {
                format!("{}:{}-{}", start.line + 1, start.col + 1, excl.col + 1)
            } else {
                format!(
                    "{}:{}-{}:{}",
                    start.line + 1,
                    start.col + 1,
                    excl.line + 1,
                    excl.col + 1,
                )
            }
        }
    }
}

/// One byte past the char at `pos`, or `(pos.line + 1, 0)` if `pos` sits on the implicit `\n`
/// at the end of its line. Falls back to a +1 approximation when the line isn't in the
/// pushed window (which makes the cursor off-screen anyway).
fn exclusive_end_of(state: &AppState, pos: LogicalPosition) -> LogicalPosition {
    let local_idx = (pos.line as i64) - (state.ed().window_first_logical_line as i64);
    let Some(render) = (if local_idx >= 0 {
        state.ed().lines.get(local_idx as usize)
    } else {
        None
    }) else {
        return LogicalPosition {
            line: pos.line,
            col: pos.col + 1,
        };
    };
    let last_vrow = match render.visual_rows.last() {
        Some(r) => r,
        None => {
            return LogicalPosition {
                line: pos.line,
                col: pos.col + 1,
            }
        }
    };
    let last_text = last_vrow.segments.first().map_or("", |s| s.text.as_str());
    let line_text_len = last_vrow.byte_offset + last_text.len() as u32;
    if pos.col >= line_text_len {
        // Cursor on the line's implicit newline → exclusive end is the next line's col 0.
        return LogicalPosition {
            line: pos.line + 1,
            col: 0,
        };
    }
    // Cursor on a real char — advance by that char's UTF-8 byte width.
    let row = render.visual_rows.iter().find(|r| {
        let row_len = r.segments.first().map_or(0, |s| s.text.len() as u32);
        pos.col >= r.byte_offset && pos.col < r.byte_offset + row_len
    });
    let row_text = row
        .and_then(|r| r.segments.first())
        .map_or("", |s| s.text.as_str());
    let row_local = pos.col.saturating_sub(row.map_or(0, |r| r.byte_offset)) as usize;
    let char_bytes = row_text[row_local..]
        .chars()
        .next()
        .map_or(1, |c| c.len_utf8() as u32);
    LogicalPosition {
        line: pos.line,
        col: pos.col + char_bytes,
    }
}

fn place_terminal_cursor(f: &mut Frame, state: &AppState, buffer_area: Rect, status_area: Rect) {
    // Settings overlay takes precedence over every other cursor target. We only place the caret
    // when a text field is focused — the name field (index 0) or the add-root input row (last
    // index); on a root row the cursor is hidden (no `set_cursor_position` call → ratatui hides
    // it for this frame).
    if let Some(settings) = state.project_settings.as_ref() {
        if settings.selected == 0 {
            place_settings_name_cursor(f, settings, buffer_area);
        } else if settings.selected == settings.roots.len() + 1 {
            place_settings_input_cursor(f, settings, buffer_area);
        }
        return;
    }
    let ed = state.ed();
    if matches!(ed.mode, EditorMode::Search) && state.save_prompt.is_none() && !state.picker.open {
        // Park the terminal cursor on the status row, just past `/` + the typed query up
        // to the input cursor (so Left/Right navigate within the query, not always at the
        // end).
        let typed_w = ed.search.query.width_to_cursor() as u16;
        let col = status_area
            .x
            .saturating_add((1 + typed_w).min(status_area.width.saturating_sub(1)));
        f.set_cursor_position((col, status_area.y));
        return;
    }
    if state.picker.open {
        // No caret while a delete confirmation owns the input row, or while the LSP detail
        // drill-down replaces it — there's nothing to type into.
        if state.picker.pending_delete.is_some() || state.picker.lsp_detail.is_some() {
            return;
        }
        // Place the cursor inside the picker overlay's input row, at the current insertion
        // point within the query (or at the start, on the placeholder, when empty). For the
        // Explorer picker we offset by the dir-context prefix width — the prefix sits before
        // the typed query and the cursor needs to land after it.
        // Same rect the overlay drew (collapsed boxes keep the same top edge, so only the
        // height guard differs from the full-size rect).
        // No caret while a chip is selected either — there's no insertion point; the inverted
        // chip is the focus indicator.
        if state.picker.chip_selected.is_some() {
            return;
        }
        let box_area = collapsed_picker_box_rect(
            buffer_area,
            picker_content_rows(&state.picker),
            state.picker.chip_editor.is_some(),
        );
        if box_area.width >= 4 && box_area.height >= 3 {
            // Inner = inside the borders; inner padding adds another column on each side.
            let text_x = box_area.x + 2;
            let text_y = box_area.y + 1;
            let text_w = box_area.width.saturating_sub(4);
            // The chip editor line sits one row below the input; its caret offset comes from
            // the same span builder the renderer uses.
            if state.picker.chip_editor.is_some() {
                let (_, cursor_off) = chip_editor_spans(state);
                let col = text_x.saturating_add(cursor_off.min(text_w.saturating_sub(1)));
                f.set_cursor_position((col, text_y + 1));
                return;
            }
            let (label_text, path_text) = explorer_input_prefix(state, text_w as usize);
            let prefix_w = (label_text.width() + path_text.width()) as u16;
            // Mirror the renderer's chip layout so the caret lands after the chip row.
            let chips_w =
                picker_chip_spans(state, chip_budget(text_w as usize, prefix_w as usize)).1 as u16;
            let typed_w = state.picker.query.width_to_cursor() as u16;
            let col = text_x
                .saturating_add(prefix_w)
                .saturating_add(chips_w)
                .saturating_add(typed_w.min(text_w.saturating_sub(1)));
            f.set_cursor_position((col, text_y));
        }
        return;
    }
    if let Some(confirm) = state.confirm_prompt.as_ref() {
        // Park at the end of " {message}? [y/N]" so the I-beam sits past the prompt.
        let line = format!(" {}? [y/N]", confirm.message);
        let max_col = status_area
            .x
            .saturating_add(status_area.width.saturating_sub(1));
        let col = status_area
            .x
            .saturating_add(line.width() as u16)
            .min(max_col);
        f.set_cursor_position((col, status_area.y));
        return;
    }
    if let Some(prompt) = state.save_prompt.as_ref() {
        const PROMPT: &str = " save as: ";
        let prompt_w = PROMPT.width() as u16;
        let dir_w = save_prompt_prefix_width(prompt, state) as u16;
        let typed_w = prompt.input.width_to_cursor() as u16;
        let max_col = status_area
            .x
            .saturating_add(status_area.width.saturating_sub(1));
        let col = status_area
            .x
            .saturating_add(prompt_w.saturating_add(dir_w).saturating_add(typed_w))
            .min(max_col);
        f.set_cursor_position((col, status_area.y));
        return;
    }
    let Some((visual_row, visual_col)) = cursor_visual_position(state, buffer_area.height as u32)
    else {
        return; // cursor off-screen
    };
    let row = buffer_area.y + visual_row;
    // `visual_col` is content-relative; shift past the gutter to the real screen column.
    let col = buffer_area
        .x
        .saturating_add(GUTTER_WIDTH)
        .saturating_add(visual_col.min(buffer_area.width.saturating_sub(1)));
    // Hide the caret when the (bottom-anchored) hover popup is painted over it — no
    // `set_cursor_position` call leaves the terminal cursor hidden for this frame.
    if let Some(layout) = hover_layout(state, buffer_area) {
        let b = layout.area;
        if row >= b.y && row < b.y + b.height && col >= b.x && col < b.x + b.width {
            return;
        }
    }
    f.set_cursor_position((col, row));
}

/// Map the cursor's logical (line, col) to (visual_row_offset_from_top_of_viewport, visual_col).
/// Returns `None` if the cursor is off-screen (above the top, below the bottom, off-screen left
/// after horizontal scroll, or its logical line hasn't been pushed into the window yet).
pub fn cursor_visual_position(state: &AppState, viewport_rows: u32) -> Option<(u16, u16)> {
    let top = state.ed().scroll_logical_line;
    let cursor = state.ed().cursor.position;
    if cursor.line < top {
        return None;
    }
    let scroll_col = if matches!(state.ed().wrap, WrapMode::None) {
        state.ed().scroll_col
    } else {
        0
    };

    // Visual rows of the top line hidden above the viewport. `visual_offset` is measured from the
    // top line's first row; the on-screen row is `visual_offset - skip`. Clamp identically to
    // `draw_buffer` so the two never disagree.
    let skip = {
        let local = (top as i64) - (state.ed().window_first_logical_line as i64);
        if local >= 0 && (local as usize) < state.ed().lines.len() {
            let r = &state.ed().lines[local as usize];
            let h = (r.virtual_rows_above.len() + r.visual_rows.len().max(1)) as u32;
            state.ed().scroll_skip_rows.min(h.saturating_sub(1))
        } else {
            0
        }
    };
    let bottom = viewport_rows + skip; // bail once we're past the last visible row

    let mut visual_offset: u32 = 0;
    for line_idx in top..=cursor.line {
        let local_idx = (line_idx as i64) - (state.ed().window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.ed().lines.len() as i64 {
            return None;
        }
        let render = &state.ed().lines[local_idx as usize];
        // Phantom diff rows render above this line's content, so they push the cursor down whether
        // or not this is the cursor's line.
        visual_offset += render.virtual_rows_above.len() as u32;
        if line_idx == cursor.line {
            let row_idx = find_row_idx_for_col(&render.visual_rows, cursor.col);
            visual_offset += row_idx as u32;
            if visual_offset < skip || visual_offset >= bottom {
                return None; // hidden above the top, or below the bottom
            }
            let visual_offset = visual_offset - skip;
            let row = &render.visual_rows[row_idx];
            // Walk chars in the row's text up to the cursor's byte offset, summing display
            // widths. The cursor lives in byte coordinates on the wire, but we render at display
            // columns — without this conversion a multi-byte char like `—` (3 bytes, 1 cell)
            // would push the cursor 2 columns past where the char visually ends.
            let row_text = row.segments.first().map(|s| s.text.as_str()).unwrap_or("");
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
            let marker = if row.byte_offset > 0 {
                CONTINUATION_MARKER_WIDTH
            } else {
                0
            };
            let logical_visual_col = marker + row.continuation_indent + display_col_in_text;
            if logical_visual_col < scroll_col {
                return None; // scrolled off the left
            }
            let visual_col = logical_visual_col - scroll_col;
            return Some((visual_offset as u16, visual_col as u16));
        }
        visual_offset += render.visual_rows.len() as u32;
        if visual_offset >= bottom {
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub fn screen_to_logical(
    state: &AppState,
    screen_row: u16,
    screen_col: u16,
) -> Option<LogicalPosition> {
    if (screen_row as u32) >= state.viewport_rows {
        return None;
    }
    // Strip the gutter: a click in the gutter column maps to the start of the line's content.
    let screen_col = screen_col.saturating_sub(GUTTER_WIDTH);
    // Screen row 0 is the top line's `scroll_skip_rows`-th visual row, so the click's global
    // visual-row offset (from the top line's first row) is `screen_row + skip`.
    let mut rows_remaining = screen_row as u32 + state.ed().scroll_skip_rows;
    let mut logical_line = state.ed().scroll_logical_line;
    loop {
        let local_idx = (logical_line as i64) - (state.ed().window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.ed().lines.len() as i64 {
            // Click is past the last line we have rendered — clamp to the end of the buffer.
            let last_line = state.ed().line_count.saturating_sub(1);
            return Some(LogicalPosition {
                line: last_line,
                col: u32::MAX,
            });
        }
        let render = &state.ed().lines[local_idx as usize];
        // Phantom diff rows render above the line's content. A click on one maps to the start of
        // the real line it sits above (they have no addressable position of their own).
        let virtual_rows = render.virtual_rows_above.len() as u32;
        if rows_remaining < virtual_rows {
            return Some(LogicalPosition {
                line: logical_line,
                col: 0,
            });
        }
        rows_remaining -= virtual_rows;
        let visual_rows_in_line = render.visual_rows.len() as u32;
        if rows_remaining < visual_rows_in_line {
            let vrow = &render.visual_rows[rows_remaining as usize];
            return Some(LogicalPosition {
                line: logical_line,
                col: byte_at_screen_col(state, vrow, screen_col),
            });
        }
        rows_remaining -= visual_rows_in_line;
        logical_line = logical_line.checked_add(1)?;
    }
}

/// Walk the visual row's text by display width to find the byte offset (within the logical line)
/// that lines up with `screen_col`. Clicks on the marker / continuation indent map to the start
/// of the row's text. Clicks past the end of the text map to the end of the text.
fn byte_at_screen_col(state: &AppState, vrow: &VisualRow, screen_col: u16) -> u32 {
    let scroll_col = if matches!(state.ed().wrap, WrapMode::None) {
        state.ed().scroll_col
    } else {
        0
    };
    let marker = if vrow.byte_offset > 0 {
        CONTINUATION_MARKER_WIDTH
    } else {
        0
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a grep result list: `groups` is a list of (file, hit_count). Each file contributes
    /// one header row plus `hit_count` selectable item rows.
    fn grep_items(groups: &[(&str, usize)]) -> Vec<PickerItem> {
        let mut items = Vec::new();
        for (fi, (path, n)) in groups.iter().enumerate() {
            for line in 0..*n {
                items.push(PickerItem::GrepHit {
                    path_index: fi as u32,
                    relative_path: (*path).to_string(),
                    line: line as u32,
                    col: 0,
                    preview: String::new(),
                    match_indices: Vec::new(),
                });
            }
        }
        items
    }

    /// Grep scroll math must bottom-align using the header-aware row count, not a flat one row
    /// per item. The earlier flat math under-scrolled by the number of header rows, leaving the
    /// selected row just below the visible box.
    #[test]
    fn grep_scroll_accounts_for_header_rows() {
        // Three files, three hits each → 9 items, 12 rendered rows (3 headers + 9 hits).
        let items = grep_items(&[("a.rs", 3), ("b.rs", 3), ("c.rs", 3)]);
        let kind = Some(aether_protocol::picker::PickerKind::Grep);
        let pane = 6usize; // 6 visual rows on screen

        // From the top, items 0..4 fit in 6 rows (header a + 3 hits = 4 rows; header b + 1 hit
        // = 6). Item 4 (second hit of b.rs) is the first that overflows, so it must scroll.
        let scroll = picker_scroll_for_selected(&items, 4, 0, pane, kind);
        assert!(scroll > 0, "selecting item 4 should scroll; got {scroll}");
        // After scrolling, item 4 must actually be within the rendered window.
        let count = grep_visible_item_count_from(&items, scroll, pane);
        assert!(
            (scroll..scroll + count).contains(&4),
            "selected item 4 outside window [{scroll}, {})",
            scroll + count
        );

        // A flat (non-grep) bottom-align would land at `4 + 1 - 6` saturating to 0 — which would
        // (wrongly) leave item 4 below the box. The header-aware path scrolls down instead.
        assert_eq!(
            (4 + 1usize).saturating_sub(pane),
            0,
            "flat math would not scroll here"
        );
    }

    /// Selecting an already-visible row leaves the scroll untouched; scrolling above the window
    /// pins the selection to the top.
    #[test]
    fn grep_scroll_is_stable_within_window_and_pins_upward() {
        let items = grep_items(&[("a.rs", 3), ("b.rs", 3)]);
        let kind = Some(aether_protocol::picker::PickerKind::Grep);
        let pane = 8usize;
        // Item 0 from offset 0 is visible → no change.
        assert_eq!(picker_scroll_for_selected(&items, 0, 0, pane, kind), 0);
        // Selecting an item above the current scroll pins it to the top.
        assert_eq!(picker_scroll_for_selected(&items, 2, 4, pane, kind), 2);
    }

    #[test]
    fn ordered_selection_keeps_point_visible_in_search() {
        let at = |line, col| LogicalPosition { line, col };
        let point = CursorState {
            position: at(1, 3),
            anchor: at(1, 3),
            ..Default::default()
        };
        // A single-char selection (point) shows in Normal and stays visible while the search input
        // has focus, but Insert mode is caret-only.
        assert_eq!(
            ordered_selection(&point, EditorMode::Normal),
            Some((at(1, 3), at(1, 3)))
        );
        assert_eq!(
            ordered_selection(&point, EditorMode::Search),
            Some((at(1, 3), at(1, 3)))
        );
        assert_eq!(ordered_selection(&point, EditorMode::Insert), None);

        // A multi-char range shows regardless of mode (incl. Search), and is returned start-first.
        let range = CursorState {
            position: at(1, 5),
            anchor: at(1, 1),
            ..Default::default()
        };
        assert_eq!(
            ordered_selection(&range, EditorMode::Search),
            Some((at(1, 1), at(1, 5)))
        );
    }

    /// Spot-check the rendered overlay: unpaired descriptions appear verbatim, Alt variants are
    /// folded inline, and forward/backward pairs are merged into one row with merged keys and
    /// descriptions.
    #[test]
    fn help_lines_render_expected_rows() {
        // Concatenate every tab (wide enough that nothing wraps); the expected rows are spread
        // across them — `Toggle soft wrap` on Normal/Insert, the grep pair on Normal, etc.
        let rendered: String = HelpTab::ALL
            .iter()
            .flat_map(|t| help_lines(*t, 100))
            .flat_map(|l| l.spans.into_iter())
            .map(|s| s.content.into_owned())
            .collect();
        // Unpaired bindings appear verbatim (key + description).
        for needle in [
            "Toggle soft wrap",
            "Clear the active search",
            "Show keyboard shortcuts",
            "Center cursor in window",
        ] {
            assert!(rendered.contains(needle), "missing: {needle:?}");
        }
        // Direction pairs are merged (keys and descriptions), with a spaced `/` separator.
        for needle in [
            "h / l",
            "Character left / right",
            "j / k",
            "Logical line down / up",
            "[ / ]",
            "Previous / Next navigation unit",
            "{ / }",
            "Select to start / end of unit",
            "< / >",
            "Previous / Next grep hit",
            "↑ / ↓",
            "Scroll up / down one line",
            "← / →",
            "PageUp / PageDown",
            "Scroll page up / down",
        ] {
            assert!(rendered.contains(needle), "expected merged row: {needle:?}");
        }
        // Alt variants fold onto the base line, and merge alongside a direction pair.
        assert!(
            rendered.contains("Alt-h / l"),
            "Alt variant of a pair should merge too"
        );
        assert!(rendered.contains("First non-blank / End of line"));
        // The un-merged "lef / right" bug (char-level merge splitting a shared letter) must not recur.
        assert!(
            !rendered.contains("lef / right"),
            "descriptions must merge word-wise"
        );
    }

    /// The mode-divergent Ctrl keys read with the right scope on each tab — selection-scoped on
    /// Normal, line-scoped on Insert — and the Selection group's bare `c`/`r` no longer mis-fold
    /// the unrelated `Ctrl-c`/`Ctrl-r` onto their rows.
    #[test]
    fn help_lines_describe_ctrl_keys_per_mode() {
        let render = |tab| -> Vec<String> {
            help_lines(tab, 120)
                .into_iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        let normal = render(HelpTab::Normal);
        let insert = render(HelpTab::Insert);
        // A line holding both the key and its description (spacing-agnostic — the gutter width
        // shifts as keys merge).
        let row = |ls: &[String], key: &str, desc: &str| {
            ls.iter().any(|l| l.contains(key) && l.contains(desc))
        };

        // Normal: selection-scoped wording; Insert: line-scoped wording.
        assert!(row(&normal, "Ctrl-c", "Change selection"));
        assert!(row(&insert, "Ctrl-d", "Delete line"));
        assert!(row(&insert, "Ctrl-c", "Change line"));
        assert!(row(&insert, "Ctrl-y", "Copy line"));

        // Normal collapses the two keys for "delete selection" (the Delete key and Ctrl-d) into one
        // aliased row, comma-separated; Insert keeps them apart (different commands there).
        assert!(row(&normal, "Delete, Ctrl-d", "Delete selection"));
        assert!(!insert.iter().any(|l| l.contains("Delete, Ctrl-d")));

        // The fold bug: `Ctrl-c` must not be glued onto the bare-`c` Collapse row.
        let collapse = normal
            .iter()
            .find(|l| l.contains("Collapse selection"))
            .expect("Collapse row present");
        assert!(
            !collapse.contains("Ctrl-c"),
            "Ctrl-c must not fold onto the bare `c` row: {collapse:?}"
        );
    }

    /// The comma joining aliased keys (`Delete, Ctrl-d`) renders in the dim separator colour, like
    /// the direction-pair `/`. The literal comma *key* (`Space ,`) stays key-coloured, and prose
    /// commas in descriptions are untouched (covered by `sep == main` for descriptions).
    #[test]
    fn alias_separator_comma_is_dimmed() {
        let comma_fg = |lines: &[Line<'static>], row: &str| -> Option<Color> {
            lines
                .iter()
                .find(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .contains(row)
                })
                .and_then(|l| {
                    l.spans
                        .iter()
                        .find(|s| s.content.as_ref() == ",")
                        .map(|s| s.style.fg)
                })
                .flatten()
        };
        let normal = help_lines(HelpTab::Normal, 120);
        let app = help_lines(HelpTab::Application, 120);
        // NORD3 = the dim separator colour; NORD9 = the key colour.
        assert_eq!(
            comma_fg(&normal, "Delete, Ctrl-d"),
            Some(NORD3),
            "alias separator comma should be dimmed"
        );
        assert_eq!(
            comma_fg(&app, "Space ,"),
            Some(NORD9),
            "the literal comma key must stay key-coloured"
        );
    }

    /// The modal backdrop mutes a cell's colour and emphasis to the base palette but keeps its glyph
    /// (so the content stays faintly visible behind a dialog).
    #[test]
    fn dim_backdrop_mutes_cells_keeping_glyphs() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(
            0,
            0,
            "code",
            Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
        );
        dim_backdrop(&mut buf, area);
        let cell = buf.cell((0, 0)).expect("cell present");
        assert_eq!(cell.symbol(), "c", "glyph is preserved");
        assert_eq!(cell.fg, NORD3, "foreground muted to grey");
        assert_eq!(cell.bg, NORD0, "background flattened to base");
        assert!(
            !cell.modifier.contains(Modifier::BOLD),
            "emphasis is dropped"
        );
    }

    /// The tab bar marks the active tab with accent colour + underline (no divider glyphs); inactive
    /// tabs are dim and unadorned.
    #[test]
    fn tab_bar_underlines_active() {
        let line = tab_bar_line(HelpTab::Insert);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('│'), "no divider glyphs");
        let span = |label: &str| {
            line.spans
                .iter()
                .find(|s| s.content.as_ref() == label)
                .unwrap_or_else(|| panic!("tab {label:?} present"))
        };
        let active = span("Insert");
        assert_eq!(active.style.fg, Some(NORD8), "active tab is accented");
        assert!(
            active.style.add_modifier.contains(Modifier::UNDERLINED),
            "active tab is underlined"
        );
        let other = span("Normal");
        assert_eq!(other.style.fg, Some(NORD3), "inactive tab is dim");
        assert!(!other.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn merge_keys_factors_single_chars_else_shows_both() {
        assert_eq!(merge_keys("Alt-h", "Alt-l"), "Alt-h / l");
        assert_eq!(merge_keys("↑", "↓"), "↑ / ↓");
        assert_eq!(merge_keys("[", "]"), "[ / ]");
        assert_eq!(merge_keys("x", "x"), "x");
        // Multi-char differing middles aren't factored (avoids "PageUp / Down").
        assert_eq!(merge_keys("PageUp", "PageDown"), "PageUp / PageDown");
    }

    #[test]
    fn merge_descs_is_word_wise() {
        // The shared trailing letter of "left"/"right" stays attached (word-level merge).
        assert_eq!(merge_descs("Char left", "Char right"), "Char left / right");
        assert_eq!(
            merge_descs("Scroll up one line", "Scroll down one line"),
            "Scroll up / down one line"
        );
        assert_eq!(
            merge_descs("First non-blank of line", "End of line"),
            "First non-blank / End of line"
        );
    }

    #[test]
    fn wrap_words_breaks_on_spaces() {
        assert_eq!(wrap_words("a b c", 3), vec!["a b", "c"]);
        assert_eq!(wrap_words("hello world", 100), vec!["hello world"]);
        assert_eq!(wrap_words("", 10), vec![String::new()]);
        // A single over-long word overflows rather than being hard-split.
        assert_eq!(
            wrap_words("supercalifragilistic", 5),
            vec!["supercalifragilistic"]
        );
    }

    #[test]
    fn hover_lines_strips_fences_wraps_and_trims() {
        let text = "```rust\nfn foo()\n```\n\nDocs paragraph here";
        assert_eq!(
            hover_lines(text, 80),
            vec![
                "fn foo()".to_string(),
                String::new(),
                "Docs paragraph here".to_string()
            ]
        );
        // Leading/trailing blank lines are trimmed; long lines wrap.
        assert_eq!(hover_lines("\n\nhi\n\n", 80), vec!["hi".to_string()]);
        assert!(hover_lines("aaaa bbbb cccc", 9).len() >= 2);
    }

    #[test]
    fn hover_display_lines_tags_blocks_with_severity() {
        use crate::app::HoverBlock;
        let blocks = vec![
            HoverBlock {
                text: "Error: bad thing".into(),
                severity: Some(DiagnosticSeverity::Error),
            },
            HoverBlock {
                text: "Hint: maybe".into(),
                severity: Some(DiagnosticSeverity::Hint),
            },
        ];
        let lines = hover_display_lines(&blocks, 80);
        // Diagnostic blocks are prefixed with the severity icon on their first line.
        assert_eq!(
            lines[0],
            (
                "⊗ Error: bad thing".to_string(),
                Some(DiagnosticSeverity::Error)
            )
        );
        assert_eq!(
            lines[1],
            (String::new(), None),
            "blank separator between blocks"
        );
        assert_eq!(
            lines[2],
            ("○ Hint: maybe".to_string(), Some(DiagnosticSeverity::Hint))
        );
        // A plain (hover) block carries no severity → default color.
        let plain = vec![HoverBlock {
            text: "fn x()".into(),
            severity: None,
        }];
        assert_eq!(
            hover_display_lines(&plain, 80)[0],
            ("fn x()".to_string(), None)
        );
    }

    /// Concatenate a styled line's span text for assertions.
    #[cfg(test)]
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_styled_breaks_on_spaces_and_preserves_style() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let segs = vec![("hello world foo".to_string(), bold)];
        let lines = wrap_styled(&segs, 11);
        // "hello world" fits in 11; "foo" wraps.
        assert_eq!(lines.len(), 2);
        let l0: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        let l1: String = lines[1].iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(l0, "hello world");
        assert_eq!(l1, "foo");
        assert!(lines[0].iter().all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn wrap_styled_hard_breaks_overlong_word() {
        let segs = vec![("abcdefghij".to_string(), Style::default())];
        let lines = wrap_styled(&segs, 4);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(joined, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn md_hover_lines_renders_heading_code_and_list() {
        let blocks = aether_client::markdown::parse(
            "# Title\n\nSome `inline` text.\n\n- one\n- two\n\n```\ncode\n```\n",
        );
        let lines = md_hover_lines(&blocks, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // Heading present and bold + brightest fg.
        let heading = &lines[0];
        assert_eq!(line_text(heading), "Title");
        assert!(heading.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(heading.spans[0].style.fg, Some(NORD6));
        // List bullets rendered.
        assert!(texts.iter().any(|t| t.starts_with("• one")));
        assert!(texts.iter().any(|t| t.starts_with("• two")));
        // Inline code span carries the code background.
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.as_ref() == "inline" && s.style.bg == Some(MD_CODE_BG)));
        // Fenced code line gets the code background and is width-padded.
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.bg == Some(MD_CODE_BG))
                && line_text(l).starts_with("code")));
    }

    #[test]
    fn md_list_continuation_lines_hang_indent() {
        let blocks = aether_client::markdown::parse("- alpha beta gamma delta\n");
        // Narrow width forces the item to wrap onto a continuation line.
        let lines = md_hover_lines(&blocks, 12);
        assert!(lines.len() >= 2);
        assert!(line_text(&lines[0]).starts_with("• "));
        // Continuation indents under the text (two spaces, matching "• ").
        assert!(line_text(&lines[1]).starts_with("  "));
        assert!(!line_text(&lines[1]).starts_with("• "));
    }

    #[test]
    fn hover_border_color_matches_worst_severity() {
        use crate::app::{HoverBlock, HoverBody};
        let blk = |severity| HoverBlock {
            text: "m".into(),
            severity,
        };
        // Plain (severity-less) diagnostic block → frost blue.
        assert_eq!(hover_border_color(&HoverBody::Blocks(vec![blk(None)])), NORD8);
        // Markdown hover → frost blue.
        assert_eq!(hover_border_color(&HoverBody::Markdown(vec![])), NORD8);
        // Worst severity wins.
        assert_eq!(
            hover_border_color(&HoverBody::Blocks(vec![
                blk(Some(DiagnosticSeverity::Hint)),
                blk(Some(DiagnosticSeverity::Error))
            ])),
            diag_color(DiagnosticSeverity::Error)
        );
        assert_eq!(
            hover_border_color(&HoverBody::Blocks(vec![blk(Some(
                DiagnosticSeverity::Warning
            ))])),
            diag_color(DiagnosticSeverity::Warning)
        );
    }

    #[test]
    fn padded_spans_pads_to_display_width() {
        let st = Style::default();
        let total: usize = padded_spans("ab", 5, st, st)
            .iter()
            .map(|s| s.content.width())
            .sum();
        assert_eq!(total, 5, "short text is padded to the column width");
        let total: usize = padded_spans("abcde", 3, st, st)
            .iter()
            .map(|s| s.content.width())
            .sum();
        assert_eq!(total, 5, "already-wider text is not truncated");
    }

    #[test]
    fn narrow_help_wraps_to_more_lines() {
        // Squeezing the width forces descriptions to wrap / Alt variants to stack, so the overlay
        // grows taller. (The scroll machinery then makes the extra lines reachable.)
        assert!(help_lines(HelpTab::Normal, 100).len() > help_lines(HelpTab::Normal, 200).len());
    }

    // ---- truncate_to_width ----

    #[test]
    fn truncate_no_op_when_within_budget() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("hello", 100), "hello");
    }

    #[test]
    fn truncate_appends_ellipsis_when_over_budget() {
        // "hello world" is 11 cells; budget of 8 → 7 chars + `…`.
        assert_eq!(truncate_to_width("hello world", 8), "hello w…");
    }

    #[test]
    fn truncate_empty_when_max_is_zero() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_to_bare_ellipsis_when_budget_is_one() {
        // No room for even a single content char alongside the ellipsis.
        assert_eq!(truncate_to_width("hello", 1), "…");
    }

    #[test]
    fn truncate_respects_double_wide_chars() {
        // "あ" is 2 cells. "あabc" is 5 cells. With max 4, we'd ideally fit "あa" + `…` (4
        // cells). The greedy fill stops once adding the next char would overshoot.
        let s = "あabc";
        let out = truncate_to_width(s, 4);
        assert_eq!(out.width(), 4);
        assert!(out.ends_with('…'));
    }

    // ---- file_item_spans ----

    #[test]
    fn file_item_root_label_follows_path_dimmed() {
        let labels = vec!["alpha".to_string(), "beta".to_string()];
        let spans = file_item_spans(1, "src/main.rs", &[], None, &labels, false, 40);
        let text = spans_text(&spans);
        assert!(text.starts_with("  src/main.rs  beta")); // bullet cell, path, then the label
        let label = spans.last().expect("label span");
        assert_eq!(label.content.as_ref(), "  beta");
        assert_eq!(label.style.fg, Some(NORD3));
    }

    #[test]
    fn file_item_single_root_has_no_label() {
        let spans = file_item_spans(0, "src/main.rs", &[], None, &[], false, 40);
        assert_eq!(spans_text(&spans), "  src/main.rs");
    }

    // ---- lsp_server_item_spans ----

    #[test]
    fn lsp_row_status_dot_and_bulleted_tail() {
        let spans = lsp_server_item_spans(
            LspServerRow {
                name: "rust-analyzer",
                language: "rust",
                root_label: "backend",
                status: &LspStatus::Ready,
                progress: &[],
            },
            &[],
            false,
            60,
        );
        let text = spans_text(&spans);
        assert!(text.starts_with("• rust-analyzer"));
        assert!(text.ends_with("rust · backend"));
        assert_eq!(spans[0].style.fg, Some(NORD14)); // ready → green dot
                                                     // At the project root the tail is just the language — no separator.
        let single = lsp_server_item_spans(
            LspServerRow {
                name: "rust-analyzer",
                language: "rust",
                root_label: "",
                status: &LspStatus::Stopped,
                progress: &[],
            },
            &[],
            false,
            60,
        );
        assert!(spans_text(&single).ends_with("  rust"));
        assert_eq!(single[0].style.fg, Some(NORD3)); // stopped → dim dot
    }

    // ---- collapsed picker box ----

    #[test]
    fn collapsed_picker_box_shrinks_to_content_keeping_top() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let full = picker_box_rect(area);
        let collapsed = collapsed_picker_box_rect(area, 3, false);
        assert_eq!(collapsed.height, 7); // 3 content rows + borders + input + separator
        assert_eq!(
            (collapsed.x, collapsed.y, collapsed.width),
            (full.x, full.y, full.width)
        );
    }

    #[test]
    fn truncate_path_elides_middle_segments() {
        let p = "crates/aether-server/src/handlers.rs";
        // Fits → unchanged.
        assert_eq!(
            truncate_path_with_indices(p, &[], 40).0,
            "crates/aether-server/src/handlers.rs"
        );
        // Over budget: whole middle segments collapse to one `…`, filename always survives,
        // and the candidate keeping the most segments wins.
        assert_eq!(
            truncate_path_with_indices(p, &[], 30).0,
            "crates/…/src/handlers.rs"
        );
        // Tighter: the tail is preferred over leading dirs.
        assert_eq!(
            truncate_path_with_indices(p, &[], 20).0,
            "…/src/handlers.rs"
        );
        assert_eq!(truncate_path_with_indices(p, &[], 16).0, "…/handlers.rs");
        // Tighter than `…/{filename}`: char-level floor keeps the filename's tail.
        assert_eq!(truncate_path_with_indices(p, &[], 8).0, "…lers.rs");
        // Non-paths skip straight to the floor.
        assert_eq!(
            truncate_path_with_indices("a long project name", &[], 8).0,
            "…ct name"
        );
    }

    #[test]
    fn truncate_path_prefers_tail_on_ties() {
        // Both (lead 1, tail 2) and (lead 0, tail 3) keep three segments; the tail-heavy
        // candidate wins when it fits.
        let p = "aa/bb/cc/dd/ee.rs";
        assert_eq!(truncate_path_with_indices(p, &[], 15).0, "…/cc/dd/ee.rs");
    }

    #[test]
    fn truncate_path_remaps_match_indices() {
        let p = "crates/aether-server/src/handlers.rs";
        // Matches: "cra" (0..3), "ser" inside aether-server (14..17), "han" (25..28).
        let indices: Vec<u32> = vec![0, 1, 2, 14, 15, 16, 25, 26, 27];
        let (display, mapped) = truncate_path_with_indices(p, &indices, 30);
        assert_eq!(display, "crates/…/src/handlers.rs");
        // Kept-lead indices map identity; elided-span indices drop; tail indices shift onto
        // the display's tail.
        let chars: Vec<char> = display.chars().collect();
        let shown: String = mapped.iter().map(|&i| chars[i as usize]).collect();
        assert_eq!(shown, "crahan");
        assert_eq!(mapped, vec![0, 1, 2, 13, 14, 15]);
    }

    #[test]
    fn picker_box_width_caps_on_wide_terminals() {
        // Comfortable terminal: percentage scaling, under the cap.
        let medium = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        assert!(picker_box_rect(medium).width < PICKER_WIDTH_CAP);
        // Ultrawide: 80% would be 240 cols — the cap wins, and the box stays centred.
        let wide = Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 60,
        };
        let r = picker_box_rect(wide);
        assert_eq!(r.width, PICKER_WIDTH_CAP);
        assert_eq!(r.x, (300 - PICKER_WIDTH_CAP) / 2);
    }

    #[test]
    fn collapsed_picker_box_caps_at_full_size() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        assert_eq!(
            collapsed_picker_box_rect(area, 10_000, false),
            picker_box_rect(area)
        );
    }

    #[test]
    fn collapsed_picker_box_drops_separator_row_when_empty() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        assert_eq!(collapsed_picker_box_rect(area, 0, false).height, 3); // borders + input only
    }

    #[test]
    fn collapsed_picker_box_grows_for_open_chip_editor() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        // The chip editor line adds one chrome row below the input.
        assert_eq!(collapsed_picker_box_rect(area, 3, true).height, 8);
    }

    #[test]
    fn picker_content_rows_counts_kind_specifics() {
        use aether_protocol::picker::PickerKind;
        let mut p = crate::picker::PickerState {
            kind: Some(PickerKind::Files),
            total_matches: 5,
            ..Default::default()
        };
        assert_eq!(picker_content_rows(&p), 5);
        // The synthetic "Create …" row is client-side, on top of total_matches.
        p.synthetic_create_idx = Some(5);
        assert_eq!(picker_content_rows(&p), 6);
        // Grep counts the server-reported display rows (per-file headers included).
        p.synthetic_create_idx = None;
        p.kind = Some(PickerKind::Grep);
        p.total_display_rows = Some(8);
        assert_eq!(picker_content_rows(&p), 8);
        // References shows a one-row message while empty.
        p.kind = Some(PickerKind::References);
        p.total_matches = 0;
        assert_eq!(picker_content_rows(&p), 1);
    }

    // ---- grep_hit_spans ----

    #[test]
    fn grep_hit_line_number_right_aligned() {
        let spans = grep_hit_spans(41, "let x = 1;", &[], false, 30);
        let text = spans_text(&spans);
        assert!(text.starts_with("let x = 1;"));
        assert!(text.ends_with("42"));
        assert_eq!(spans_total_width(&spans), 30);
        // The number is dim; the padding before it carries at least the 2-col gap.
        assert!(text.contains("let x = 1;  "));
        let num = spans.last().expect("line-number span");
        assert_eq!(num.style.fg, Some(NORD3));
    }

    #[test]
    fn toast_accent_color_matches_kind() {
        use crate::app::StatusKind;
        // Matches the web/native toast border colours.
        assert_eq!(toast_accent_color(StatusKind::Info), NORD8);
        assert_eq!(toast_accent_color(StatusKind::Success), NORD14);
        assert_eq!(toast_accent_color(StatusKind::Warning), NORD13);
        assert_eq!(toast_accent_color(StatusKind::Error), NORD11);
    }

    #[test]
    fn picker_dim_spans_brighten_on_highlighted_row() {
        // NORD3 is illegible on the NORD2 selection background — highlighted rows lift their
        // dim spans (here: the grep line number, the file row's root label) to NORD4.
        let num = grep_hit_spans(41, "let x = 1;", &[], true, 30);
        assert_eq!(num.last().unwrap().style.fg, Some(NORD4));
        let labels = vec!["alpha".to_string(), "beta".to_string()];
        let file = file_item_spans(1, "src/main.rs", &[], None, &labels, true, 40);
        assert_eq!(file.last().unwrap().style.fg, Some(NORD4));
    }

    #[test]
    fn grep_hit_truncates_long_preview_keeping_line_number() {
        let preview = "a very long line of code that cannot possibly fit in the row";
        let spans = grep_hit_spans(99, preview, &[], false, 24);
        let text = spans_text(&spans);
        assert!(text.contains('…'));
        assert!(text.ends_with("100"));
        assert_eq!(spans_total_width(&spans), 24);
    }

    #[test]
    fn grep_hit_strips_leading_whitespace_and_shifts_matches() {
        // Match on "hel" at chars 4..7 of the untrimmed preview; after stripping the 4-char
        // indent the highlight must land on the same letters.
        let spans = grep_hit_spans(0, "    helper();", &[4, 5, 6], false, 40);
        let text = spans_text(&spans);
        assert!(text.starts_with("helper();"));
        let hl: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(NORD13))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, "hel");
    }

    #[test]
    fn grep_hit_drops_matches_inside_stripped_whitespace() {
        let spans = grep_hit_spans(0, "    x", &[1, 2], false, 40);
        assert!(spans.iter().all(|s| s.style.fg != Some(NORD13)));
        assert!(spans_text(&spans).starts_with("x "));
    }

    // ---- build_editor_status_spans ----

    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    fn spans_total_width(spans: &[Span<'_>]) -> usize {
        spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn editor_status_spans_no_status_pads_to_right_edge() {
        let status = crate::app::StatusMessage::default();
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
            },
            None,
            Vec::new(),
            &status,
            vec![Span::raw("12:5")],
            30,
        );
        let text = spans_text(&spans);
        assert!(text.starts_with("[proj] file.rs"));
        assert!(text.ends_with("12:5"));
        assert_eq!(spans_total_width(&spans), 30);
    }

    /// A transient buffer italicises the project/file segment (no explicit marker text); a
    /// permanent one doesn't.
    #[test]
    fn editor_status_spans_italicise_transient_label() {
        let status = crate::app::StatusMessage::default();
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: true,
            },
            None,
            Vec::new(),
            &status,
            vec![],
            30,
        );
        let label = spans
            .iter()
            .find(|s| s.content.contains("file.rs"))
            .expect("label span present");
        assert!(label.style.add_modifier.contains(Modifier::ITALIC));
        assert!(!spans_text(&spans).contains("transient"), "no marker text");

        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
            },
            None,
            Vec::new(),
            &status,
            vec![],
            30,
        );
        let label = spans
            .iter()
            .find(|s| s.content.contains("file.rs"))
            .unwrap();
        assert!(!label.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn editor_status_spans_renders_buffer_status_dot() {
        let status = crate::app::StatusMessage::default();
        let dot = Span::styled(
            BUFFER_STATUS_DOT.to_string(),
            Style::default().fg(buffer_status_color(BufferStatusKind::Unsaved)),
        );
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
            },
            Some(dot),
            Vec::new(),
            &status,
            vec![],
            30,
        );
        // The dot leads the row, before the project name, in the unsaved (frost-blue) colour.
        let text = spans_text(&spans);
        assert!(text.starts_with(&format!("{BUFFER_STATUS_DOT} [proj] file.rs")));
        let dot_span = spans
            .iter()
            .find(|s| s.content.contains(BUFFER_STATUS_DOT))
            .expect("status dot span present");
        assert_eq!(dot_span.style.fg, Some(NORD9));
        assert_eq!(spans_total_width(&spans), 30);
    }

    #[test]
    fn editor_status_spans_renders_status_with_color() {
        let status = crate::app::StatusMessage::success("saved (rev 1)");
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
            },
            None,
            Vec::new(),
            &status,
            vec![Span::raw("12:5")],
            60,
        );
        // Status text should appear, sandwiched between the left bit and the padding/right.
        let text = spans_text(&spans);
        assert!(text.contains("[proj] file.rs"));
        assert!(text.contains("saved (rev 1)"));
        assert!(text.ends_with("12:5"));
        // The span containing the status text must carry the success colour.
        let status_span = spans
            .iter()
            .find(|s| s.content.contains("saved (rev 1)"))
            .expect("status span present");
        assert_eq!(status_span.style.fg, Some(NORD8));
    }

    #[test]
    fn editor_status_spans_drops_status_when_left_pre_alone_overflows() {
        // total=12, right=4, gap=1 → left_max=7. "[proj] file.rs" (14) > 7: the file label gets
        // the budget (and fits exactly), the project prefix — which can't fit whole alongside it
        // — is dropped, and the status is dropped entirely.
        let status = crate::app::StatusMessage::error("save failed: disk full");
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
            },
            None,
            Vec::new(),
            &status,
            vec![Span::raw("12:5")],
            12,
        );
        let text = spans_text(&spans);
        // No part of the status text should make it into the rendered line.
        assert!(
            !text.contains("save failed"),
            "status should have been dropped: {text:?}"
        );
        assert!(
            text.starts_with("file.rs"),
            "label survives, prefix dropped: {text:?}"
        );
        assert!(text.ends_with("12:5"));
        assert_eq!(spans_total_width(&spans), 12);
    }

    /// A long path elides (keeping the filename end) when the label alone overflows the row.
    #[test]
    fn editor_status_spans_elides_long_label() {
        let status = crate::app::StatusMessage::default();
        let spans = build_editor_status_spans(
            StatusLabel {
                project_prefix: "[proj] ",
                file_label: "src/deeply/nested/module/file.rs",
                transient: false,
            },
            None,
            Vec::new(),
            &status,
            vec![Span::raw("12:5")],
            25,
        );
        let text = spans_text(&spans);
        assert!(text.contains('…'));
        assert!(
            text.contains("file.rs"),
            "filename end survives elision: {text:?}"
        );
        assert_eq!(spans_total_width(&spans), 25);
    }

    fn dspan(start: u32, end: u32, severity: DiagnosticSeverity) -> DiagnosticSpan {
        DiagnosticSpan {
            start,
            end,
            severity,
            message: "m".into(),
        }
    }

    #[test]
    fn diagnostics_on_visual_row_clips_to_row_and_widens_zero_width() {
        let diags = vec![
            dspan(4, 9, DiagnosticSeverity::Error),
            dspan(20, 20, DiagnosticSeverity::Warning), // zero-width point
        ];
        // Row [0,12): the error clips to (4,9); the point at 20 is off-row.
        assert_eq!(
            diagnostics_on_visual_row(0, 12, &diags),
            vec![(4, 9, DiagnosticSeverity::Error)]
        );
        // Row [16,30): the point widens to one cell at row-relative 4.
        assert_eq!(
            diagnostics_on_visual_row(16, 14, &diags),
            vec![(4, 5, DiagnosticSeverity::Warning)]
        );
        // Empty rows carry nothing.
        assert!(diagnostics_on_visual_row(0, 0, &diags).is_empty());
    }

    /// Per-char underline state from `build_spans`, indexed by column (ASCII input → col == byte).
    fn underline_cols(spans: &[Span<'static>]) -> Vec<(bool, Option<Color>)> {
        let mut out = Vec::new();
        for s in spans {
            for _ in s.content.chars() {
                out.push((
                    s.style.add_modifier.contains(Modifier::UNDERLINED),
                    s.style.underline_color,
                ));
            }
        }
        out
    }

    #[test]
    fn build_spans_underlines_diagnostic_in_severity_color() {
        let diags = [(2u32, 4u32, DiagnosticSeverity::Warning)];
        let cells = underline_cols(&build_spans("abcdef", &[], None, &[], &[], &diags, 80));
        for (col, (underlined, color)) in cells.into_iter().enumerate() {
            if col == 2 || col == 3 {
                assert!(underlined, "cell {col} underlined");
                assert_eq!(color, Some(NORD13), "cell {col} warning-yellow");
            } else {
                assert!(!underlined, "cell {col} not underlined");
            }
        }
    }

    #[test]
    fn build_spans_underline_uses_worst_severity_on_overlap() {
        // Hint over [0,3) with an error over [1,2): the error color wins on the overlapping cell.
        let diags = [
            (0u32, 3u32, DiagnosticSeverity::Hint),
            (1u32, 2u32, DiagnosticSeverity::Error),
        ];
        let cells = underline_cols(&build_spans("xyz", &[], None, &[], &[], &diags, 80));
        assert_eq!(cells[1].1, Some(NORD11), "overlap shows error red");
        assert_eq!(cells[0].1, Some(NORD4), "non-overlap keeps hint colour");
    }

    #[test]
    fn lsp_status_color_maps_states() {
        assert_eq!(lsp_status_color(&LspStatus::Ready), NORD14);
        assert_eq!(lsp_status_color(&LspStatus::Initializing), NORD13);
        assert_eq!(lsp_status_color(&LspStatus::Restarting), NORD13);
        assert_eq!(
            lsp_status_color(&LspStatus::Crashed {
                code: None,
                message: String::new()
            }),
            NORD11
        );
        assert_eq!(lsp_status_color(&LspStatus::Stopped), NORD3);
    }

    #[test]
    fn lsp_progress_hint_summarizes_active_work() {
        let mk = |title: &str, pct: Option<u32>| LspProgress {
            title: title.into(),
            message: None,
            percentage: pct,
        };
        assert_eq!(lsp_progress_hint(&[]), "");
        assert_eq!(lsp_progress_hint(&[mk("Indexing", None)]), "  Indexing");
        assert_eq!(
            lsp_progress_hint(&[mk("cargo check", Some(28))]),
            "  cargo check 28%"
        );
        // Several concurrent operations → first (with %) plus a "+N" overflow marker.
        assert_eq!(
            lsp_progress_hint(&[mk("cargo check", Some(28)), mk("Indexing", None)]),
            "  cargo check 28% +1"
        );
    }

    #[test]
    fn cursor_line_bg_uses_diff_variant_on_changed_lines() {
        use aether_protocol::viewport::DiffStage::{Staged, Unstaged};
        // An added/modified cursor line gets the green/olive variant, not the plain cursorline —
        // and crucially not the diff tint itself, so it reads as "cursor here AND changed".
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Added), Unstaged),
            CURSOR_LINE_ADDED_BG
        );
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Modified), Unstaged),
            CURSOR_LINE_MODIFIED_BG
        );
        assert_ne!(
            cursor_line_bg(Some(DiffMarker::Added), Unstaged),
            GIT_ADDED_BG
        );
        // A staged line keeps its dimmer identity under the cursor instead of flaring back up to
        // the unstaged brightness.
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Added), Staged),
            CURSOR_LINE_STAGED_ADDED_BG
        );
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Modified), Staged),
            CURSOR_LINE_STAGED_MODIFIED_BG
        );
        // Deleted (no real-line tint) and unchanged lines fall back to the plain cursorline.
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Deleted), Unstaged),
            CURSOR_LINE_BG
        );
        assert_eq!(cursor_line_bg(None, Unstaged), CURSOR_LINE_BG);
    }
}
