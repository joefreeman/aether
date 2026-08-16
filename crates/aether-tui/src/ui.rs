//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::{
    jumplist_counter_label, search_counter_label, search_match_count_label, AppState,
    BufferStatusKind, EditorMode, SearchState, BUFFER_STATUS_DOT,
};
use aether_client::markdown::{Block as MdBlock, Inline as MdInline};
use aether_client::session::{AppSettingControl, ConnState};
use aether_client::theme::{Rgb, Theme};
use aether_protocol::cursor::CursorState;
use aether_protocol::git::GitStatus;
use aether_protocol::lsp::{LspProgress, LspStatus};
use aether_protocol::picker::{BufferDirtyState, GroupHeader, GroupSpan, PickerItem, PickerKind};
use aether_protocol::search::SearchMatchRange;
use aether_protocol::settings::ThemeMode;
use aether_protocol::sneak::SneakTarget;
use aether_protocol::viewport::{
    DiagnosticSeverity, DiagnosticSpan, DiffMarker, DiffStage, EmphasisRange, Highlight,
    VisualRow, WrapMode,
};
use aether_protocol::LogicalPosition;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
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

// ---- Theme -------------------------------------------------------------------------------------
// Colours come from the core's role tables (`aether_client::theme`) — the single source of truth
// shared by every shell. `Theme::DARK` maps each role to exactly the Nord constant this file used
// to hardcode (the role comments in theme.rs name them), so dark renders pixel-identical to the
// pre-theme TUI; `Theme::LIGHT` is the light counterpart. Call sites reference roles through
// [`th`] + [`c`]. The active mode is thread-local: the draw path is single-threaded, but `cargo
// test` runs many tests in one process, and a process-global would let one test's Light leak into
// another's dark assertions.

thread_local! {
    /// The mode this thread paints with. The shell sets it every frame from `session.theme`
    /// before `draw`; tests exercising light mode set it (and restore Dark) themselves.
    static THEME_MODE: std::cell::Cell<ThemeMode> = const { std::cell::Cell::new(ThemeMode::Dark) };
}

/// Install the theme mode for this thread's subsequent painting.
pub fn set_theme_mode(mode: ThemeMode) {
    THEME_MODE.with(|m| m.set(mode));
}

/// The active role table, per the thread-local mode set by [`set_theme_mode`].
fn th() -> &'static Theme {
    Theme::of(THEME_MODE.with(|m| m.get()))
}

/// Core [`Rgb`] → ratatui [`Color`], at the draw boundary.
fn c(rgb: Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

/// Whether the status row is drawn. It takes the terminal's bottom row and the content pane —
/// where every overlay is centred — gets the rest. Shared by [`draw`] and [`content_area`] so the
/// renderer and the mouse hit-tests can't disagree about where the content pane ends.
fn shows_status_row(state: &AppState) -> bool {
    state.has_editor() || state.conn == ConnState::Connecting
}

/// The content pane in a `cols × rows` terminal — [`draw`]'s first chunk, the rect the picker and
/// the other overlays are centred in. Reconstructed for hit-testing a mouse press against what was
/// painted (the shell tracks the terminal size, not the layout).
fn content_area(state: &AppState, cols: u16, rows: u16) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows.saturating_sub(shows_status_row(state) as u16),
    }
}

pub fn draw(f: &mut Frame, state: &AppState) {
    // The status row carries save-as / new-file prompts and the dirty + cursor indicator for an
    // active editor. The add-root prompt lives *inside* the settings overlay, not here. Transient
    // feedback no longer lives here — it floats as a toast (see `draw_toast_overlay`) — so the row
    // is shown only for an active editor, leaving the no-workspace view its full vertical space.
    // The status row shows for an active editor, and also at boot while `Connecting` (no editor
    // yet) so the connection indicator has its familiar home — the chrome is up from the start.
    let show_status = shows_status_row(state);
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
        // The markdown reading view replaces the editor window wholesale while active
        // (docs/markdown-view.md); all overlays/toasts/status draw over either the same way.
        if state.read.is_some() {
            draw_read_view(f, state, chunks[0]);
        } else {
            draw_buffer(f, state, chunks[0]);
        }
    } else {
        draw_no_workspace_view(f, state, chunks[0]);
    }
    // Hover popup (Space k): floats over the buffer, below any modal (a keypress that opens a modal
    // first dismisses hover, so they never coexist).
    if state.has_editor() && state.hover.is_some() {
        draw_hover_overlay(f, state, chunks[0]);
    }
    // A centered modal dims the content behind it so it stands out. Done once here, before any
    // overlay paints: each overlay `Clear`s and repaints its own box opaquely, so only the area
    // *behind* the dialog ends up dimmed.
    let modal_open = state.picker.open
        || state.workspace_settings.is_some()
        || state.app_settings.is_some()
        || state.app_info.is_some()
        || state.picker.lsp_detail.is_some();
    // Status-bar prompts dim the editor too, so attention moves to the prompt: the save-as path
    // input and the y/N confirm prompts. Search is deliberately excluded — it live-highlights
    // matches in the buffer, so the editor must stay legible (and it sets neither flag below).
    let status_prompt_open = state.save_prompt.is_some()
        || state.confirm_prompt.is_some()
        || state.open_path_prompt.is_some();
    if modal_open || status_prompt_open {
        dim_backdrop(f.buffer_mut(), chunks[0]);
    }
    // The unified picker overlay sits on top of either screen — same renderer for Files /
    // Buffers / Grep / Explorer / Workspaces.
    if state.picker.open {
        draw_picker_overlay(f, state, chunks[0]);
    }
    // Workspace settings overlay (Space P): centered modal listing the active workspace's roots.
    if state.workspace_settings.is_some() {
        draw_workspace_settings_overlay(f, state, chunks[0]);
    }
    // Application settings overlay (Space .): centered modal listing global settings.
    if state.app_settings.is_some() {
        draw_app_settings_overlay(f, state, chunks[0]);
    }
    // LSP-server detail (Space l → Enter): a top-level overlay — the picker is closed when the
    // core drills into `Prompt::LspInfo`, so it's not part of the picker box.
    if let Some(detail) = state.picker.lsp_detail.as_ref() {
        draw_lsp_detail_overlay(f, detail, chunks[0]);
    }
    // Application info (Space ?): the same box as the LSP detail, one section per group.
    if let Some(info) = state.app_info.as_ref() {
        draw_app_info_overlay(f, info, chunks[0]);
    }
    if show_status {
        draw_status(f, state, chunks[1]);
    }
    // The settings overlay needs a caret on its input row even when no editor exists (e.g. right
    // after `workspace/create`). Fall back to a zero Rect for the status area in that case — the
    // settings branch in `place_terminal_cursor` doesn't read it.
    if state.has_editor() || state.workspace_settings.is_some() {
        let buffer_area = chunks[0];
        let status_area = chunks.get(1).copied().unwrap_or(Rect::default());
        place_terminal_cursor(f, state, buffer_area, status_area);
    }
    // The hint (docs/hints.md): a quiet top-right chip. Above overlays (a picker
    // context's hints must show over the picker box) — it collides with nothing else up there.
    draw_hint_corner(f, state, chunks[0]);
    // Transient toasts: stacked in the bottom-right of the content area (above the status row) over
    // everything, since they're ephemeral feedback. Drawn last so a modal never hides them.
    draw_toast_overlay(f, state, chunks[0]);
}

/// The hint corner (docs/hints.md): one quiet "Hint: …" line in the top-right of the content
/// area — the key label in accent, the sentence dim, on the panel background. Deliberately
/// subtler than a toast: ambient chrome, not a notification.
fn draw_hint_corner(f: &mut Frame, state: &AppState, area: Rect) {
    const MARGIN_X: u16 = 2;
    const PREFIX: &str = "Hint: ";
    let Some((before, keys, after)) = &state.hint else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let max_text = (area.width as usize).saturating_sub((MARGIN_X * 2 + 2) as usize);
    let fixed = PREFIX.width() + before.width() + keys.width();
    let mut after = after.clone();
    if fixed + after.width() > max_text {
        if fixed + 1 > max_text {
            return; // no room at all — skip rather than render garbage
        }
        after = truncate_to_width(&after, max_text - fixed);
    }
    let box_w = (1 + fixed + after.width() + 1) as u16;
    let rect = Rect {
        x: area.x + area.width.saturating_sub(box_w + MARGIN_X),
        y: area.y,
        width: box_w,
        height: 1,
    };
    f.render_widget(Clear, rect);
    let tint = Style::default().bg(c(th().bg_panel));
    let dim = tint.fg(c(th().fg_dim));
    let spans = vec![
        Span::styled(" ".to_string(), tint),
        Span::styled(PREFIX.to_string(), dim),
        Span::styled(before.clone(), dim),
        Span::styled(keys.clone(), tint.fg(c(th().accent))),
        Span::styled(after, dim),
        Span::styled(" ".to_string(), tint),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)).style(tint), rect);
}

/// Mute every cell in `area` to a faint grey on the base background — the modal backdrop. Keeps the
/// glyphs (so the content stays faintly legible) and their emphasis (italics / bold / underline are
/// preserved — `set_style` only patches the fields we set), just flattening the colour, so a dialog
/// painted on top reads as the only live thing on screen.
fn dim_backdrop(buf: &mut Buffer, area: Rect) {
    // Override only fg/bg; leave the cell's existing modifiers (italic etc.) intact.
    let dim = Style::default().fg(c(th().fg_faint)).bg(c(th().bg));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(dim);
            }
        }
    }
}

/// Workspace-settings overlay. A bordered modal (no border title) holding, top-to-bottom:
/// a `Workspace settings (<name>)` heading, a blank row, a `Workspace roots:` section label, the
/// list of roots, an always-present "Add root..." input row, and — when the last add/remove
/// attempt failed — a red error footer. Selection highlights the path text (bold + accent) on
/// root rows only; the input row carries no highlight (its terminal caret is the focus cue).
fn draw_workspace_settings_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(settings) = state.workspace_settings.as_ref() else {
        return;
    };
    let box_area = picker_box_rect(area);
    let Some(layout) = settings_layout(box_area, settings.error.is_some()) else {
        return;
    };
    f.render_widget(Clear, box_area);
    let block = overlay_block();
    f.render_widget(block, box_area);

    draw_settings_header(f, settings, layout.header);
    draw_settings_rows(f, state, settings, layout.rows);
    if let (Some(err_area), Some(msg)) = (layout.error, settings.error.as_deref()) {
        let style = Style::default().fg(c(th().error)).bg(c(th().bg));
        let text = truncate_right(msg, err_area.width as usize);
        f.render_widget(Paragraph::new(Span::styled(text, style)), err_area);
    }
}

/// Application-settings overlay (`Space .`): a small bordered modal of grouped checkbox settings.
/// Each group has a frost-accent header; each setting is a flush-left white label with its
/// `[✓]`/`[ ]` checkbox on the right, and its description on the line directly below (no gap). A
/// blank line separates the group header and each setting. Only the focused setting's *checkbox* is
/// highlighted (a selection-shade cell), not the whole row. Toggle-only — no caret to place.
fn draw_app_settings_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(settings) = state.app_settings.as_ref() else {
        return;
    };
    let box_area = picker_box_rect(area);
    if box_area.width < 4 || box_area.height < 3 {
        return;
    }
    f.render_widget(Clear, box_area);
    let block = overlay_block();
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
    let w = content.width as usize;

    const CHECK_W: usize = 3; // "[✓]" / "[ ]"
    let title_style = Style::default()
        .fg(c(th().fg_bright))
        .bg(c(th().bg))
        .add_modifier(Modifier::BOLD);
    let group_style = Style::default().fg(c(th().accent)).bg(c(th().bg));
    let desc_style = Style::default().fg(c(th().fg_faint)).bg(c(th().bg));

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        truncate_right("Application settings", w),
        title_style,
    )));

    // Walk the groups, tracking the running flat row index so the focus lands on the right
    // checkbox (group headers aren't part of the index space).
    let mut flat = 0usize;
    for group in &settings.groups {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            truncate_right(group.title, w),
            group_style,
        )));
        for row in &group.rows {
            let selected = flat == settings.selected;
            // A gap before each setting (separating it from the header / the previous setting).
            lines.push(Line::from(""));
            // Label flush-left, control right-aligned. Only the control carries the focus highlight
            // (the selection background). A toggle shows a checkbox; a stepped value (font size) shows the
            // number — the terminal can't change its own font, but the value is shown for parity and
            // because the setting is synced (it drives the GUI/web clients).
            let (control_text, control_fg) = match row.control {
                AppSettingControl::Toggle(true) => ("[\u{2713}]".to_string(), c(th().accent)),
                AppSettingControl::Toggle(false) => ("[ ]".to_string(), c(th().fg_bright)),
                AppSettingControl::Value(v) => (v.to_string(), c(th().accent)),
            };
            let control_w = control_text.chars().count().max(CHECK_W);
            let check_bg = c(if selected { th().bg_selection } else { th().bg });
            let label_budget = w.saturating_sub(control_w + 1);
            let label = truncate_right(row.label, label_budget);
            let pad = w
                .saturating_sub(control_w)
                .saturating_sub(label.chars().count());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}{}", label, " ".repeat(pad)),
                    Style::default().fg(c(th().fg_bright)).bg(c(th().bg)),
                ),
                Span::styled(
                    format!("{control_text:>control_w$}"),
                    Style::default().fg(control_fg).bg(check_bg),
                ),
            ]));
            // Description on the very next line — grouped tight under the label.
            lines.push(Line::from(Span::styled(
                truncate_right(row.hint, w),
                desc_style,
            )));
            flat += 1;
        }
    }

    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(c(th().fg)).bg(c(th().bg))),
        content,
    );
}

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

/// The on-screen rectangle of the app-info dialog (border included), or `None` when it isn't open.
/// Used by the mouse handler to hit-test a press: inside is swallowed, outside dismisses. Derived
/// from the same [`picker_box_rect`] the renderer uses, against the editor area reconstructed from
/// the stored viewport size — so the hit box can't drift from what was drawn.
pub fn app_info_rect(state: &AppState) -> Option<Rect> {
    state.app_info.as_ref()?;
    let area = Rect::new(0, 0, state.viewport_cols as u16, state.viewport_rows as u16);
    Some(picker_box_rect(area))
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
    let block = overlay_block().border_style(Style::default().fg(hover_border_color(&hover.body)));
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
            .style(Style::default().bg(c(th().bg)).fg(c(th().fg)))
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
/// gutter dot / text), or the accent (frost blue in dark) for a Markdown LSP-hover popup.
fn hover_border_color(body: &crate::app::HoverBody) -> Color {
    match body {
        crate::app::HoverBody::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.severity)
            .max_by_key(|s| severity_rank(*s))
            .map_or(c(th().accent), diag_color),
        crate::app::HoverBody::Markdown(_) => c(th().accent),
    }
}

/// Render a hover body to fully-styled, width-wrapped display lines. Diagnostic blocks keep their
/// severity-icon prefix and colour; Markdown is rendered with headings, code backgrounds, inline
/// emphasis, list indentation, and styled (non-clickable) links.
fn render_hover_lines(body: &crate::app::HoverBody, width: usize) -> Vec<Line<'static>> {
    match body {
        crate::app::HoverBody::Blocks(blocks) => hover_display_lines(blocks, width)
            .into_iter()
            .map(|(text, severity)| {
                let fg = severity.map_or(c(th().fg), diag_color);
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
            let base = Style::default()
                .fg(c(th().fg_bright))
                .add_modifier(Modifier::BOLD);
            let segs = md_inline_segs(content, base);
            wrap_styled(&segs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        MdBlock::Paragraph { content, .. } => {
            let segs = md_inline_segs(content, Style::default().fg(c(th().fg)));
            wrap_styled(&segs, width)
                .into_iter()
                .map(Line::from)
                .collect()
        }
        MdBlock::Code { code, .. } => {
            // Each code line gets a code background, padded out to the full width so the block reads
            // as a solid panel.
            let style = Style::default().fg(c(th().fg)).bg(c(th().md_code_bg));
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
        MdBlock::List {
            ordered,
            start,
            items,
            ..
        } => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let mut marker = if *ordered {
                    format!("{}. ", start + i as u64)
                } else {
                    "• ".to_string()
                };
                // Task-list items carry their checkbox on the marker.
                if let Some(done) = item.checked {
                    marker.push_str(if done { "☑ " } else { "☐ " });
                }
                let indent = " ".repeat(marker.width());
                let inner_w = width.saturating_sub(marker.width());
                let item_lines = md_hover_lines(&item.blocks, inner_w);
                for (j, line) in item_lines.into_iter().enumerate() {
                    // First line of the item carries the bullet/number; continuation lines hang under
                    // the text with a matching indent.
                    let prefix = if j == 0 {
                        marker.clone()
                    } else {
                        indent.clone()
                    };
                    let mut spans = vec![Span::styled(prefix, Style::default().fg(c(th().fg)))];
                    spans.extend(line.spans);
                    out.push(Line::from(spans));
                }
            }
            out
        }
        MdBlock::Quote { content, .. } => {
            let bar = Span::styled("│ ", Style::default().fg(c(th().fg_faint)));
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
        MdBlock::Rule { .. } => {
            vec![Line::from(Span::styled(
                "─".repeat(width),
                Style::default().fg(c(th().fg_faint)),
            ))]
        }
        // The remaining kinds only occur in document (reading-view) parses; hover content never
        // produces them, but a fallback keeps hover total. The reading view has its own renderer.
        MdBlock::Table { head, rows, .. } => std::iter::once(head)
            .chain(rows.iter())
            .filter(|row| !row.is_empty())
            .map(|row| {
                let cells: Vec<String> = row
                    .iter()
                    .map(|c| {
                        md_inline_segs(c, Style::default())
                            .into_iter()
                            .map(|(t, _)| t)
                            .collect::<String>()
                    })
                    .collect();
                Line::from(Span::styled(
                    cells.join(" | "),
                    Style::default().fg(c(th().fg)),
                ))
            })
            .collect(),
        MdBlock::Image { alt, .. } => vec![Line::from(Span::styled(
            format!("[image: {alt}]"),
            Style::default().fg(c(th().fg_faint)),
        ))],
        MdBlock::FrontMatter { .. } => Vec::new(),
        MdBlock::FootnoteDef { label, content, .. } => {
            let mut lines = vec![Line::from(Span::styled(
                format!("[{label}]:"),
                Style::default().fg(c(th().fg_faint)),
            ))];
            lines.extend(md_hover_lines(content, width));
            lines
        }
        MdBlock::Html { raw, .. } => raw
            .split('\n')
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(c(th().fg_faint)),
                ))
            })
            .collect(),
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
                out.push((text.clone(), base.fg(c(th().accent)).bg(c(th().md_code_bg))));
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
                    base.fg(c(th().accent_alt))
                        .add_modifier(Modifier::UNDERLINED),
                    out,
                );
            }
            MdInline::Strikethrough { content } => {
                md_collect_segs(content, base.add_modifier(Modifier::CROSSED_OUT), out);
            }
            MdInline::Image { alt, .. } => {
                out.push((format!("[{alt}]"), base.fg(c(th().fg_faint))))
            }
            MdInline::FootnoteRef { label, .. } => {
                out.push((format!("[{label}]"), base.fg(c(th().fg_faint))));
            }
            // Segment flow has no line-break primitive; a hard break degrades to a space in
            // hover popovers (the reading view renders breaks properly).
            MdInline::HardBreak => out.push((" ".to_string(), base)),
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

/// A 1-column vertical scrollbar over `total` lines with `visible` rows shown from `offset`.
/// Thin wrapper over [`render_scrollbar`] for static overlays (hover, search popover).
fn draw_vertical_scrollbar(f: &mut Frame, area: Rect, offset: u16, total: u16, visible: u16) {
    render_scrollbar(
        f,
        area,
        u64::from(offset),
        u64::from(total),
        u64::from(visible),
    );
}

/// The one TUI scrollbar renderer: a 1-column track in the leftmost column of `area`, with a
/// thumb sized `visible/total` of the height and positioned at `offset/total`. Geometry comes
/// from [`aether_client::scrollbar::thumb`] (shared with the other shells); the glyphs/colours
/// (a bolder `┃` thumb in the dim foreground over a faint `│` track) are the TUI's house style,
/// shared by the editor pane,
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
    let thumb_style = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));
    let track_style = Style::default().fg(c(th().bg_selection)).bg(c(th().bg));
    for i in 0..track_h {
        let in_thumb = i >= thumb_y && i < thumb_y + thumb_h;
        let glyph = if in_thumb { "┃" } else { "│" };
        let style = if in_thumb { thumb_style } else { track_style };
        buf.set_string(area.x, area.y + i, glyph, style);
    }
}

/// Greedy word-wrap to `width` columns. Always returns at least one (possibly empty) line. Words
/// longer than `width` overflow rather than being hard-split — fine for the short overlay strings
/// (hover text, settings values) this wraps.
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

/// Label above the editable workspace-name field.
const NAME_LABEL: &str = "Name:";

/// Header block: `Workspace settings` heading, a blank spacer, the editable name field (a `Name:`
/// label with the value on the indented line below it), another blank, and the `Workspace roots:`
/// label. Degrades gracefully when the header area is shorter than its 6 rows. The value renders
/// in plain (white) text like the add-root input row; its terminal caret — placed separately — is
/// the focus cue.
fn draw_settings_header(f: &mut Frame, settings: &crate::app::WorkspaceSettingsState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let heading_style = Style::default()
        .fg(c(th().fg_bright))
        .bg(c(th().bg))
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(c(th().fg))
        .bg(c(th().bg))
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    let area_w = area.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(6);
    if area.height >= 1 {
        let heading = truncate_right("Workspace settings", area_w);
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
        // `Workspace roots:` label.
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
        lines.push(Line::from(Span::styled("Workspace roots:", label_style)));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(c(th().fg)).bg(c(th().bg))),
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
/// workspace picker); the pending-delete row swaps the path for a red `Remove "<path>"? [y/N]`
/// prompt. The input row carries no selection styling — its visible terminal caret is the focus
/// cue. Each list item is indented one column past the section label.
fn draw_settings_rows(
    f: &mut Frame,
    state: &AppState,
    settings: &crate::app::WorkspaceSettingsState,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    use crate::app::SettingsRowView;
    let base_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    let total_items = settings.rows.len();
    let max = (area.height as usize).max(1);
    // `selected` is dialog-global (0 = name field, drawn in the header). Scroll on the *display*
    // position of the focused row, which the section heading shifts down by one.
    let focused_display = settings.focused_display_index().unwrap_or(0);
    let start = focused_display
        .saturating_sub(max.saturating_sub(1))
        .min(total_items.saturating_sub(max));
    let area_w = area.width as usize;
    let placeholder_style = Style::default()
        .fg(c(th().fg_faint))
        .bg(c(th().bg))
        .add_modifier(Modifier::ITALIC);
    let mut lines: Vec<Line> = Vec::new();
    for row in settings.rows.iter().skip(start).take(max) {
        let highlighted = row.select.is_some() && row.select == Some(settings.selected);
        // 1-col indent so list items sit visually under the section label.
        let leading = Span::styled(" ", base_style);
        let text_budget = area_w.saturating_sub(1);
        // Input row placeholder / typed text: never highlighted, since the caret marks the focus.
        let input_line = |text: &str, placeholder: &str| {
            let (text, style) = if text.is_empty() {
                (placeholder.to_string(), placeholder_style)
            } else {
                (
                    text.to_string(),
                    Style::default().fg(c(th().fg)).bg(c(th().bg)),
                )
            };
            Line::from(vec![
                Span::styled(" ", base_style),
                Span::styled(text, style),
            ])
        };
        match &row.view {
            SettingsRowView::Root(root) => {
                // A colour-coded dot when the active buffer under this root is dirty / changed on
                // disk (` •`), reserving its width so the path truncates to leave room.
                let status = root_buffer_status(state, root);
                let dot_w = if status.is_some() { 2 } else { 0 };
                let truncated = truncate_middle(root, text_budget.saturating_sub(dot_w));
                let bg = picker_row_bg(highlighted);
                let path_style = Style::default().fg(c(th().fg)).bg(bg);
                let mut spans = vec![leading, Span::styled(truncated, path_style)];
                if let Some(kind) = status {
                    spans.push(Span::styled(" ".to_string(), path_style));
                    spans.push(Span::styled(
                        BUFFER_STATUS_DOT.to_string(),
                        path_style.fg(buffer_status_color(kind)),
                    ));
                }
                lines.push(Line::from(spans));
            }
            SettingsRowView::AddRoot => {
                let placeholder = if highlighted { "" } else { "Add root..." };
                lines.push(input_line(&settings.add_input.text, placeholder));
            }
            SettingsRowView::Section(label) => {
                // Flush left (no indent) so it reads as a heading over the rows beneath it, and
                // styled exactly like the header's `Workspace roots:` label.
                lines.push(Line::from(Span::styled(
                    truncate_right(label, area_w),
                    Style::default()
                        .fg(c(th().fg))
                        .bg(c(th().bg))
                        .add_modifier(Modifier::BOLD),
                )));
            }
            SettingsRowView::Blank => lines.push(Line::from("")),
            SettingsRowView::Project {
                path,
                language,
                error,
            } => {
                // Trailing tag: the language whose server this pins, or — when the declaration is
                // broken — the reason, in red. The tag is reserved first so the path truncates
                // around it rather than pushing it off the edge.
                let (tag, tag_fg) = match error {
                    Some(e) => (e.clone(), c(th().error)),
                    None => (language.clone(), c(th().fg_faint)),
                };
                let tag = truncate_right(&tag, text_budget.saturating_sub(4).min(40));
                let tag_w = tag.chars().count();
                let truncated =
                    truncate_middle(path, text_budget.saturating_sub(tag_w.saturating_add(1)));
                let bg = picker_row_bg(highlighted);
                lines.push(Line::from(vec![
                    leading,
                    Span::styled(truncated, Style::default().fg(c(th().fg)).bg(bg)),
                    Span::styled(" ".to_string(), Style::default().bg(bg)),
                    Span::styled(tag, Style::default().fg(tag_fg).bg(bg)),
                ]));
            }
            SettingsRowView::AddProject => {
                lines.push(project_editor_line(
                    &settings.add_project,
                    base_style,
                    placeholder_style,
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(lines).style(base_style), area);
}

/// Render the add-project row: a bulleted two-segment path editor mirroring the save prompt — an
/// optional root typeahead (multi-root workspaces only), then the root-relative marker path with a
/// dim ghost suggestion. Neither segment carries a selection highlight; the caret marks the focus,
/// exactly as the add-root row does.
/// The gap between the path and language segments — wide enough to read as a separate field
/// without a glyph, and the width the caret math adds when the language segment has focus.
const SEGMENT_GAP: &str = "   ";

fn project_editor_line<'a>(
    ed: &'a crate::app::ProjectEditorState,
    base_style: Style,
    placeholder_style: Style,
) -> Line<'a> {
    // Unfocused and empty, the row collapses to its affordance — the same shape the add-root row
    // has. Focused, the caret is the cue and the ghost carries the suggestion.
    if !ed.focused && ed.path_input.text.is_empty() {
        return Line::from(vec![
            Span::styled(" ", base_style),
            Span::styled("Add project...".to_string(), placeholder_style),
        ]);
    }
    let mut spans = vec![Span::styled(" ", base_style)];
    let text_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    // Same ghost tone as the chip editor's suggestion text.
    let ghost_style = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));
    if ed.multi_root {
        // While the root segment is focused it shows the typed filter plus its inline completion;
        // otherwise the settled root label.
        if ed.on_root {
            let style = if ed.root_invalid {
                text_style.fg(c(th().error))
            } else {
                text_style
            };
            spans.push(Span::styled(ed.root_input.text.clone(), style));
            if let Some(ghost) = &ed.root_ghost {
                spans.push(Span::styled(ghost.clone(), ghost_style));
            }
        } else {
            // The committed-prefix blue the dir chip editor and status bar use for a settled root.
            spans.push(Span::styled(
                ed.root_display.clone(),
                Style::default().fg(c(th().accent)).bg(c(th().bg)),
            ));
        }
        spans.push(Span::styled(": ", ghost_style));
    }
    {
        let style = if ed.path_invalid {
            text_style.fg(c(th().error))
        } else {
            text_style
        };
        spans.push(Span::styled(ed.path_input.text.clone(), style));
        // A ghost is a completion aid for the segment you're *editing*. Left showing while another
        // segment has focus it reads as part of the value — `databricks/` with a trailing
        // `.databricks/` looks like the path you're about to commit, which it isn't.
        if ed.on_path() {
            if let Some(ghost) = &ed.path_ghost {
                spans.push(Span::styled(ghost.clone(), ghost_style));
            }
        }
    }
    // The optional language override, after the path. Only drawn once there's something to say:
    // typed text, or the segment having focus (so you can see where the caret went).
    if ed.on_language || !ed.language_input.text.is_empty() {
        spans.push(Span::styled(SEGMENT_GAP, ghost_style));
        let style = if ed.language_invalid {
            text_style.fg(c(th().error))
        } else {
            text_style.fg(c(th().accent))
        };
        if ed.language_input.text.is_empty() {
            spans.push(Span::styled("language…".to_string(), placeholder_style));
        } else {
            spans.push(Span::styled(ed.language_input.text.clone(), style));
            if let Some(ghost) = &ed.language_ghost {
                spans.push(Span::styled(ghost.clone(), ghost_style));
            }
        }
    }
    Line::from(spans)
}

/// Place the terminal caret on the settings overlay's name value (header line 3 — below the
/// heading, blank spacer, and `Name:` label, indented one column). Mirrors `draw_settings_header`.
/// Only places the caret when the header is tall enough to show the value; otherwise leaves it
/// unset (ratatui hides it).
fn place_settings_name_cursor(
    f: &mut Frame,
    settings: &crate::app::WorkspaceSettingsState,
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
/// `draw_workspace_settings_overlay`: same inner padding, same error-footer split, same scroll
/// slide. Only places the caret when the input row is currently visible (with a small list and
/// a tall box this is almost always true; if it scrolled off, we just leave the caret unset and
/// ratatui hides it for the frame).
fn place_settings_input_cursor(
    f: &mut Frame,
    settings: &crate::app::WorkspaceSettingsState,
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
    let total_items = settings.rows.len();
    let max = (rows.height as usize).max(1);
    // See `draw_settings_rows`: scroll on the focused row's *display* position, which the section
    // heading shifts relative to the selection index.
    let Some(input_idx) = settings.focused_display_index() else {
        return;
    };
    let start = input_idx
        .saturating_sub(max.saturating_sub(1))
        .min(total_items.saturating_sub(max));
    if input_idx < start || input_idx >= start + max {
        return;
    }
    // Which input owns the caret, and how far the row's own prefix pushes it right. The
    // add-project row is a two-segment editor: on its path segment the caret sits past the
    // rendered `root: ` prefix.
    let ed = &settings.add_project;
    let root_w = if ed.multi_root {
        (ed.root_display.width() + 2) as u16 // "label" + ": "
    } else {
        0
    };
    let (input, prefix_w) = match settings.focused() {
        Some(crate::app::SettingsRowView::AddRoot) => (&settings.add_input, 0),
        Some(crate::app::SettingsRowView::AddProject) if ed.on_root => (&ed.root_input, 0),
        // The language segment is drawn after the path plus its two-space gap.
        Some(crate::app::SettingsRowView::AddProject) if ed.on_language => (
            &ed.language_input,
            root_w + ed.path_input.text.width() as u16 + SEGMENT_GAP.len() as u16,
        ),
        Some(crate::app::SettingsRowView::AddProject) => (&ed.path_input, root_w),
        _ => return,
    };
    let row_y = rows.y + (input_idx - start) as u16;
    // +1 for the leading " " indent each list item carries.
    let typed_w = input.width_to_cursor() as u16 + prefix_w;
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

/// Empty no-workspace view: a centered hint telling the user how to open the workspace picker.
/// Drawn instead of the buffer pane when `state.editor` is `None`. Fills the full pane in the
/// editor background so the no-workspace state visually matches an open editor instead of
/// falling through to the terminal's default colors.
/// The backdrop behind the Workspaces chooser before any workspace is selected: a bare
/// editor-background fill,
/// matching the native client's boot view. The chooser is the only UI here — dismissing it exits
/// the app (the shell sets `should_quit`), so this is only ever a momentary flash.
fn draw_no_workspace_view(f: &mut Frame, _state: &AppState, area: Rect) {
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(c(th().bg))),
        area,
    );
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

/// The one-line message shown in place of the rows when the picker is empty, or `None` when rows
/// render (or nothing should show — an unqueried Grep). The async kinds (References / DocumentSymbols)
/// get a kind-specific "Finding…" line while loading; everything else, and the async kinds once
/// settled, shows the core-owned [`empty_note`](aether_client::picker::PickerState::empty_note)
/// ("No diagnostics" / "No matches" / …). Single source for the TUI's empty pane — shared by
/// [`picker_content_rows`] (which reserves a row for it) and [`draw_picker_results`] (which draws it).
fn picker_empty_message(picker: &crate::picker::PickerState) -> Option<&str> {
    use aether_protocol::picker::PickerKind;
    if !picker.items.is_empty() {
        return None;
    }
    let async_kind = matches!(
        picker.kind,
        Some(PickerKind::References | PickerKind::DocumentSymbols | PickerKind::WorkspaceSymbols)
    );
    if picker.ticking && async_kind {
        return Some(match picker.kind {
            Some(PickerKind::DocumentSymbols) => "Finding symbols…",
            // Plural servers, and slow enough to be worth naming what's happening: pinned project
            // servers answer one at a time and a cold one can take seconds.
            Some(PickerKind::WorkspaceSymbols) => "Searching projects…",
            _ => "Finding references…",
        });
    }
    // `empty_note` is already `None` while ticking (non-async) or for an unqueried Grep.
    picker.empty_note.as_deref()
}

/// Rows the full result set needs in the results pane — what the picker box collapses to when
/// that's shorter than the full-size box (mirroring the web client, whose list shrinks to fit
/// content). Grep uses the server-reported display-row total (hits + per-file headers), plus
/// one blank gap row between each pair of groups (`total groups − 1`, where the group count is
/// the header rows: `total_display_rows − total_matches`); an empty picker with a message
/// ([`picker_empty_message`]) needs one row for it; the client-side synthetic "Create …" row
/// isn't counted in `total_matches`.
fn picker_content_rows(picker: &crate::picker::PickerState) -> u32 {
    if picker_empty_message(picker).is_some() {
        return 1; // one row for the loading / empty message ("Finding references…", "No diagnostics", …)
    }
    if picker.kind.is_some_and(|k| k.renders_group_headers()) {
        let rows = picker.total_display_rows.unwrap_or(picker.total_matches);
        if picker.groups.is_empty() {
            return rows;
        }
        let gaps = rows.saturating_sub(picker.total_matches).saturating_sub(1);
        return rows + gaps;
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

/// One rendered row of the fetched picker window. The grouped kinds (grep, changes, keybindings)
/// interleave non-selectable `Header` rows and `Gap` spacer rows with the `Item` rows; the flat
/// kinds are all `Item`. This is the TUI's local expansion of the fetched window into screen rows
/// — the analogue of the core's `display_rows` that the pixel shells virtual-scroll, except the
/// TUI materialises the inter-group gap as a real blank row rather than pixel spacing. Picker
/// scroll is a *view-row* offset into this sequence, so it advances one screen row at a time even
/// across a group boundary. (An item-index scroll can't: consecutive items straddle a header + gap
/// there, so the view jumps 2–3 rows — the grep/keybindings scroll bug.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickerRow {
    /// Blank spacer above an interior group header.
    Gap,
    /// A group header; the index is into the window's `groups` spans.
    Header(usize),
    /// A selectable item; the index is into the window's `items`.
    Item(usize),
}

/// Expand the fetched window (`items_len` items described by the window's `groups` spans) into
/// view rows: a header before each group run and a blank gap above each *interior* header (never
/// above the pane's first row). The server repeats a split group's header at span `start == 0`, so
/// a window that begins mid-group still leads with its group's header. Flat windows (no spans) map
/// one item to one row, so view-row scroll collapses to item scroll — exactly the flat-picker
/// behaviour.
pub fn picker_window_rows(
    items_len: usize,
    groups: &[GroupSpan],
    collapsible: bool,
) -> Vec<PickerRow> {
    // Collapsible kinds (docs/picker-groups.md): headers arrive as real, selectable
    // `PickerItem::Group` rows, so the window maps 1:1 — no interleaved headers, no gaps.
    // The spans only feed the sticky stamp ([`collapsible_pin`]).
    if collapsible {
        return (0..items_len).map(PickerRow::Item).collect();
    }
    let mut rows = Vec::with_capacity(items_len + groups.len() * 2);
    let mut gi = 0;
    for i in 0..items_len {
        while gi < groups.len() && groups[gi].start as usize == i {
            if !rows.is_empty() {
                rows.push(PickerRow::Gap); // interior header opens with a gap; the list top never does
            }
            rows.push(PickerRow::Header(gi));
            gi += 1;
        }
        rows.push(PickerRow::Item(i));
    }
    rows
}

/// The span to stamp over the pane's top row for a *collapsible* pinning kind: the governing
/// run's span, but only when the top view row sits inside the expanded run — when the top row
/// IS the run's own `Group` header it already renders itself identically, so no stamp (and a
/// press there hits the row, which is selectable). Shared by the draw and the hit-test so the
/// two can't disagree about whether row 0 is covered.
pub fn collapsible_pin<'a>(
    state: &'a AppState,
    rows: &[PickerRow],
    top: usize,
) -> Option<&'a GroupSpan> {
    let kind = state.picker.kind?;
    if !kind.collapsible() || !pins_group_header(kind) {
        return None;
    }
    let PickerRow::Item(i) = rows.get(top)? else {
        return None;
    };
    if matches!(
        state.picker.items.get(*i),
        Some(PickerItem::Group { .. }) | None
    ) {
        return None;
    }
    state
        .picker
        .groups
        .iter()
        .rev()
        .find(|s| (s.start as usize) <= *i)
}

/// The group whose header should pin over the pane top when view row `top` is first visible: the
/// last group starting at or before `top`. `None` for a flat window (no headers).
pub fn picker_governing_group(rows: &[PickerRow], top: usize) -> Option<usize> {
    let top = top.min(rows.len().saturating_sub(1));
    rows.get(..=top)?.iter().rev().find_map(|r| match r {
        PickerRow::Header(gi) => Some(*gi),
        _ => None,
    })
}

/// Whether this picker kind pins a sticky group header over the pane's top row — mirrors the
/// native/web clients (`aether_iced::picker::pins_group_header`): the file-grouped kinds, the
/// Jumplist, and Keybindings pin their header; References renders section labels but
/// deliberately doesn't pin.
/// The pinned header covers the top view row, so the scroll math keeps the selection below it.
pub fn pins_group_header(kind: PickerKind) -> bool {
    kind.groups_by_file() || matches!(kind, PickerKind::Keybindings | PickerKind::Jumplist)
}

/// The view-row scroll offset (`top`, an index into [`picker_window_rows`]) that keeps the selected
/// item on screen and clear of the pinned header, given the current offset. `pin` is whether this
/// kind pins a sticky header over the pane's top row (that row is then covered, so the selection
/// must sit below it). Moving the selection one item shifts `top` by at most one row — the smooth
/// one-row scroll the item-index math couldn't manage across a group boundary.
pub fn picker_row_scroll_for_selected(
    rows: &[PickerRow],
    selected_item: usize,
    current_top: usize,
    pane_height: usize,
    pin: bool,
) -> usize {
    let pane = pane_height.max(1);
    let max_top = rows.len().saturating_sub(pane);
    let Some(sel_row) = rows
        .iter()
        .position(|r| matches!(r, PickerRow::Item(i) if *i == selected_item))
    else {
        return current_top.min(max_top);
    };
    let pin_rows = pin as usize; // the pinned header covers the top view row
    let mut top = current_top.min(max_top);
    if sel_row < top + pin_rows {
        // Above the window, or hidden under the pin: reveal it just below the pinned header.
        top = sel_row.saturating_sub(pin_rows);
    } else if sel_row >= top + pane {
        // Below the window: bottom-align so the selection is the last visible row.
        top = sel_row + 1 - pane;
    }
    top.min(max_top)
}

/// The §9 group-run reveal (docs/picker-groups.md): the pane-top view row that frames the
/// freshly-opened run — the minimal move from `top` that brings the run's last row into view,
/// capped so the run's header never leaves the pane top. A run taller than the pane shows the
/// header at the very top, where the row renders *itself* (the sticky pin only stamps when the
/// top row sits inside the run), so capping there keeps the first item visible below it.
/// `header_rel` is the header's window-relative view row; rows are window view rows
/// (collapsible kinds map 1:1 to items).
pub fn picker_scroll_for_run(
    top: usize,
    pane_height: usize,
    header_rel: usize,
    len: usize,
) -> usize {
    let pane = pane_height.max(1);
    let last = header_rel + len;
    if header_rel < top {
        // Header above the pane (a backward step): align it to the top.
        header_rel
    } else if last >= top + pane {
        // Run overflows the pane bottom: scroll the minimum that shows its last row, capped
        // so the header stays visible.
        (last + 1 - pane).min(header_rel)
    } else {
        top
    }
}

/// The shell's picker-scroll continuity state: the first visible view row (`top`), the selected
/// item's on-screen row within the pane (`sel_pane`), and the `core.offset` these were computed
/// against (`offset`). Threaded across syncs so the scroll survives a recentering refetch.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PickerScroll {
    pub top: usize,
    pub sel_pane: usize,
    pub offset: u32,
}

/// Advance the picker-scroll state for one sync. Normally seeds the keep-on-screen math from the
/// previous `top`; when a refetch recentered the window (`cur_offset` differs from the tracked
/// `offset`) it reseeds from the selection's remembered pane row so the on-screen position carries
/// across the view-row-space shift.
///
/// Crucially, when the selected item isn't in the fetched window — `items` cleared during an
/// in-flight refetch, or the selection momentarily past the window under fast (mouse-wheel)
/// scrolling — it **preserves** `sel_pane` and `offset` rather than recomputing them from a missing
/// selection. Recomputing there would collapse `sel_pane` to 0 and, when the real window lands,
/// snap the selection to the pane top. That empty-frame clobber is the intermittent "selection
/// jumps to the top" seen when scrolling fast. `top` still follows for rendering.
pub fn picker_scroll_step(
    rows: &[PickerRow],
    selected_item: usize,
    pane_height: usize,
    pin: bool,
    cur_offset: u32,
    prev: PickerScroll,
) -> PickerScroll {
    match rows
        .iter()
        .position(|r| matches!(r, PickerRow::Item(i) if *i == selected_item))
    {
        Some(sel_row) => {
            let seed = if cur_offset == prev.offset {
                prev.top
            } else {
                sel_row.saturating_sub(prev.sel_pane)
            };
            let top = picker_row_scroll_for_selected(rows, selected_item, seed, pane_height, pin);
            PickerScroll {
                top,
                sel_pane: sel_row.saturating_sub(top),
                offset: cur_offset,
            }
        }
        // Selection not in the fetched window (refetch in flight): keep the continuity anchor,
        // only clamp `top` so the render stays in bounds.
        None => PickerScroll {
            top: prev.top.min(rows.len()),
            sel_pane: prev.sel_pane,
            offset: prev.offset,
        },
    }
}

/// Shared frame for the floating overlays (picker, the two settings modals, LSP detail, hover):
/// rounded corners + the overlay-border grey over the editor background, mirroring the rounded
/// "card" the native
/// and web clients draw. Callers that tint the border (the hover popup keys it to diagnostic
/// severity) chain `.border_style(...)` afterwards to override the default.
fn overlay_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(c(th().overlay_border)))
        .style(Style::default().bg(c(th().bg)).fg(c(th().fg)))
}

/// The picker overlay's on-screen geometry. [`draw_picker_overlay`] paints from it and
/// [`picker_hit`] maps mouse presses back through it, so the renderer and the pointer can't drift
/// apart. Rects are absolute (terminal cells) and already horizontally padded where the painter
/// pads.
struct PickerLayout {
    /// The bordered box, borders included.
    box_area: Rect,
    /// The query / chips row.
    input: Rect,
    /// The chip-editor line below the input, when one is open.
    chip_editor: Option<Rect>,
    /// The full-width rule under the input, when the box has content to separate.
    separator: Option<Rect>,
    /// The results pane — the rows [`picker_window_rows`] fills.
    results: Rect,
}

/// Lay out the picker overlay inside `area` (the content pane). `None` when the box would be too
/// small to draw anything meaningful — the renderer then draws nothing, so nothing is hit-testable
/// either.
fn picker_layout(state: &AppState, area: Rect) -> Option<PickerLayout> {
    let editor_open = state.picker.chip_editor.is_some();
    let content_rows = picker_content_rows(&state.picker);
    let box_area = collapsed_picker_box_rect(area, content_rows, editor_open);
    if box_area.width < 4 || box_area.height < 3 {
        return None;
    }
    // Inner layout: input row, the chip editor line when one is open (revealed *below* the
    // input so chips + query stay visible while editing), separator row (full-width, ties into
    // the borders), results. The separator row only exists when there's content to separate —
    // matching the chrome math in `collapsed_picker_box_rect`. This isn't just cosmetic: an
    // overconstrained vertical split (more `Length(1)` rows than the collapsed box has) makes
    // ratatui's solver zero out an *earlier* row, so an unconditional separator constraint in
    // a content-less box would swallow the editor line and render the separator in its place.
    let inner = overlay_block().inner(box_area);
    let has_content = content_rows > 0;
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
    let mut next = 1;
    let chip_editor = editor_open.then(|| {
        let r = rows[next];
        next += 1;
        pad_horizontal(r)
    });
    let separator = has_content.then(|| {
        let r = rows[next];
        next += 1;
        r
    });
    Some(PickerLayout {
        box_area,
        input: pad_horizontal(rows[0]),
        chip_editor,
        separator,
        results: pad_horizontal(rows[next]),
    })
}

fn draw_picker_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let Some(layout) = picker_layout(state, area) else {
        return; // Too small to draw anything meaningful.
    };
    f.render_widget(Clear, layout.box_area);
    f.render_widget(overlay_block(), layout.box_area);
    draw_picker_input_row(f, state, layout.input);
    if let Some(editor) = layout.chip_editor {
        draw_chip_editor_row(f, state, editor);
    }
    if let Some(separator) = layout.separator {
        draw_picker_separator(f, layout.box_area, separator);
    }
    draw_picker_results(f, state, layout.results);
}

/// Where a mouse press landed while the picker overlay is up — the terminal's answer to the pixel
/// shells' per-row `mouse_area` / `mousedown` handlers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickerHit {
    /// A selectable row: the item's index *within the fetched window* (add `picker.offset` for the
    /// absolute index the core's click event takes).
    Item(usize),
    /// Somewhere else inside the box — input row, chip editor, a group header, an inter-group gap,
    /// the borders, the padding, the scrollbar. Swallowed: the press does nothing.
    Chrome,
    /// Outside the box, on the dimmed backdrop.
    Backdrop,
}

/// Hit-test a press at terminal cell `(row, col)` against the picker overlay as drawn in a
/// `cols × rows` terminal. Runs the same [`picker_layout`] the renderer paints from and the same
/// view-row expansion [`draw_picker_results`] iterates, so a press always resolves to the row
/// under the pointer.
pub fn picker_hit(state: &AppState, cols: u16, rows: u16, row: u16, col: u16) -> PickerHit {
    let area = content_area(state, cols, rows);
    // Nothing drawn (a terminal too small for the box) — treat the whole screen as backdrop.
    let Some(layout) = picker_layout(state, area) else {
        return PickerHit::Backdrop;
    };
    let at = Position::new(col, row);
    if !layout.box_area.contains(at) {
        return PickerHit::Backdrop;
    }
    if !layout.results.contains(at) {
        return PickerHit::Chrome;
    }
    // The pane holds the empty / "Finding…" message instead of rows.
    if picker_empty_message(&state.picker).is_some() {
        return PickerHit::Chrome;
    }
    let collapsible = state.picker.kind.is_some_and(PickerKind::collapsible);
    let view_rows = picker_window_rows(state.picker.items.len(), &state.picker.groups, collapsible);
    let top = state.picker.visible_start.min(view_rows.len());
    let pane_row = (row - layout.results.y) as usize;
    // The sticky group header is stamped *over* the pane's top row, so a press there hits the
    // header — not the item it covers (mirrors the pin in `draw_picker_results`). For the
    // collapsible kinds the stamp exists only mid-run (`collapsible_pin`); a `Group` row at the
    // top renders itself and stays clickable.
    let pinned = if collapsible {
        collapsible_pin(state, &view_rows, top).is_some()
    } else {
        state.picker.kind.is_some_and(pins_group_header)
            && !state.picker.groups.is_empty()
            && picker_governing_group(&view_rows, top).is_some()
    };
    if pane_row == 0 && pinned {
        return PickerHit::Chrome;
    }
    match view_rows.get(top + pane_row) {
        Some(PickerRow::Item(i)) => PickerHit::Item(*i),
        // A header, a gap, or past the last row: nothing to select.
        _ => PickerHit::Chrome,
    }
}

/// The LSP-server detail screen (`Space l` → Enter on a server): a full-size box rendering the
/// server's status. It's a modal prompt (`Prompt::LspInfo`), *not* a picker sub-state — the core
/// closes the picker when drilling in — so it draws as its own top-level overlay rather than
/// inside the picker box. `Ctrl-r` restarts the server; any other key closes.
fn draw_lsp_detail_overlay(f: &mut Frame, detail: &crate::picker::LspServerDetail, area: Rect) {
    let box_area = picker_box_rect(area);
    if box_area.width < 4 || box_area.height < 3 {
        return;
    }
    f.render_widget(Clear, box_area);
    let block = overlay_block();
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    draw_lsp_detail(f, detail, pad_horizontal(inner));
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
        c(th().warning)
    } else {
        lsp_status_color(&detail.status)
    };
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("• ".to_string(), Style::default().fg(dot_color)),
            Span::styled(
                detail.name.clone(),
                Style::default().fg(c(th().fg)).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    let w = text_w as usize;
    push_detail_row(&mut lines, "Language", &detail.language, c(th().fg), w);
    push_detail_row(
        &mut lines,
        "Workspace",
        &detail.workspace_root,
        c(th().fg),
        w,
    );
    if let LspStatus::Crashed { code, message } = &detail.status {
        let mut msg = message.clone();
        if let Some(c) = code {
            msg.push_str(&format!(" (exit code {c})"));
        }
        push_detail_row(&mut lines, "Error", &msg, c(th().error), w);
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
        push_detail_row(
            &mut lines,
            if i == 0 { "Working" } else { "" },
            &text,
            c(th().warning),
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
            .style(Style::default().bg(c(th().bg)).fg(c(th().fg)))
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

/// The application-info dialog (`Space ?`): build identity, the daemon we're connected to, and
/// where this profile's state lives. Shares the LSP detail's box and label/value rows — they're the
/// same kind of screen, and the section headings are the only thing this one adds.
///
/// Content is entirely the core's ([`aether_client::app_info::sections`]); the TUI picks the box
/// and the colours.
fn draw_app_info_overlay(f: &mut Frame, info: &crate::app::AppInfoView, area: Rect) {
    let box_area = picker_box_rect(area);
    if box_area.width < 4 || box_area.height < 3 {
        return;
    }
    f.render_widget(Clear, box_area);
    let block = overlay_block();
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    let area = pad_horizontal(inner);
    let text_w = area.width.saturating_sub(2).max(1) as usize; // reserve the scrollbar column + gap

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "Aether".to_string(),
            Style::default()
                .fg(c(th().fg_bright))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (i, section) in info.sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title.to_string(),
            Style::default().fg(c(th().accent)),
        )));
        for row in &section.rows {
            // Yellow is the drift warning — the one row here that means something is wrong rather
            // than merely being a fact about the install.
            let color = match row.tone {
                aether_client::app_info::InfoTone::Warn => c(th().warning),
                aether_client::app_info::InfoTone::Normal => c(th().fg),
            };
            push_detail_row(&mut lines, row.label, &row.value, color, text_w);
        }
    }
    let total = lines.len() as u16;
    let body_h = area.height;
    info.scroll.record(total, body_h);
    let offset = info.scroll.offset();
    let text_area = Rect {
        width: text_w as u16,
        ..area
    };
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(c(th().bg)).fg(c(th().fg)))
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

/// One labelled row of a detail dialog (LSP server, app info): a dim `Label` column, then the value
/// in `color`, wrapped to the remaining width with continuation lines indented to the value column.
/// An empty label keeps the column (wrap continuations; second and later Working operations).
fn push_detail_row(
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
            Span::styled(
                format!("{lbl:<KEY_W$}"),
                Style::default().fg(c(th().fg_faint)),
            ),
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
    let base_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    let placeholder_style = Style::default()
        .fg(c(th().fg_faint))
        .bg(c(th().bg))
        .add_modifier(Modifier::ITALIC);
    // Both the root label and the relative-path portion are *committed* parts of the prefix —
    // colour them the same blue so the contrast in the row reads as "committed prefix" (blue)
    // vs "editable query" (default fg). Mirrored in the save-as prompt renderer.
    let label_style = Style::default().fg(c(th().accent)).bg(c(th().bg));
    let path_style = Style::default().fg(c(th().accent)).bg(c(th().bg));

    let total_width = area.width as usize;
    let (label_text, path_text) = explorer_input_prefix(state, total_width);
    let prefix_w = label_text.width() + path_text.width();
    let (chip_spans, chips_w) = picker_chip_spans(state, chip_budget(total_width, prefix_w));
    let prefix_has_content = prefix_w > 0 || chips_w > 0;

    // The explorer tab-completion ghost: the common-prefix suffix `Tab` would append, dim after
    // the query (even when the query is empty — a fresh dir whose entries all share a prefix).
    let ghost = state
        .picker
        .completion
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let ghost_style = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));

    let (left_text, left_style, left_w) = if state.picker.query.is_empty() {
        // Suppress the placeholder when the explorer prefix, a chip row, or a completion ghost is
        // already telling the user what's in effect — that *is* the context. Otherwise keep it.
        if prefix_has_content || ghost.is_some() {
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
        // A list narrowed *below* its candidate set shows `matched/total`; an unfiltered list — and
        // grep, where every candidate is a hit — collapses to a single total. Guarded on `>` rather
        // than `!=` so a candidate count that isn't a larger superset (e.g. an async picker whose
        // fill push raced ahead of the view response, leaving a stale 0) reads as just the match
        // count, not a misleading `106/0`. A throbber sits to the left while results stream.
        let num = if state.picker.total_candidates > state.picker.total_matches {
            format!(
                "{}/{}",
                state.picker.total_matches, state.picker.total_candidates
            )
        } else {
            format!("{}", state.picker.total_matches)
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
    let ghost_w = ghost.as_deref().map(str::width).unwrap_or(0);
    if let Some(ghost) = ghost {
        spans.push(Span::styled(ghost, ghost_style));
    }
    let used = prefix_w + chips_w + left_w + ghost_w;
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
    let label_style = Style::default().fg(c(th().accent)).bg(c(th().bg));
    let text_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    let ghost_style = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));
    // An invalid segment (root matching no label / path that doesn't exist) renders red — the
    // visible form of "the commit gate will refuse this".
    let invalid_style = Style::default().fg(c(th().error)).bg(c(th().bg));

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w: usize = 0;
    let mut cursor: usize = 0;
    let push = |spans: &mut Vec<Span<'static>>, w: &mut usize, text: String, style: Style| {
        *w += text.width();
        spans.push(Span::styled(text, style));
    };

    match ed.kind {
        ChipEditorKind::Glob { .. } => {
            push(&mut spans, &mut w, format!("{} ", ed.tag), label_style);
            cursor = w + ed.input.width_to_cursor();
            push(&mut spans, &mut w, ed.input.text.clone(), text_style);
        }
        ChipEditorKind::Dir { .. } => {
            // One field to the eye: `{tag} {root}: {path}` in multi-root workspaces (the same
            // `root: path` shape the dir chip and status bar use), `{tag} {path}` in
            // single-root ones — `tag` is `path:` (file-or-dir scope) or `dir:` (Files picker).
            // The root segment while unfocused — and the `:` separator — render in the
            // committed-prefix blue; the focused segment carries the caret; invalid go red.
            let multi_root = state.workspace_paths.len() > 1;
            push(&mut spans, &mut w, format!("{} ", ed.tag), label_style);
            if multi_root {
                let labels = crate::labels::root_labels(&state.workspace_paths);
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
    let base_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
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
/// The search prompt's lead segment: the `/` (or `?`) prefix followed by the active match-option
/// chips, styled like the grep picker's filter chips (accent on the selection shade; the whole-word chip
/// underlined). Returns the spans plus their total display width, so the caret placement can land
/// just past them and the typed query.
fn search_prompt_lead(search: &SearchState) -> (Vec<Span<'static>>, u16) {
    let prefix = if search.extend_to_cursor { "?" } else { "/" };
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
    let mut width = prefix.width() as u16;
    if !search.option_chips.is_empty() {
        spans.push(Span::raw(" "));
        width += 1;
        let chip_style = Style::default().fg(c(th().accent)).bg(c(th().bg_selection));
        let selected_style = Style::default().fg(c(th().fg_on_accent)).bg(c(th().accent));
        for (i, (label, underline)) in search.option_chips.iter().enumerate() {
            let mut style = if search.chip_selected == Some(i) {
                selected_style
            } else {
                chip_style
            };
            if *underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            spans.push(Span::styled(label.clone(), style));
            spans.push(Span::raw(" "));
            width += label.width() as u16 + 1;
        }
    }
    (spans, width)
}

fn picker_chip_spans(state: &AppState, max_w: usize) -> (Vec<Span<'static>>, usize) {
    let chips = state.picker.chips(&state.workspace_paths);
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

    let chip_style = Style::default().fg(c(th().accent)).bg(c(th().bg_selection));
    let chip_exclude_style = Style::default().fg(c(th().error)).bg(c(th().bg_selection));
    let chip_selected_style = Style::default().fg(c(th().fg_on_accent)).bg(c(th().accent));
    let chip_selected_exclude_style = Style::default().fg(c(th().fg_on_accent)).bg(c(th().error));
    let gap_style = Style::default().fg(c(th().fg)).bg(c(th().bg));
    let marker_style = Style::default().fg(c(th().fg_faint)).bg(c(th().bg));

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
/// the root in multi-root workspaces) and a `{relative}/` segment (rendered in blue). Either may
/// be empty: the label segment is empty in single-root workspaces and at the top of a root with no
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
    let (label_part, path_part) = match crate::app::strip_longest_root(dir, &state.workspace_paths)
    {
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
        Some(aether_protocol::picker::PickerKind::Workspaces) => "Select workspace…",
        Some(aether_protocol::picker::PickerKind::Diagnostics) => "Diagnostics in current file…",
        Some(aether_protocol::picker::PickerKind::DiagnosticsWorkspace) => {
            "Diagnostics in workspace…"
        }
        Some(aether_protocol::picker::PickerKind::LspServers) => "List LSPs…",
        Some(aether_protocol::picker::PickerKind::References) => "List references…",
        Some(aether_protocol::picker::PickerKind::DocumentSymbols) => "Go to symbol…",
        Some(aether_protocol::picker::PickerKind::WorkspaceSymbols) => "Go to symbol in workspace…",
        Some(aether_protocol::picker::PickerKind::GitChangesFile) => "Changes in current file…",
        Some(aether_protocol::picker::PickerKind::GitChanges) => "Changes in workspace…",
        Some(aether_protocol::picker::PickerKind::Keybindings) => "Find keybinding…",
        Some(aether_protocol::picker::PickerKind::Jumplist) => "Filter the jumplist…",
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
    // Match the frame: the rule and its `├`/`┤` tees are part of the overlay border, so they share
    // `overlay_block`'s overlay-border role.
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(c(th().overlay_border)).bg(c(th().bg))),
        area,
    );
    let buf = f.buffer_mut();
    let style = Style::default().fg(c(th().overlay_border)).bg(c(th().bg));
    let left_x = box_area.x;
    let right_x = box_area.x + box_area.width.saturating_sub(1);
    if area.y >= buf.area.y && area.y < buf.area.y + buf.area.height {
        buf.set_string(left_x, area.y, "├", style);
        buf.set_string(right_x, area.y, "┤", style);
    }
}

fn draw_picker_results(f: &mut Frame, state: &AppState, area: Rect) {
    // An empty picker shows a one-line message instead of rows: a "Finding…" progress line for the
    // async kinds (References / DocumentSymbols open empty during their LSP round-trip), and the
    // core-owned empty note ("No diagnostics", "No matches", …) for a settled-empty set. A blank
    // pane would read as broken. `picker_empty_message` is the shared decision (also drives the
    // box's row reservation); an unqueried Grep returns `None` here and falls through to draw nothing.
    if let Some(msg) = picker_empty_message(&state.picker) {
        f.render_widget(
            Paragraph::new(msg).style(
                Style::default()
                    .bg(c(th().bg))
                    .fg(c(th().fg_faint))
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

    // Expand the fetched window into view rows (header / gap / item) and render the slice
    // `[top, top + pane_height)`. Scrolling is by view row, so grep / keybindings advance one
    // screen row at a time even across a group boundary; the rows outside the slice are the
    // over-fetched cache that lets us scroll without an RPC.
    let pane_height = area.height as usize;
    let groups = &state.picker.groups;
    let collapsible = state.picker.kind.is_some_and(PickerKind::collapsible);
    let rows = picker_window_rows(state.picker.items.len(), groups, collapsible);
    let top = state.picker.visible_start.min(rows.len());
    let end = (top + pane_height).min(rows.len());

    let mut lines: Vec<Line> = Vec::with_capacity(pane_height);
    // Sections (References' Definition/References split, keybinding groups) render their label
    // verbatim; file groups render the workspace-relative path with the root label.
    let header_spans = |header: &GroupHeader| -> Vec<Span<'static>> {
        match header {
            GroupHeader::File {
                path_index,
                relative_path,
            } => grep_file_header_spans(
                *path_index,
                relative_path,
                &state.root_labels,
                text_width as usize,
            ),
            GroupHeader::Label { label } => section_header_spans(label, text_width as usize),
        }
    };
    for row in &rows[top..end] {
        let i = match row {
            // Inter-group gap: an empty line — the Paragraph's page background fills the row.
            PickerRow::Gap => {
                lines.push(Line::default());
                continue;
            }
            PickerRow::Header(gi) => {
                lines.push(Line::from(header_spans(&groups[*gi].header)));
                continue;
            }
            PickerRow::Item(i) => *i,
        };
        let item = &state.picker.items[i];
        // A staged delete renders its [y/N] confirmation *over* the target row — in the same
        // warning red the settings overlay uses for root removal — replacing the normal spans.
        // Matched by `item_key` (which ignores fuzzy-match highlight offsets) rather than the
        // selected index, so a background re-rank can't smear the prompt onto the wrong row.
        if let Some(pending) = state.picker.pending_delete.as_ref() {
            if crate::picker::item_key(item) == crate::picker::item_key(&pending.item) {
                let prefix = format!("Delete {} \"", pending.noun);
                const SUFFIX: &str = "\"? [y/N]";
                let warn_style = Style::default()
                    .fg(c(th().error))
                    .bg(c(th().bg))
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
        // Two-level hierarchy (docs/picker-groups.md §9): the collapsible kinds' item rows
        // indent two cells under their group header, aligning with the header text past its
        // "▸ "/"▾ " disclosure mark. Header rows (and everything in the flat kinds) start flush.
        let indent = collapsible && !matches!(item, PickerItem::Group { .. });
        let item_width = (text_width as usize).saturating_sub(if indent { 2 } else { 0 });
        let mut spans = picker_item_spans(
            item,
            &state.root_labels,
            state.tether,
            highlighted,
            item_width,
        );
        if indent {
            let style = if highlighted {
                Style::default().bg(c(th().bg_selection))
            } else {
                Style::default()
            };
            spans.insert(0, Span::styled("  ", style));
        }
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
                spans.push(Span::styled(
                    " ".repeat(pad),
                    Style::default().bg(c(th().bg_selection)),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    // Sticky group header: for the pinning kinds, stamp the top view row's *governing* group
    // header over the pane's first row (the web/native `position: sticky`). When that row is
    // already its group's header the overlay is identical; when it's an item scrolled up under
    // the group, the header covers it — the scroll math keeps the selection clear of this row.
    // Collapsible kinds stamp only mid-run, dressed like the `Group` row the span stands in
    // for (count + disclosure mark); their own header rows render themselves.
    if let Some(kind) = state.picker.kind {
        if collapsible {
            if let Some(span) = collapsible_pin(state, &rows, top) {
                if let Some(first) = lines.first_mut() {
                    *first = Line::from(group_row_spans(
                        &span.header,
                        span.count.unwrap_or(0),
                        span.expanded.unwrap_or(false),
                        &state.root_labels,
                        false,
                        text_width as usize,
                    ));
                }
            }
        } else if pins_group_header(kind) && !groups.is_empty() {
            if let (Some(first), Some(gi)) = (lines.first_mut(), picker_governing_group(&rows, top))
            {
                *first = Line::from(header_spans(&groups[gi].header));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(c(th().bg)).fg(c(th().fg))),
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
    let collapsible = state.picker.kind.is_some_and(PickerKind::collapsible);
    // Collapsible kinds scroll row space (headers are rows), so the bar spans the row total.
    let total = if collapsible {
        state
            .picker
            .total_display_rows
            .unwrap_or(state.picker.total_matches) as u64
    } else {
        state.picker.total_matches as u64
    };
    // Thumb size = the items actually on screen; thumb position = the absolute index of the top
    // visible item (`offset` + its window-relative index). Derive both from the view rows in the
    // slice — over-fetch means `items.len()` would oversize the thumb and `offset` alone peg it.
    let rows = picker_window_rows(state.picker.items.len(), &state.picker.groups, collapsible);
    let top = state.picker.visible_start.min(rows.len());
    let end = (top + area.height as usize).min(rows.len());
    let items_in_view = rows[top..end].iter().filter_map(|r| match r {
        PickerRow::Item(i) => Some(*i),
        _ => None,
    });
    let mut window = 0u64;
    let mut first_item = None;
    for i in items_in_view {
        first_item.get_or_insert(i);
        window += 1;
    }
    let offset = state.picker.offset as u64 + first_item.unwrap_or(0) as u64;
    render_scrollbar(f, area, offset, total, window);
}

fn picker_item_spans(
    item: &PickerItem,
    root_labels: &[String],
    tether: Option<aether_protocol::BufferId>,
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    if let PickerItem::Group {
        header,
        count,
        expanded,
    } = item
    {
        return group_row_spans(
            header,
            *count,
            *expanded,
            root_labels,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::GrepHit {
        line,
        preview,
        match_indices,
        ..
    } = item
    {
        return preview_row_spans(*line, preview, match_indices, highlighted, max_width);
    }
    // A captured entry renders exactly like a grep hit — trimmed preview + right-aligned dim line
    // number — so the two read alike (docs/jumplist.md §2.2). No dot, no dressing.
    if let PickerItem::JumplistEntry {
        line,
        display,
        match_indices,
        ..
    } = item
    {
        return preview_row_spans(*line, display, match_indices, highlighted, max_width);
    }
    if let PickerItem::GitChange {
        preview,
        match_indices,
        stage,
        added,
        removed,
        ..
    } = item
    {
        return git_change_row_spans(
            preview,
            match_indices,
            *stage,
            *added,
            *removed,
            highlighted,
            max_width,
        );
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
    // File rows get a dim disambiguated-root label; everything else falls through with the
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
    // Buffer rows get a leading dim `{label}: ` prefix for multi-root workspaces, matching the
    // status bar / title and the other clients. `display` (the match haystack) is the bare
    // relative path, so the highlight lands only on the path, not the prefix.
    if let PickerItem::Buffer {
        buffer_id,
        display,
        status,
        path_index,
        match_indices,
        transient,
        ..
    } = item
    {
        return buffer_item_spans(
            *path_index,
            display,
            match_indices,
            *status,
            *transient,
            tether == Some(*buffer_id),
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
    if let PickerItem::Symbol {
        name,
        symbol_kind,
        detail,
        depth,
        context,
        match_indices,
        ..
    } = item
    {
        return symbol_item_spans(
            SymbolRow {
                name,
                kind: *symbol_kind,
                detail,
                depth: *depth,
                context: *context,
            },
            match_indices,
            highlighted,
            max_width,
        );
    }
    if let PickerItem::Keybinding {
        desc,
        mode,
        keys,
        match_indices,
        ..
    } = item
    {
        return keybinding_item_spans(
            KeybindingRow { desc, mode, keys },
            match_indices,
            highlighted,
            max_width,
        );
    }

    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);

    // A workspace row's display label is computed (an ephemeral context becomes "(workspace N)"), so
    // it's owned here to outlive the borrow taken in the match below.
    let workspace_label = match item {
        PickerItem::Workspace { name, .. } => aether_client::labels::workspace_display(name),
        _ => String::new(),
    };

    // Right-aligned buffer-state dot — matches the status bar's colour-coded indicator and the
    // rich clients' trailing dot. Rendered after the display so it doesn't shift `match_indices`
    // (which index into the display). `None` = clean. The workspace picker reuses the same dot to
    // flag workspaces with unsaved buffers, so the two pickers read alike.
    let (display_raw, match_indices, dot_color, italic, dim) = match item {
        PickerItem::Workspace {
            name,
            unsaved_buffers,
            match_indices,
        } => {
            // An ephemeral context shows as an italic "(workspace N)"; its internal id isn't a
            // meaningful match haystack, so drop the highlight indices for it.
            let ephemeral = aether_protocol::is_ephemeral_workspace_id(name);
            (
                workspace_label.as_str(),
                if ephemeral {
                    &[][..]
                } else {
                    match_indices.as_slice()
                },
                // Frost-blue dot when the workspace has unsaved buffers, matching the unsaved
                // buffer-dot colour; nothing when clean.
                (*unsaved_buffers > 0).then_some(c(th().state_unsaved)),
                ephemeral,
                false,
            )
        }
        PickerItem::Buffer { .. }
        | PickerItem::File { .. }
        | PickerItem::GrepHit { .. }
        | PickerItem::JumplistEntry { .. }
        | PickerItem::GitChange { .. }
        | PickerItem::DirEntry { .. }
        | PickerItem::Root { .. }
        | PickerItem::Diagnostic { .. }
        | PickerItem::LspServer { .. }
        | PickerItem::Reference { .. }
        | PickerItem::Symbol { .. }
        | PickerItem::Keybinding { .. }
        | PickerItem::Group { .. } => unreachable!("handled above"),
    };
    let (base, match_style) = if italic {
        (
            base.add_modifier(Modifier::ITALIC),
            match_style.add_modifier(Modifier::ITALIC),
        )
    } else {
        (base, match_style)
    };
    // Dormant rows lose their foreground brightness (keeping the highlight bg), reading as "present
    // but not loaded" — the terminal analogue of the GUI's greyed text.
    let (base, match_style) = if dim {
        (base.fg(c(th().fg_faint)), match_style.fg(c(th().fg_faint)))
    } else {
        (base, match_style)
    };

    // The dot renders as ` •` (leading space + glyph) — reserve its width so the path truncates
    // to leave room for it.
    // Reserve 3 cols for the dot region: ≥1 separating space, the glyph, and a trailing space.
    let dot_w = if dot_color.is_some() { 3 } else { 0 };
    let text_budget = max_width.saturating_sub(dot_w);
    let (display, indices) = truncate_path_with_indices(display_raw, match_indices, text_budget);
    let display_w = display.width();

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
        // Pad out to the right so the dot floats flush-right, like the rich clients (iced's
        // `Space::Fill`, the web's `margin-left:auto`). The `●` glyph is ambiguous-width — many
        // terminals draw it two cells wide — so reserve a trailing space to keep it off the very
        // edge. `text_budget` guarantees ≥1 col of separating padding remains.
        let pad = max_width.saturating_sub(display_w + 2).max(1);
        spans.push(Span::styled(" ".repeat(pad), base));
        spans.push(Span::styled(BUFFER_STATUS_DOT.to_string(), base.fg(color)));
        spans.push(Span::styled(" ".to_string(), base));
    }
    spans
}

/// Buffer-state dot colour for a picker row, matching the editor status bar / web favicon.
/// `None` for a clean buffer (no dot).
fn buffer_dirty_dot_color(status: BufferDirtyState) -> Option<Color> {
    let t = th();
    match status {
        BufferDirtyState::Clean => None,
        BufferDirtyState::Unsaved => Some(c(t.state_unsaved)),
        BufferDirtyState::ExternallyModified => Some(c(t.state_changed)),
        BufferDirtyState::ExternallyDeleted => Some(c(t.state_deleted)),
    }
}

/// Header row above each file's hits in the Grep picker: `{label}: {relative}` (the label only
/// for multi-root workspaces), all in the accent, bold. Non-selectable; the picker cursor
/// lives on the GrepHit rows below.
fn grep_file_header_spans(
    path_index: u32,
    relative_path: &str,
    root_labels: &[String],
    max_width: usize,
) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(c(th().accent))
        .bg(c(th().bg))
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

/// A collapsible group's header row (docs/picker-groups.md) — a real, selectable row, unlike
/// the derived headers above: `▸/▾` disclosure mark + the header text in the same accent bold
/// chrome, the run's item count right-aligned dim, and the selection band when
/// highlighted like any other row.
fn group_row_spans(
    header: &GroupHeader,
    count: u32,
    expanded: bool,
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    // The accent alone carries the header chrome — no bold: with the run's items indented under
    // the header (docs/picker-groups.md §9.2) the hierarchy already reads, and the derived
    // (non-collapsible) section headers keep their bold as the visual tell for "not a row".
    let style = Style::default().fg(c(th().accent)).bg(bg);
    let count_style = Style::default().fg(picker_dim_fg(highlighted)).bg(bg);
    // The mark reads as part of the header chrome; ambiguous-width glyph, so the budget below
    // keeps ≥1 spare col (the same allowance the buffer-status dot makes).
    let mark = if expanded { "▾ " } else { "▸ " };
    let text = match header {
        GroupHeader::File {
            path_index,
            relative_path,
        } => {
            let label = root_label_or_blank(root_labels, *path_index);
            if label.is_empty() {
                relative_path.clone()
            } else {
                format!("{label}: {relative_path}")
            }
        }
        GroupHeader::Label { label } => label.clone(),
    };
    let count_str = count.to_string();
    let text_budget = max_width.saturating_sub(2 + count_str.len() + 1);
    let (display, _) = truncate_path_with_indices(&text, &[], text_budget);
    let display_w = display.width();
    let pad = max_width
        .saturating_sub(2 + display_w + count_str.len())
        .max(1);
    vec![
        Span::styled(mark.to_string(), style),
        Span::styled(display, style),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(count_str, count_style),
    ]
}

/// Picker section label (References' `Definition` / `References`, a Keybindings group) — same
/// bold header chrome as [`grep_file_header_spans`] but a label rather than a file path.
fn section_header_spans(label: &str, max_width: usize) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(c(th().accent))
        .bg(c(th().bg))
        .add_modifier(Modifier::BOLD);
    vec![Span::styled(truncate_right(label, max_width), style)]
}

/// Dim foreground for secondary text on a picker row (root labels, line numbers, locations,
/// metadata tails). The faint shade neighbours the selection background (adjacent Polar Night
/// shades in dark) and all but vanishes on it, so the highlighted row lifts its dim spans one
/// rung — to `fg_muted`, legible on the selection band yet still clearly subordinate to the
/// full-foreground primary text — the same treatment the web client gives
/// `.picker-row.selected` metadata.
fn picker_dim_fg(highlighted: bool) -> Color {
    c(if highlighted {
        th().fg_muted
    } else {
        th().fg_faint
    })
}

/// Background for a picker result row: the selection band when highlighted, else the panel
/// fill. The panel fill is the editor background — the same as the editor, so overlays stay flat rather than
/// elevated (the rounded frame + separator set the picker off instead of a raised background; see
/// [`overlay_block`]). Centralises the choice that was copy-pasted across every row builder; if the
/// overlays ever gain elevation, this `else` branch is the single place the rows would change.
fn picker_row_bg(highlighted: bool) -> Color {
    c(if highlighted {
        th().bg_selection
    } else {
        th().bg
    })
}

/// File picker row: `{relative}  {label}` — the relative path styled like other picker items
/// (fuzzy-match highlight included), then for multi-root workspaces the root's label in a dim
/// foreground (the faint shade) after it. The label is plain text — match indices in the protocol always
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
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
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

/// One Buffers-picker row: the buffer's path highlighted by `match_indices`, then (multi-root only)
/// the disambiguated root label dim after the name — same placement as the Files picker — and a
/// flush-right dirty dot. `display` is the bare relative path (the match haystack), so the highlight
/// lands only on the path, never the label. Transient buffers slant; the session's tether gets the
/// status bar's dim ` *` after the path (docs/tether.md — closing that row exits the client).
#[allow(clippy::too_many_arguments)]
fn buffer_item_spans(
    path_index: Option<u32>,
    display: &str,
    match_indices: &[u32],
    status: BufferDirtyState,
    transient: bool,
    tethered: bool,
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    let mut base = Style::default().fg(c(th().fg)).bg(bg);
    let mut match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    if transient {
        base = base.add_modifier(Modifier::ITALIC);
        match_style = match_style.add_modifier(Modifier::ITALIC);
    }
    let label_style = Style::default().fg(picker_dim_fg(highlighted)).bg(bg);

    // Suffix (multi-root only): the dim root label after the name, like the Files picker. `None`
    // path_index (scratch/external buffers) → no suffix.
    let suffix = match path_index {
        Some(i) => {
            let label = root_label_or_blank(root_labels, i);
            if label.is_empty() {
                String::new()
            } else {
                format!("  {label}")
            }
        }
        None => String::new(),
    };

    // The tether mark (docs/tether.md): a dim ` *` after the path, before the root label —
    // matching the status bar. Upright even on a slanted transient row.
    let tether_mark = if tethered { " *" } else { "" };

    // Reserve the dot region (` • ` = 3 cols) plus the tether mark and the suffix from the
    // path's truncation budget.
    let dot_w = if buffer_dirty_dot_color(status).is_some() {
        3
    } else {
        0
    };
    let path_budget = max_width
        .saturating_sub(dot_w)
        .saturating_sub(tether_mark.width())
        .saturating_sub(suffix.width());
    let (path, indices) = truncate_path_with_indices(display, match_indices, path_budget);

    let mut spans: Vec<Span<'static>> = Vec::new();
    push_styled_with_match_indices(&mut spans, &path, &indices, base, match_style);
    if !tether_mark.is_empty() {
        spans.push(Span::styled(tether_mark.to_string(), label_style));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, label_style));
    }

    if let Some(color) = buffer_dirty_dot_color(status) {
        // Float the dot flush-right, like the rich clients. Reserve a trailing space for the
        // ambiguous-width `●` glyph. Width so far = path + suffix.
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let pad = max_width.saturating_sub(used + 2).max(1);
        spans.push(Span::styled(" ".repeat(pad), base));
        spans.push(Span::styled(BUFFER_STATUS_DOT.to_string(), base.fg(color)));
        spans.push(Span::styled(" ".to_string(), base));
    }
    spans
}

/// Root row in the Explorer's Roots mode. Renders the disambiguated label as a single span;
/// match indices from the server index into the root's *basename* — which is always the start
/// of the label under option-B disambiguation — so we can apply them directly to the label
/// string. Selected row gets the standard selection background, like other pickers.
fn root_item_spans(
    path_index: u32,
    match_indices: &[u32],
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
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

/// One preview row — a Grep hit or a Jumplist entry: the preview (leading whitespace stripped)
/// with `match_indices` highlighted, then the line number right-aligned at the row's edge in a
/// dim colour — mirroring the web client's layout. An overflowing preview is cut with a dim `…`
/// so the line number (plus at least a 2-col gap) always stays visible, whatever its digit count.
fn preview_row_spans(
    line: u32,
    preview: &str,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
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

/// One Git-changes hunk row: the changed line on the left — the query match highlighted, like a
/// grep hit — then a right-aligned coloured `+added -removed` summary (green adds, red removes).
/// Staged hunks render dim (the inline-diff convention: bright = unstaged, dim = staged), so a
/// file's staged and unstaged hunks are tellable apart. `match_indices` are char offsets into
/// `preview`.
fn git_change_row_spans(
    preview: &str,
    match_indices: &[u32],
    stage: DiffStage,
    added: u32,
    removed: u32,
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let dim_style = base.fg(picker_dim_fg(highlighted));
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    // Count colours follow the gutter: bright green/red for unstaged, the dimmed shades for staged.
    let added_style = base.fg(stage_color(
        stage,
        c(th().git_added),
        c(th().git_staged_added),
    ));
    let removed_style = base.fg(stage_color(
        stage,
        c(th().git_deleted),
        c(th().git_staged_deleted),
    ));
    let gap = 2; // minimum gap between the preview and the summary

    // The summary: `-R` then `+A` (additions flush right, diffstat-style), omitting a zero side
    // (a pure add/delete shows one number).
    let summary = match (added, removed) {
        (0, 0) => String::new(),
        (a, 0) => format!("+{a}"),
        (0, r) => format!("-{r}"),
        (a, r) => format!("-{r} +{a}"),
    };
    let trimmed = preview.trim_start();
    let preview_budget = max_width.saturating_sub(gap + summary.width());

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

    // Highlight the query match within the shown text. Indices are into the (already server-
    // trimmed) preview; shift by any leading whitespace we stripped and drop any past the cut.
    let lead_chars = (preview.chars().count() - trimmed.chars().count()) as u32;
    let kept_char_count = shown.chars().count() as u32;
    let kept_indices: Vec<u32> = match_indices
        .iter()
        .filter_map(|i| i.checked_sub(lead_chars))
        .filter(|&i| i < kept_char_count)
        .collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    push_styled_with_match_indices(&mut spans, &shown, &kept_indices, base, match_style);
    if ellipsis {
        spans.push(Span::styled("…".to_string(), dim_style));
    }
    // Pad to the right edge so summaries align down the column, then the coloured counts.
    let used = shown.width() + usize::from(ellipsis) + summary.width();
    spans.push(Span::styled(
        " ".repeat(max_width.saturating_sub(used)),
        base,
    ));
    match (added, removed) {
        (0, 0) => {}
        (a, 0) => spans.push(Span::styled(format!("+{a}"), added_style)),
        (0, r) => spans.push(Span::styled(format!("-{r}"), removed_style)),
        (a, r) => {
            spans.push(Span::styled(format!("-{r}"), removed_style));
            spans.push(Span::styled(" ".to_string(), base));
            spans.push(Span::styled(format!("+{a}"), added_style));
        }
    }
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
    let bg = picker_row_bg(highlighted);
    // The message itself is colored by severity (matching the squiggle/popup); fuzzy matches stay
    // the bright accent so they remain visible. The range trails in gray parentheses.
    let base = Style::default().fg(diag_color(severity)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
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

/// One references-picker row, matching the native client: the referenced line's preview on the
/// left (`match_indices` highlighted, the same fuzzy-match tinting the grep/diagnostics rows use),
/// then a dim `path:line` location right-aligned at the row's edge (path segment-elided when long,
/// so the filename + line survive). Leading indentation is stripped (noise in a flat list), and an
/// overflowing preview is cut with a dim `…` so the location always stays visible — mirroring the
/// grep row's layout, just with the cross-file path alongside the line number.
fn reference_item_spans(
    display_path: &str,
    line: u32,
    preview: &str,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    let dim_style = base.fg(picker_dim_fg(highlighted));
    let gap = 2; // minimum gap between the preview and the location

    // Right-aligned `path:line`, capped to half the row so a deep path can't crowd out the code.
    let line_part = format!(":{}", line + 1);
    let loc_budget = (max_width / 2).max(line_part.width());
    let path_budget = loc_budget.saturating_sub(line_part.width());
    let (path_shown, _) = truncate_path_with_indices(display_path, &[], path_budget);
    let loc = format!("{path_shown}{line_part}");

    // Preview on the left, leading indentation stripped; shift match indices down by the stripped
    // char count (indices inside the stripped whitespace drop out).
    let trimmed = preview.trim_start();
    let lead_chars = (preview.chars().count() - trimmed.chars().count()) as u32;
    let shifted: Vec<u32> = match_indices
        .iter()
        .filter_map(|i| i.checked_sub(lead_chars))
        .collect();

    let preview_budget = max_width.saturating_sub(gap + loc.width());
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
    // Pad out to the right edge (≥ the gap by construction) so the location aligns down the list.
    let used = shown.width() + usize::from(ellipsis) + loc.width();
    spans.push(Span::styled(
        " ".repeat(max_width.saturating_sub(used)),
        base,
    ));
    spans.push(Span::styled(loc, dim_style));
    spans
}

/// The display fields of one document symbol, borrowed from [`PickerItem::Symbol`].
struct SymbolRow<'a> {
    name: &'a str,
    kind: aether_protocol::picker::SymbolKind,
    detail: &'a str,
    depth: u32,
    /// An ancestor shown only for tree context while filtering — rendered dim, non-selectable.
    context: bool,
}

/// One document-symbol picker row: an indent for nesting depth, the symbol name (fuzzy-match
/// highlighted) with its dim `detail` (signature) beside it, and the kind tag
/// right-aligned at the row's right edge — mirroring the rich clients' alignment.
fn symbol_item_spans(
    sym: SymbolRow<'_>,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let SymbolRow {
        name,
        kind: symbol_kind,
        detail,
        depth,
        context,
    } = sym;
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    let dim = base.fg(picker_dim_fg(highlighted));
    // A `context` row is an ancestor shown only for tree context while filtering — render the whole
    // row (name included) dim, so it reads as a non-selectable header above its matched descendants.
    let base = if context { dim } else { base };

    // Layout (matches the rich clients): indent (nesting), the name, then the dim `detail`
    // (signature) beside it, and the kind tag right-aligned at the row's right edge.
    // The kind tag + a gap are reserved up front; the name takes priority over `detail` for the
    // remaining left space.
    let trunc = |s: &str, budget: usize| -> String {
        s.chars()
            .scan(0usize, |w, c| {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if *w + cw > budget {
                    None
                } else {
                    *w += cw;
                    Some(c)
                }
            })
            .collect()
    };

    let indent = "  ".repeat(depth as usize);
    let indent_w = indent.width();
    let kind = symbol_kind.label();
    let kind_w = kind.width();
    const GAP: usize = 2; // minimum gap between the left content and the right-aligned kind
    let left_budget = max_width.saturating_sub(kind_w + GAP);

    let mut spans: Vec<Span<'static>> = Vec::new();
    if !indent.is_empty() {
        spans.push(Span::styled(indent.clone(), dim));
    }

    // Name (highlighted), truncated to the left budget; drop match indices past the cut.
    let name_shown = trunc(name, left_budget.saturating_sub(indent_w));
    let name_w = name_shown.width();
    let kept_char_count = name_shown.chars().count() as u32;
    let kept_indices: Vec<u32> = match_indices
        .iter()
        .copied()
        .filter(|&i| i < kept_char_count)
        .collect();
    push_styled_with_match_indices(&mut spans, &name_shown, &kept_indices, base, match_style);

    // Dim `detail` beside the name (the "parent"/signature), with whatever left space remains.
    let mut detail_w = 0;
    let detail_budget = left_budget.saturating_sub(indent_w + name_w + 1);
    if !detail.is_empty() && detail_budget > 0 {
        let dshown = trunc(detail, detail_budget);
        if !dshown.is_empty() {
            detail_w = 1 + dshown.width();
            spans.push(Span::styled(format!(" {dshown}"), dim));
        }
    }

    // Pad out to right-align the kind tag (≥ GAP by construction).
    let left_used = indent_w + name_w + detail_w;
    spans.push(Span::styled(
        " ".repeat(max_width.saturating_sub(left_used + kind_w)),
        base,
    ));
    spans.push(Span::styled(kind.to_string(), dim));
    spans
}

/// The display fields of one keyboard shortcut, borrowed from [`PickerItem::Keybinding`].
/// No `group`: it renders as the section header above the run (the server-pushed
/// [`GroupSpan`]s), not on the row itself.
struct KeybindingRow<'a> {
    desc: &'a str,
    mode: &'a str,
    keys: &'a str,
}

/// One Keybindings picker row: the description on the left (the group is the section header
/// above the run, not row text), a dim `(mode)` for Insert/Search rows (default modes are
/// elided, matching the haystack), and the key chord right-aligned at the row's edge in frost
/// blue (matching the native client). `match_indices` are char offsets into the composed
/// haystack (`{desc} [({mode}) ]{keys}`); `keybinding_match_segments` rebases them onto each
/// rendered segment, so highlights land on segment text and never on the separators.
fn keybinding_item_spans(
    row: KeybindingRow<'_>,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let KeybindingRow { desc, mode, keys } = row;
    let bg = picker_row_bg(highlighted);
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    let dim = base.fg(picker_dim_fg(highlighted));

    let seg = aether_client::picker::keybinding_match_segments(desc, mode, keys, match_indices);
    let shows_mode = aether_protocol::picker::KeybindingEntry::shows_mode(mode);

    let mut spans: Vec<Span<'static>> = Vec::new();
    push_styled_with_match_indices(&mut spans, desc, &seg.desc, base, match_style);
    let mut used = desc.width() + keys.width();
    if shows_mode {
        spans.push(Span::styled(" (".to_string(), dim));
        push_styled_with_match_indices(&mut spans, mode, &seg.mode, dim, match_style);
        spans.push(Span::styled(")".to_string(), dim));
        used += 3 + mode.width();
    }

    // Pad out to right-align the chord. The pad carries the row background, like the text.
    spans.push(Span::styled(
        " ".repeat(max_width.saturating_sub(used)),
        base,
    ));
    push_styled_with_match_indices(
        &mut spans,
        keys,
        &seg.keys,
        base.fg(c(th().accent)),
        match_style,
    );
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
    let bg = picker_row_bg(highlighted);
    // A ready server with active `$/progress` work shows the busy colour (same as the status bar).
    let busy = matches!(status, LspStatus::Ready) && !progress.is_empty();
    let dot_color = if busy {
        c(th().warning)
    } else {
        lsp_status_color(status)
    };
    let base = Style::default().fg(c(th().fg)).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
    // Dim tail: `language · root`, the root only when the server isn't at the workspace root
    // (empty `root_label` → omitted, so single-root workspaces show just the language).
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
        spans.push(Span::styled(
            hint,
            Style::default().fg(c(th().warning)).bg(bg),
        ));
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

/// One Explorer entry row: leaf name with a trailing `/` for directories, the accent
/// for directories, fuzzy-match highlights overlaid the same way the Files picker does. The
/// `/` suffix is appended *after* the name proper so `match_indices` (which index into the
/// name) don't have to know about it.
/// Status-bullet colour for a Git status: green for new, yellow for modified, red for
/// removed/conflict. `None` for ignored (and clean) entries — they carry no bullet (ignored is
/// dimmed via its text colour instead).
fn git_status_bullet_color(s: GitStatus) -> Option<Color> {
    th().git_status_bullet(s).map(c)
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
    let bg = picker_row_bg(highlighted);
    // Leading status indicator: a coloured `•` for a changed entry, a blank cell otherwise so every
    // row's text stays column-aligned. Two cols wide (bullet + space).
    let bullet_span = git_status_bullet_span(git_status, bg);
    // Text colour keeps the frost-blue dir / snow-white file scheme; ignored entries dim to a
    // lighter gray (legible on both the normal and selected backgrounds).
    let fg = match git_status {
        Some(GitStatus::Ignored) => c(th().fg_dim),
        _ if is_dir => c(th().accent),
        _ => c(th().fg),
    };
    let base = Style::default().fg(fg).bg(bg);
    let match_style = base
        .fg(c(th().match_highlight))
        .add_modifier(Modifier::BOLD);
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

/// Paint the markdown reading view (docs/markdown-view.md §2.8): the core's laid-out rows at the
/// shell's scroll, the content column centered to the reading measure, the focused element
/// row-tinted (a focused link inverts its own span instead).
fn draw_read_view(f: &mut Frame, state: &AppState, area: Rect) {
    use aether_client::read_layout::SpanKind;
    let Some(rv) = state.read.as_ref() else {
        return;
    };
    let (content_cols, margin) = read_measure(area.width);
    // The rect hosts the gutter *plus* the text measure: the layout wraps rows to
    // `content_cols` alone, so a full line just reaches the right edge instead of losing its
    // last cells to the Paragraph clip.
    let content = Rect {
        x: area.x.saturating_add(margin),
        y: area.y,
        width: (content_cols + READ_GUTTER).min(area.width),
        height: area.height,
    };
    // Fill the whole area (margins included) with the editor's base background, so the reading
    // view sits on the same canvas as the buffer.
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(c(th().bg))),
        area,
    );
    if rv.rows.is_empty() {
        if rv.loading {
            let msg = Paragraph::new(Line::from(Span::styled(
                "Loading…",
                Style::default().fg(c(th().fg_faint)).bg(c(th().bg)),
            )));
            f.render_widget(msg, content);
        }
        return;
    }
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    // The scroll runs over *padded* rows: READ_PAD_TOP blank rows above the document and
    // READ_PAD_BOTTOM below, so the text doesn't sit flush against the chrome.
    for screen_row in 0..area.height {
        let padded = rv.scroll as usize + screen_row as usize;
        let Some((idx, row)) = padded
            .checked_sub(READ_PAD_TOP as usize)
            .and_then(|i| rv.rows.get(i).map(|r| (i, r)))
        else {
            lines.push(Line::default());
            continue;
        };
        // The reading position (block grain) is a frost bar in a 2-column gutter (settled on
        // after trying background tints and full-width bands); every row carries the prefix so
        // nothing shifts as focus moves. Always present — even when the cursor sits inside a
        // link, the link's containing block keeps its bar (two projections of one cursor).
        // `bar_rows` is the focused block's whole subtree (nested items included).
        let row_focused = rv.bar_rows.is_some_and(|(a, b)| idx >= a && idx <= b);
        // An extended selection tints its blocks' rows with the editor's selection shade
        // (docs/markdown-view.md §12): page-background cells swap to the selection shade at push time
        // (`finish_row`); spans with their own background — code panels, chips — keep it,
        // exactly like the table band. Blank separator rows inside the range stay on the page
        // background (only their gutter stub would tint) — per-block bands with clean gaps,
        // matching the GUI/web per-block tint.
        let row_selected =
            rv.sel_rows.is_some_and(|(a, b)| idx >= a && idx <= b) && !row.spans.is_empty();
        let finish_row = move |spans: Vec<Span<'static>>| -> Line<'static> {
            if !row_selected {
                return Line::from(spans);
            }
            Line::from(
                spans
                    .into_iter()
                    .map(|mut s| {
                        if s.style.bg.is_none() || s.style.bg == Some(c(th().bg)) {
                            s.style = s.style.bg(c(th().bg_selection));
                        }
                        s
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let mut spans: Vec<Span> = vec![if row_focused {
            Span::styled("▎ ", Style::default().fg(c(th().accent)).bg(c(th().bg)))
        } else {
            Span::styled("  ", Style::default().bg(c(th().bg)))
        }];
        // The language-tag row renders as a small "tab": panel background only under
        // ` json ` (the tag plus a space either side), the rest of the row on the page
        // background — so the panel proper opens on the row below, with the tag perched on
        // top. Any leading prefix spans (a quote bar) stay as-is.
        if let Some(t) = row
            .spans
            .iter()
            .position(|s| matches!(s.style.kind, SpanKind::CodeFrame))
        {
            for rs in &row.spans[..t] {
                spans.push(Span::styled(rs.text.clone(), read_span_style(rs.style)));
            }
            let tag = &row.spans[t];
            spans.push(Span::styled(
                format!("{} ", tag.text),
                read_span_style(tag.style),
            ));
            lines.push(finish_row(spans));
            continue;
        }
        // Code rows are unchunked — they may exceed the measure — and render through a
        // horizontal window: pad/indicator column, clipped runs, background fill to the
        // measure, pad/indicator column. Leading non-code spans (a quote bar, a list indent)
        // stay put; only the code part pans. `…` at either edge marks clipped content.
        let code_at = row
            .spans
            .iter()
            .position(|s| matches!(s.style.kind, SpanKind::CodeBlock));
        if let Some(split) = code_at {
            let (prefix, code_part) = row.spans.split_at(split);
            let mut used = 2usize;
            for rs in prefix {
                used += rs.text.width();
                spans.push(Span::styled(rs.text.clone(), read_span_style(rs.style)));
            }
            let off = row
                .element
                .and_then(|e| rv.hscroll.get(&e))
                .copied()
                .unwrap_or(0) as usize;
            let window = (content.width as usize).saturating_sub(used + 2).max(1);
            let total: usize = code_part.iter().map(|s| s.text.width()).sum();
            let indicator = Style::default().fg(c(th().fg_dim)).bg(c(th().md_code_bg));
            spans.push(Span::styled(
                if off > 0 && total > 0 { "…" } else { " " },
                indicator,
            ));
            let clipped = aether_client::read_layout::clip_spans(code_part, off, window);
            let mut shown = 0usize;
            for rs in &clipped {
                let style = match &rs.syntax {
                    // A fenced-code token: the editor's own tree-sitter theme, on the panel.
                    Some(kind) => theme_for(kind).bg(c(th().md_code_bg)),
                    None => read_span_style(rs.style),
                };
                shown += rs.text.width();
                spans.push(Span::styled(rs.text.clone(), style));
            }
            // The block's bottom pad row hosts a horizontal scrollbar when the block
            // overflows — the horizontal twin of the page bar (`─` track, bolder `━` thumb),
            // sized/positioned by the same shared thumb math.
            let is_block_end = rv.rows.get(idx + 1).map(|r| r.element) != Some(row.element);
            let bar = (total == 0 && is_block_end)
                .then(|| {
                    // Only code rows count toward overflow — the pinned tag row is chrome
                    // (counting it once made every tagged panel "overflow").
                    let widest = row
                        .element
                        .map(|e| aether_client::read_layout::hscroll_content_width(&rv.rows, e))
                        .unwrap_or(0);
                    aether_client::scrollbar::thumb(
                        window as f64,
                        widest as f64,
                        window as f64,
                        off as f64,
                        1.0,
                    )
                })
                .flatten();
            if let Some((thumb_x, thumb_w)) = bar {
                let tx = (thumb_x.round() as usize).min(window.saturating_sub(1));
                let tw = (thumb_w.round() as usize).max(1).min(window - tx);
                let track = Style::default()
                    .fg(c(th().bg_selection))
                    .bg(c(th().md_code_bg));
                let thumb = Style::default().fg(c(th().fg_dim)).bg(c(th().md_code_bg));
                spans.push(Span::styled("─".repeat(tx), track));
                spans.push(Span::styled("━".repeat(tw), thumb));
                spans.push(Span::styled("─".repeat(window - tx - tw), track));
            } else {
                let fill = window.saturating_sub(shown);
                if fill > 0 {
                    spans.push(Span::styled(
                        " ".repeat(fill),
                        Style::default().bg(c(th().md_code_bg)),
                    ));
                }
            }
            spans.push(Span::styled(
                if total > off + window { "…" } else { " " },
                indicator,
            ));
            lines.push(finish_row(spans));
            continue;
        }
        // Table rows are unchunked too — natural column widths may exceed the measure — and
        // pan through the same horizontal window, but on the page background (no panel
        // fill). Instead of a chrome row, the bottom border doubles as the scroll track:
        // the thumb range of `└──┴──┘` brightens and bolds (`─` → `━`).
        let table_at = row
            .spans
            .iter()
            .position(|s| matches!(s.style.kind, SpanKind::TableBorder));
        if let Some(split) = table_at {
            let (prefix, table_part) = row.spans.split_at(split);
            let mut used = 2usize;
            for rs in prefix {
                used += rs.text.width();
                spans.push(Span::styled(rs.text.clone(), read_span_style(rs.style)));
            }
            let off = row
                .element
                .and_then(|e| rv.hscroll.get(&e))
                .copied()
                .unwrap_or(0) as usize;
            let total: usize = table_part.iter().map(|s| s.text.width()).sum();
            // A table that fits sits flush with the prose — no pad columns, so its frame lines
            // up with the text column beside it. Only an overflowing table spends a column
            // either side on the `…` pan indicators, narrowing its window to match (which is
            // the window `read_hscroll_by` clamps against — a fitting table never pans).
            let avail = (content.width as usize).saturating_sub(used).max(1);
            let overflows = total > avail;
            let window = if overflows {
                avail.saturating_sub(2).max(1)
            } else {
                avail
            };
            let indicator = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));
            if overflows {
                spans.push(Span::styled(if off > 0 { "…" } else { " " }, indicator));
            }
            let clipped = aether_client::read_layout::clip_spans(table_part, off, window);
            let is_block_end = rv.rows.get(idx + 1).map(|r| r.element) != Some(row.element);
            let is_bottom_border = is_block_end
                && clipped.len() == 1
                && table_part.len() == 1
                && table_part[0].text.starts_with('└');
            let bar = is_bottom_border
                .then(|| {
                    let widest = row
                        .element
                        .map(|e| aether_client::read_layout::hscroll_content_width(&rv.rows, e))
                        .unwrap_or(0);
                    aether_client::scrollbar::thumb(
                        window as f64,
                        widest as f64,
                        window as f64,
                        off as f64,
                        1.0,
                    )
                })
                .flatten();
            if let Some((thumb_x, thumb_w)) = bar {
                let tx = (thumb_x.round() as usize).min(window.saturating_sub(1));
                let tw = (thumb_w.round() as usize).max(1).min(window - tx);
                let chars: Vec<char> = clipped[0].text.chars().collect();
                let seg = |r: std::ops::Range<usize>| -> String {
                    chars[r.start.min(chars.len())..r.end.min(chars.len())]
                        .iter()
                        .collect()
                };
                let base = read_span_style(clipped[0].style);
                let thumb = Style::default().fg(c(th().fg_dim)).bg(c(th().bg));
                spans.push(Span::styled(seg(0..tx), base));
                spans.push(Span::styled(seg(tx..tx + tw).replace('─', "━"), thumb));
                spans.push(Span::styled(seg(tx + tw..chars.len()), base));
            } else {
                // A banded row (header panel / body stripe) paints its background across the
                // row's *interior* — cell text, padding and the column dividers, but not the
                // two frame bars that close the row (`TableBorder`), so the box keeps its own
                // edges. Spans with a background of their own (inline-code chips) keep it, so a
                // chip still reads as a chip on top of the band.
                let band = read_table_band(&row.spans);
                for rs in &clipped {
                    let mut style = read_span_style(rs.style);
                    if let Some(bg) = band {
                        let frame = matches!(rs.style.kind, SpanKind::TableBorder);
                        if style.bg.is_none() && !frame {
                            style = style.bg(bg);
                        }
                    }
                    if rs.element.is_some() && rs.element == rv.target_focus {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    spans.push(Span::styled(rs.text.clone(), style));
                }
            }
            let shown: usize = clipped.iter().map(|s| s.text.width()).sum();
            let fill = window.saturating_sub(shown);
            if fill > 0 {
                spans.push(Span::styled(
                    " ".repeat(fill),
                    Style::default().bg(c(th().bg)),
                ));
            }
            if overflows {
                spans.push(Span::styled(
                    if total > off + window { "…" } else { " " },
                    indicator,
                ));
            }
            lines.push(finish_row(spans));
            continue;
        }
        for rs in &row.spans {
            let mut style = read_span_style(rs.style);
            // The Enter target (the interactive span the cursor sits inside) inverts on top.
            if rs.element.is_some() && rs.element == rv.target_focus {
                style = style.add_modifier(Modifier::REVERSED);
            }
            spans.push(Span::styled(rs.text.clone(), style));
        }
        lines.push(finish_row(spans));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(c(th().bg)).fg(c(th().fg))),
        content,
    );
    // The same scrollbar the editor pane draws, in the area's rightmost column (over the
    // padded height, so the thumb geometry matches what scrolling actually covers).
    let total = rv.rows.len() as u64 + u64::from(READ_PAD_TOP) + u64::from(READ_PAD_BOTTOM);
    if total > u64::from(area.height) {
        let track = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };
        render_scrollbar(f, track, rv.scroll as u64, total, area.height as u64);
    }
}

/// Blank rows padding the reading view above and below the document.
pub const READ_PAD_TOP: u16 = 2;
pub const READ_PAD_BOTTOM: u16 = 2;

/// The reading-position gutter every painted line leads with (`"▎ "` / `"  "`).
pub const READ_GUTTER: u16 = 2;

/// Text measure + centering margin for a terminal `term_cols` wide. The measure is taken over
/// the columns left of the gutter and the margin centers gutter+text as one block, so a
/// full-measure line plus the gutter exactly fills the painted rect — laying out at the raw
/// terminal width instead used to clip the last [`READ_GUTTER`] cells of every full line.
pub fn read_measure(term_cols: u16) -> (u16, u16) {
    aether_client::read_layout::measure(term_cols.saturating_sub(READ_GUTTER).max(10))
}

/// Map a core reading-view [`aether_client::read_layout::SpanStyle`] to the terminal theme.
/// The background band a table row paints under its interior, if any: the core marks a striped
/// body row's cell padding [`SpanKind::TableStripe`], and the colour lives on that kind in
/// [`read_span_style`]. Header and border rows carry no band — the header reads as the header
/// through weight and colour, and the frame stays on the page background.
fn read_table_band(spans: &[aether_client::read_layout::ReadSpan]) -> Option<Color> {
    use aether_client::read_layout::SpanKind as K;
    spans
        .iter()
        .find(|s| matches!(s.style.kind, K::TableStripe))
        .and_then(|s| read_span_style(s.style).bg)
}

fn read_span_style(s: aether_client::read_layout::SpanStyle) -> Style {
    use aether_client::markdown::AlertKind;
    use aether_client::read_layout::SpanKind as K;
    let t = th();
    let alert_color = |k: AlertKind| match k {
        AlertKind::Note => t.info,
        AlertKind::Tip => t.ok,
        AlertKind::Important => t.md_alert_important,
        AlertKind::Warning => t.warning,
        AlertKind::Caution => t.error,
    };
    let mut st = match s.kind {
        K::Text => Style::default().fg(c(t.fg)),
        // The heading colour ladder (shared with web/iced): the title/function hue for the
        // majors, the type hue (teal in dark) for H3, bright for H4, and body-grey for H5/H6 —
        // bold (from the span style) still sets the minor levels off from prose.
        K::Heading(1 | 2) => Style::default().fg(c(t.syn_function)),
        K::Heading(3) => Style::default().fg(c(t.syn_type)),
        K::Heading(4) => Style::default().fg(c(t.fg_bright)),
        K::Heading(_) => Style::default().fg(c(t.fg)),
        // Inline code: body-coloured text on the panel shade (matches the web chip).
        K::Code => Style::default().fg(c(t.fg)).bg(c(t.md_code_bg)),
        K::CodeBlock => Style::default().fg(c(t.fg)).bg(c(t.md_code_bg)),
        // The pinned language tag on the panel: the overlay-border grey, over the panel
        // background so the pad row reads as one solid strip.
        K::CodeFrame => Style::default().fg(c(t.overlay_border)).bg(c(t.md_code_bg)),
        K::Rule | K::TableBorder | K::TableDivider | K::Dim => Style::default().fg(c(t.fg_faint)),
        // A completed task item reads as done without becoming chrome: the muted shade —
        // still legible prose rather than `Dim`'s border grey.
        K::TaskDone => Style::default().fg(c(t.fg_muted)),
        K::Link => Style::default().fg(c(t.accent_alt)),
        // Bullets/numbers/checkboxes read as body text (uniform with the web/native markers).
        K::Marker => Style::default().fg(c(t.fg)),
        K::QuoteBar(None) => Style::default().fg(c(t.fg_faint)),
        K::QuoteBar(Some(k)) => Style::default().fg(c(alert_color(k))),
        K::AlertLabel(k) => Style::default().fg(c(alert_color(k))),
        // The header sets itself off with weight and colour alone (bold-bright over the body's
        // grey, plus the separator rule) — no band. Only the body stripe carries a background,
        // which the painter lifts onto the whole row interior (`read_table_band`).
        K::TableHead => Style::default().fg(c(t.fg_bright)),
        K::TableStripe => Style::default().fg(c(t.fg)).bg(c(t.md_table_stripe_bg)),
    };
    if s.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.underline {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
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
        && state.ed().blame.line == Some(cursor_line)
    {
        state.ed().blame.text.clone()
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
            let mut spans =
                deleted_virtual_row_spans(&vrow.text, viewport_cols, vrow.stage, &vrow.emphasis);
            // Deletion bar in the git gutter column: bright red unstaged, dimmed red staged.
            spans.insert(
                0,
                gutter_bar(stage_color(
                    vrow.stage,
                    c(th().git_deleted),
                    c(th().git_staged_deleted),
                )),
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
                            Style::default().bg(c(th().bg_visual)).fg(c(th().fg_faint))
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
            let emphasis_on_row =
                emphasis_on_visual_row(vrow.byte_offset, row_text_len, &render.diff_emphasis);
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
            // Intra-line diff emphasis, horizontally scroll-adjusted like the brackets (no-op
            // under soft wrap where scroll_col is 0).
            let clipped_emphasis: Vec<(u32, u32)> = emphasis_on_row
                .iter()
                .filter(|&&(_, e)| e > scroll_col)
                .map(|&(s, e)| (s.saturating_sub(scroll_col), e - scroll_col))
                .collect();
            // Sneak targets, row-relative then horizontally scroll-adjusted (no-op when scroll_col
            // is 0). The label is dropped if its cell scrolled out of view.
            let clipped_sneak: Vec<(u32, u32, u32, Option<char>)> =
                sneak_targets_on_visual_row(vrow.byte_offset, row_text_len, &render.sneak_targets)
                    .into_iter()
                    .filter_map(|(s, e, pe, label)| {
                        if e <= scroll_col {
                            return None;
                        }
                        let label = if s >= scroll_col { label } else { None };
                        Some((
                            s.saturating_sub(scroll_col),
                            e - scroll_col,
                            pe.saturating_sub(scroll_col),
                            label,
                        ))
                    })
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
                    Style::default().fg(c(th().fg_faint)),
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
                &clipped_emphasis,
                stage_color(
                    render.diff_stage,
                    c(th().git_modified_emph_bg),
                    c(th().git_staged_modified_emph_bg),
                ),
                &clipped_brackets,
                &clipped_diags,
                &clipped_sneak,
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
                    Style::default().bg(c(th().bg_visual)).fg(c(th().fg_faint))
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
        Paragraph::new(lines).style(Style::default().bg(c(th().bg)).fg(c(th().fg))),
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
/// fill that spans the content width so the deletion reads as a distinct band; the intra-line
/// `emphasis` ranges (the parts the paired buffer line actually replaced) sit on a stronger red
/// fill. Tabs expand to spaces for stable width; content wider than the viewport is clipped. The
/// gutter cell is added separately by [`prepend_gutter`].
fn deleted_virtual_row_spans(
    text: &str,
    width: u16,
    stage: DiffStage,
    emphasis: &[EmphasisRange],
) -> Vec<Span<'static>> {
    let (fg, bg, emph_bg) = if stage == DiffStage::Staged {
        (
            c(th().git_staged_deleted),
            c(th().git_staged_deleted_bg),
            c(th().git_staged_deleted_emph_bg),
        )
    } else {
        (
            c(th().git_deleted),
            c(th().git_deleted_bg),
            c(th().git_deleted_emph_bg),
        )
    };
    let in_emphasis = |byte: usize| {
        emphasis
            .iter()
            .any(|r| (r.start as usize) <= byte && byte < r.end as usize)
    };
    // Walk chars (expanding tabs) and group into runs by emphasis, tracking the *original* byte
    // position so the ranges keep meaning after expansion.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_emph = false;
    let mut used = 0usize;
    // Emphasized runs get the vivid fill with the *normal* foreground: the red-on-red of the
    // base row is too low-contrast on the stronger fill, and the fg switch is itself part of
    // the signal (matching the web's `.deleted-phantom .diff-emph` rule).
    let flush = |spans: &mut Vec<Span<'static>>, run: &mut String, emph: bool| {
        if !run.is_empty() {
            let (fill, ink) = if emph { (emph_bg, c(th().fg)) } else { (bg, fg) };
            spans.push(Span::styled(
                std::mem::take(run),
                Style::default().fg(ink).bg(fill),
            ));
        }
    };
    for (i, ch) in text.char_indices() {
        if used >= width as usize {
            break;
        }
        let emph = in_emphasis(i);
        if emph != run_emph {
            flush(&mut spans, &mut run, run_emph);
            run_emph = emph;
        }
        if ch == '\t' {
            let n = (TAB_WIDTH as usize).min(width as usize - used);
            run.push_str(&" ".repeat(n));
            used += n;
        } else {
            run.push(ch);
            used += 1;
        }
    }
    flush(&mut spans, &mut run, run_emph);
    // Pad to the content width so the band reaches the right edge (in the base fill).
    if used < width as usize {
        spans.push(Span::styled(
            " ".repeat(width as usize - used),
            Style::default().fg(fg).bg(bg),
        ));
    }
    spans
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
        Some(DiffMarker::Added) => gutter_bar(stage_color(
            stage,
            c(th().git_added),
            c(th().git_staged_added),
        )),
        Some(DiffMarker::Modified) => gutter_bar(stage_color(
            stage,
            c(th().git_modified),
            c(th().git_staged_modified),
        )),
        Some(DiffMarker::Deleted) => {
            // "removed above" top marker
            Span::styled(
                "▔".to_string(),
                Style::default().fg(stage_color(
                    stage,
                    c(th().git_deleted),
                    c(th().git_staged_deleted),
                )),
            )
        }
        None => Span::styled(" ".to_string(), Style::default().fg(c(th().bg))), // unchanged → blank
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
        (DiffMarker::Added, DiffStage::Staged) => Some(c(th().git_staged_added_bg)),
        (DiffMarker::Modified, DiffStage::Staged) => Some(c(th().git_staged_modified_bg)),
        (DiffMarker::Added, _) => Some(c(th().git_added_bg)),
        (DiffMarker::Modified, _) => Some(c(th().git_modified_bg)),
    }
}

/// Background tint for the cursor's current line. On an added/modified line (diff view on) it's a
/// green/olive cursorline variant so the line still reads as changed — dimmed further when the
/// change is staged, matching the tint scheme; otherwise the plain cursorline.
fn cursor_line_bg(diff_marker: Option<DiffMarker>, stage: DiffStage) -> Color {
    match (diff_marker, stage) {
        (Some(DiffMarker::Added), DiffStage::Staged) => c(th().cursor_line_staged_added_bg),
        (Some(DiffMarker::Modified), DiffStage::Staged) => c(th().cursor_line_staged_modified_bg),
        (Some(DiffMarker::Added), _) => c(th().cursor_line_added_bg),
        (Some(DiffMarker::Modified), _) => c(th().cursor_line_modified_bg),
        _ => c(th().cursor_line_bg),
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
            Style::default()
                .fg(c(th().fg_faint))
                .add_modifier(Modifier::ITALIC),
        ));
    }
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

/// Clip intra-line diff emphasis ranges to this visual row (row-relative), like
/// [`matches_on_visual_row`].
fn emphasis_on_visual_row(
    row_byte_offset: u32,
    row_text_len: u32,
    emphasis: &[EmphasisRange],
) -> Vec<(u32, u32)> {
    if row_text_len == 0 {
        return Vec::new();
    }
    let row_end = row_byte_offset + row_text_len;
    emphasis
        .iter()
        .filter_map(|r| {
            let s = r.start.max(row_byte_offset);
            let e = r.end.min(row_end);
            (s < e).then(|| (s - row_byte_offset, e - row_byte_offset))
        })
        .collect()
}

/// Clip sneak word-jump targets to this visual row, returning row-relative `(start, end, label)`.
/// The label rides on the entry only when the word's true start falls within the row (so a word
/// continuing from a previous wrapped row keeps its highlight but not a stray label).
fn sneak_targets_on_visual_row(
    row_byte_offset: u32,
    row_text_len: u32,
    targets: &[SneakTarget],
) -> Vec<(u32, u32, u32, Option<char>)> {
    if row_text_len == 0 {
        return Vec::new();
    }
    let row_end = row_byte_offset + row_text_len;
    targets
        .iter()
        .filter_map(|t| {
            let s = t.start.max(row_byte_offset);
            let e = t.end.min(row_end);
            if s < e {
                // `clamp` requires min <= max, so only compute it once the target is known to
                // intersect the row (otherwise `s > e`).
                let pe = t.prefix_end.clamp(s, e);
                // Keep the label only if the word actually starts in this row.
                let label = (t.start >= row_byte_offset).then_some(t.label).flatten();
                Some((
                    s - row_byte_offset,
                    e - row_byte_offset,
                    pe - row_byte_offset,
                    label,
                ))
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

/// The underline / message color for a diagnostic severity — the core's mapping
/// ([`Theme::diagnostic`]): the error/warning/info hues, with Hint on the plain foreground
/// (readable on the status/popover backgrounds and distinct from the coloured severities).
fn diag_color(severity: DiagnosticSeverity) -> Color {
    c(th().diagnostic(severity))
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
// One per-byte overlay channel per arg (highlights / selection / search / brackets / diagnostics /
// sneak); bundling them would only obscure the row-render call site.
#[allow(clippy::too_many_arguments)]
fn build_spans(
    text: &str,
    highlights: &[Highlight],
    sel: Option<(u32, u32)>,
    matches: &[(u32, u32)],
    emphasis: &[(u32, u32)],
    emphasis_bg: Color,
    match_brackets: &[u32],
    diagnostics: &[(u32, u32, DiagnosticSeverity)],
    sneak: &[(u32, u32, u32, Option<char>)],
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

    let mut byte_in_emphasis: Vec<bool> = vec![false; trunc_len];
    for (s, e) in emphasis {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        for in_emph in &mut byte_in_emphasis[s..e] {
            *in_emph = true;
        }
    }

    let mut byte_is_match_bracket: Vec<bool> = vec![false; trunc_len];
    for &b in match_brackets {
        let idx = (b as usize).min(trunc_len);
        if idx < trunc_len {
            byte_is_match_bracket[idx] = true;
        }
    }

    // Sneak word-jump overlays: a per-byte "candidate word" flag (subtle tint), a "typed-prefix
    // chip" flag (bright label colour over the chars typed so far), and the label char painted over
    // the chip's first cell.
    let mut byte_in_sneak: Vec<bool> = vec![false; trunc_len];
    let mut byte_sneak_chip: Vec<bool> = vec![false; trunc_len];
    let mut byte_sneak_label: Vec<Option<char>> = vec![None; trunc_len];
    for (s, e, pe, label) in sneak {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        let pe = (*pe as usize).min(trunc_len).max(s);
        for flag in &mut byte_in_sneak[s..e] {
            *flag = true;
        }
        for flag in &mut byte_sneak_chip[s..pe] {
            *flag = true;
        }
        if let Some(lbl) = label {
            if s < trunc_len {
                byte_sneak_label[s] = Some(*lbl);
            }
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
        // Match-bracket overlay: bold + the match-bracket hue (Aurora orange in dark) — the
        // only warm tone in the palette, so it reads as a distinct "this bracket pairs with the
        // cursor" signal without colliding with the accents used elsewhere. Painted before
        // search and selection so those (which use bg) still win when stacked.
        if byte_is_match_bracket[byte_idx] {
            style = style.fg(c(th().match_bracket)).add_modifier(Modifier::BOLD);
        }
        // Intra-line diff emphasis: the stronger change fill under everything that follows —
        // search, sneak, and selection all paint over it, mirroring how the whole-line tint
        // stacks (`apply_line_tint` skips spans that carry their own bg, so this survives it).
        // Comment-coloured text gets the same legibility lift as inside a search fill.
        if byte_in_emphasis[byte_idx] {
            style = style.bg(emphasis_bg);
            if style.fg == Some(c(th().fg_faint)) || style.fg == Some(c(th().syn_comment)) {
                style = style.fg(c(th().fg));
            }
        }
        // Search match: the quiet dim fill behind the normal syntax text — visible on the
        // current-line tint while still sitting clearly below the more saturated visual
        // selection, which paints over it.
        if byte_in_match[byte_idx] {
            style = style.bg(c(th().fill_dim));
            // Comments sit on the dimmest legible rung — only ~2:1 against the fill in dark —
            // so a match inside one would be barely legible (faint text would vanish outright).
            // Lift just that text to the normal foreground; every other syntax color reads fine.
            if style.fg == Some(c(th().fg_faint)) || style.fg == Some(c(th().syn_comment)) {
                style = style.fg(c(th().fg));
            }
        }
        // Sneak candidate word: the quiet dim fill marking the jump targets (the label cell itself
        // is painted separately, below, in the char loop).
        if byte_in_sneak[byte_idx] {
            style = style.bg(c(th().fill_dim));
            if style.fg == Some(c(th().fg_faint)) || style.fg == Some(c(th().syn_comment)) {
                style = style.fg(c(th().fg));
            }
        }
        // Typed prefix: a brighter, cooler band (the sneak-prefix role) than the word tint — between it and
        // the bright label cell in prominence. The label cell is painted separately in the char loop.
        if byte_sneak_chip[byte_idx] {
            style = style.bg(c(th().sneak_prefix_bg));
            if style.fg == Some(c(th().fg_faint))
                || style.fg == Some(c(th().fg_dim))
                || style.fg == Some(c(th().syn_comment))
            {
                style = style.fg(c(th().fg));
            }
        }
        if let Some((s, e)) = sel {
            if byte_idx >= s as usize && byte_idx < e as usize {
                style = style.bg(c(th().bg_visual));
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
    // indicator glyph (the faint shade) overlaid on the selection bg — `→` for tabs, `·` for trailing
    // spaces — so the user can see the structure of what they've selected.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;
    let mut display_col: u32 = 0;
    for (byte_idx, c) in truncated.char_indices() {
        // A sneak label is painted *over* the word's first cell: a bold, high-contrast glyph (dark
        // on Aurora yellow) that replaces the underlying character. The char it covers is the one
        // the user already typed, so nothing readable is lost.
        if let Some(lbl) = byte_sneak_label[byte_idx] {
            display_col += char_display_width(c, display_col);
            push_text(
                &mut spans,
                &mut current_text,
                &mut current_style,
                &lbl.to_string(),
                // `self::` because the loop variable `c` (the char) shadows the colour helper.
                Style::default()
                    .fg(self::c(th().fg_on_accent))
                    .bg(self::c(th().match_highlight))
                    .add_modifier(Modifier::BOLD),
            );
            continue;
        }
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
                    style.fg(self::c(th().fg_faint)),
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
                style.fg(self::c(th().fg_faint)),
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

/// Map a tree-sitter highlight name to a `Style`, via the core theme's semantic table
/// ([`Theme::syntax`]) — the dotted-prefix fallback (`function.macro` → `function`), the colour,
/// and the bold/italic/underline attributes all live there; this just converts to ratatui.
fn theme_for(kind: &str) -> Style {
    let Some(sx) = th().syntax(kind) else {
        return Style::default();
    };
    let mut st = Style::default();
    if let Some(color) = sx.color {
        st = st.fg(c(color));
    }
    if sx.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if sx.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if sx.underline {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
}

fn draw_status(f: &mut Frame, state: &AppState, area: Rect) {
    let line = if let Some(confirm) = state.confirm_prompt.as_ref() {
        // Confirm prompt always wins the status row — it can layer over save_prompt.
        Line::from(vec![Span::raw(format!(" {}? [y/N]", confirm.message))])
    } else if let Some(prompt) = state.save_prompt.as_ref() {
        // Save-prompt overlay: status row hosts the prompt regardless of underlying screen.
        Line::from(draw_save_prompt_spans(prompt, state, area.width as usize).0)
    } else if let Some(input) = state.open_path_prompt.as_ref() {
        // Open-from-path overlay: a single-line path input in the status row.
        Line::from(draw_open_path_prompt_spans(input, area.width as usize).0)
    } else if !state.has_editor() {
        // No editor: at boot while `Connecting` the row carries the connection indicator (the
        // same slot that shows "Reconnecting…" mid-session); otherwise just transient feedback.
        let (text, style) = if state.conn == ConnState::Connecting {
            (
                "Connecting...".to_string(),
                status_message_style(&crate::app::StatusMessage {
                    text: String::new(),
                    kind: crate::app::StatusKind::Warning,
                }),
            )
        } else {
            (
                state.status.text.clone(),
                status_message_style(&state.status),
            )
        };
        Line::from(vec![Span::raw(" "), Span::styled(text, style)])
    } else if matches!(state.ed().mode, EditorMode::Search) {
        // `/` (or `?`) prefix, then the active match-option chips — styled like the grep picker's
        // filter chips — then the typed query and the match count.
        let (mut spans, _) = search_prompt_lead(&state.ed().search);
        spans.push(Span::raw(state.ed().search.query.text.clone()));
        if let Some(count) = search_match_count_label(state) {
            spans.push(Span::raw(format!("    {count}")));
        }
        Line::from(spans)
    } else {
        // Workspace / file / dirty-dot / transient status sit on the left; counter (search and/or
        // grep, in that order) and cursor position sit on the right, with the counter to the
        // left of the position. When the row is narrow we truncate the right edge of the left
        // segment with `…` so the right segment stays whole and the position never gets
        // painted over.
        // Persisted workspace → `[name] ` chrome; an ephemeral "(no workspace)" context (or no
        // workspace yet) shows just the file label, no bracket.
        let workspace_prefix =
            if aether_client::labels::shows_workspace_chrome(&state.workspace_name) {
                format!("[{}] ", state.workspace_name)
            } else {
                String::new()
            };
        // Buffer-state dot just after the file label — colour-coded (unsaved / changed / deleted
        // on disk), matching the web client's favicon colours.
        let status_dot = state.buffer_status().map(|kind| {
            Span::styled(
                BUFFER_STATUS_DOT.to_string(),
                Style::default()
                    .bg(c(th().bg_panel))
                    .fg(buffer_status_color(kind)),
            )
        });

        // Left: the Git change counts sit next to the file label (they're about the file's VCS
        // state). Diagnostics moved to the right segment, by the position indicator.
        let git_spans = git_status_spans(state);

        // Right segment, left→right: search/grep counters, diagnostic counts, the position /
        // selection indicator, then the LSP glyph pinned to the far edge. A double space precedes
        // each group so they don't run together.
        let base = Style::default().bg(c(th().bg_panel)).fg(c(th().fg));
        let mut right_spans: Vec<Span<'static>> = Vec::new();
        let gap = |spans: &mut Vec<Span<'static>>| {
            if !spans.is_empty() {
                spans.push(Span::styled("  ".to_string(), base));
            }
        };
        let counter_parts: Vec<String> =
            [search_counter_label(state), jumplist_counter_label(state)]
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

        // Transient feedback floats as a toast now; the left status slot instead carries the
        // persistent connection indicator (empty while connected) — sitting after the file/git with
        // a 3-space gap (see `build_editor_status_spans`), coloured yellow/red by its kind.
        let conn_status = match state.conn {
            ConnState::Connected => crate::app::StatusMessage::default(),
            // Boot-time `Connecting` has no editor (so this status row isn't shown then); the
            // backdrop carries the indicator instead. Mapped here for exhaustiveness.
            ConnState::Connecting => crate::app::StatusMessage {
                text: "Connecting...".to_string(),
                kind: crate::app::StatusKind::Warning,
            },
            ConnState::Reconnecting { .. } => crate::app::StatusMessage {
                text: "Reconnecting...".to_string(),
                kind: crate::app::StatusKind::Warning,
            },
            ConnState::Failed => crate::app::StatusMessage {
                text: "Disconnected".to_string(),
                kind: crate::app::StatusKind::Error,
            },
        };
        Line::from(build_editor_status_spans(
            StatusLabel {
                workspace_prefix: &workspace_prefix,
                file_label: &state.ed().file_label,
                transient: state.ed().transient,
                tethered: state.ed().tethered,
            },
            status_dot,
            git_spans,
            &conn_status,
            right_spans,
            area.width as usize,
        ))
    };
    let p = Paragraph::new(line).style(Style::default().bg(c(th().bg_panel)).fg(c(th().fg)));
    f.render_widget(p, area);
}

/// Accent colour for a toast's left bar — matches the web/native toast border colours
/// (info → frost blue, success → green, warning → yellow, error → red).
fn toast_accent_color(kind: crate::app::StatusKind) -> Color {
    use crate::app::StatusKind;
    let t = th();
    c(match kind {
        StatusKind::Info => t.info,
        StatusKind::Success => t.ok,
        StatusKind::Warning => t.warning,
        StatusKind::Error => t.error,
    })
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
    let max_text = (area.width as usize).saturating_sub((BAR_W + PAD * 2 + MARGIN_X * 2) as usize);
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
        let tint = Style::default()
            .bg(c(th().bg_selection))
            .fg(c(th().fg_bright));
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

/// Build the save-prompt's status-row spans and the caret column offset (from the start of the
/// status area). A render mirror of [`chip_editor_spans`], differing only in its `" save as: "`
/// label and the path ghost including files:
///
/// - a `" save as: "` label;
/// - (multi-root) the **root** segment — when focused, the typed filter (red if it matches no
///   label) plus a gray ghost suffix; when unfocused, the chosen root's label in committed blue
///   (or the raw red filter if it matches nothing). Then a `:` separator once the path is in play;
/// - the **path** segment — the typed text (red if the parent dir failed to list) plus a gray
///   ghost suffix completing the highlighted entry (a trailing `/` only behind a directory).
///
/// Status-row spans for the open-from-path prompt (`Space Alt-w`): an ` open: ` prefix then the
/// typed path. Returns the spans plus the caret's column offset so the terminal cursor lands in
/// the input. A plain single-line field — no root chips / suggestions, unlike save-as.
fn draw_open_path_prompt_spans(
    input: &crate::text_input::TextInput,
    _total_width: usize,
) -> (Vec<Span<'static>>, u16) {
    const PREFIX: &str = " open: ";
    let prefix_style = Style::default().bg(c(th().bg_panel)).fg(c(th().accent));
    let base_style = Style::default().bg(c(th().bg_panel)).fg(c(th().fg));
    let spans = vec![
        Span::styled(PREFIX, prefix_style),
        Span::styled(input.text.clone(), base_style),
    ];
    let cursor_byte = input.cursor.min(input.text.len());
    let cursor_col = PREFIX.width() + input.text[..cursor_byte].width();
    (spans, cursor_col as u16)
}

fn draw_save_prompt_spans(
    prompt: &crate::save_prompt::SavePromptState,
    state: &AppState,
    _total_width: usize,
) -> (Vec<Span<'static>>, u16) {
    use crate::picker::ChipEditorField;
    let base_style = Style::default().bg(c(th().bg_panel)).fg(c(th().fg));
    // The chosen-root label / `:` separator share the explorer's committed-prefix blue.
    let prefix_style = Style::default().bg(c(th().bg_panel)).fg(c(th().accent));
    // Ghost / suggestion text (`ghost_text`). The faint shade won't do — it's only ~17
    // brightness off the panel and reads as invisible on the status bar; nor the `DIM` modifier
    // (some terminals ignore it for bright foregrounds). So the role is a mid-tone readable on
    // the panel yet plainly dimmer than the body text — off-palette in both modes.
    let ghost_style = Style::default().bg(c(th().bg_panel)).fg(c(th().ghost_text));
    // An invalid segment (root matching no label / path whose parent doesn't exist) renders red.
    let invalid_style = Style::default().bg(c(th().bg_panel)).fg(c(th().error));

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w: usize = 0;
    let mut cursor: usize = 0;
    let push = |spans: &mut Vec<Span<'static>>, w: &mut usize, text: String, style: Style| {
        *w += text.width();
        spans.push(Span::styled(text, style));
    };

    push(&mut spans, &mut w, " save as: ".into(), base_style);

    if prompt.multi_root {
        let labels = crate::labels::root_labels(&state.workspace_paths);
        let invalid = prompt.root_invalid(&labels);
        if prompt.field == ChipEditorField::Root {
            cursor = w + prompt.root_filter.width_to_cursor();
            let style = if invalid { invalid_style } else { base_style };
            push(&mut spans, &mut w, prompt.root_filter.text.clone(), style);
            // Ghost = the current match beyond the typed prefix; nothing matches → no ghost (the
            // red typed text is the cue).
            if let Some((_, suffix)) = prompt.root_ghost(&labels) {
                push(&mut spans, &mut w, suffix, ghost_style);
            }
        } else if invalid {
            // An unfocused-but-unmatched root shows the raw red filter — not the fallback label,
            // which would advertise a commit target the gate would refuse.
            push(
                &mut spans,
                &mut w,
                prompt.root_filter.text.clone(),
                invalid_style,
            );
        } else {
            push(
                &mut spans,
                &mut w,
                prompt.root_display(&labels),
                prefix_style,
            );
        }
        // The separator appears once the path is in play (focused, or already holding text).
        if prompt.field == ChipEditorField::Path || !prompt.input.text.is_empty() {
            push(&mut spans, &mut w, ": ".into(), prefix_style);
        }
    }

    // The path segment. Red only when its parent dir failed to list (a non-matching filename leaf
    // is fine). The caret tracks the path field unless the root field is focused.
    let path_style = if prompt.path_invalid() {
        invalid_style
    } else {
        base_style
    };
    if prompt.field == ChipEditorField::Path || !prompt.multi_root {
        cursor = w + prompt.input.width_to_cursor();
        push(&mut spans, &mut w, prompt.input.text.clone(), path_style);
        // Ghost suggestion: the rest of the current match (files included; `/` only behind a dir),
        // gray after the caret.
        if let Some(suffix) = prompt.path_ghost() {
            push(&mut spans, &mut w, suffix, ghost_style);
        }
    } else {
        push(&mut spans, &mut w, prompt.input.text.clone(), path_style);
    }
    (spans, cursor as u16)
}

/// Style for a `StatusMessage` based on its kind: success → blue (matches the committed-prefix
/// blue elsewhere in the UI), warning → yellow, error → red, info → default white. Background
/// stays the panel shade to blend with the surrounding status bar.
fn status_message_style(msg: &crate::app::StatusMessage) -> Style {
    use crate::app::StatusKind;
    let t = th();
    let fg = match msg.kind {
        StatusKind::Info => t.fg,
        StatusKind::Success => t.accent,
        StatusKind::Warning => t.warning,
        StatusKind::Error => t.error,
    };
    Style::default().bg(c(t.bg_panel)).fg(c(fg))
}

/// The status row's leading label: an optional `[workspace] ` prefix, the file label, whether
/// the buffer is transient (which italicises the label), and whether it's the session's tether
/// (which appends a dim ` *` — closing it exits the client, docs/tether.md).
struct StatusLabel<'a> {
    workspace_prefix: &'a str,
    file_label: &'a str,
    transient: bool,
    tethered: bool,
}

/// Build the spans for the default editor status row: an optional leading buffer-state dot, then
/// `left_pre` (workspace/file) in the base style, an optional colored status message after a `    `
/// separator, then padding pushing the right segment flush to the row edge. When the row is too
/// narrow:
/// - the status text truncates first (`…`), preserving the dot and workspace/file;
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
        workspace_prefix,
        file_label,
        transient,
        tethered,
    } = label;
    let base_style = Style::default().bg(c(th().bg_panel)).fg(c(th().fg));
    // A transient (preview) buffer slants the file label (root + path — not the workspace name)
    // instead of spending row width on an explicit marker. Terminals without italic support
    // just show it upright.
    let label_style = if transient {
        base_style.add_modifier(Modifier::ITALIC)
    } else {
        base_style
    };
    // The tether mark (docs/tether.md): a dim ` *` after the file label — upright even on a
    // slanted transient label (it's chrome, not part of the name). Dropped on narrow rows.
    let tether_mark = if tethered { " *" } else { "" };
    // The right segment (counters / diagnostics / position / LSP glyph) is pre-built by the caller,
    // already including its internal gaps and the glyph's edge padding.
    let right_w: usize = right_spans.iter().map(|s| s.content.width()).sum();
    // Always keep at least one cell of gap between the left content and the right segment.
    let left_max = total_width.saturating_sub(right_w + 1);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    // Buffer-state dot leads the row, before the workspace name — matching the terminal title and
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
    if workspace_prefix.width() + file_label.width() + tether_mark.width() >= pre_budget {
        // Even the workspace/file segment overflows. The file label is the informative part, so
        // it gets the budget first (segment elision keeps the filename end visible); the
        // workspace prefix is shown only if it still fits whole — a partially-cut `[pr…` is
        // noise. The rest (badges, status) is dropped.
        let (t, _) = truncate_path_with_indices(file_label, &[], pre_budget);
        let prefix = if workspace_prefix.width() + t.width() <= pre_budget {
            workspace_prefix.to_string()
        } else {
            String::new()
        };
        used += prefix.width() + t.width();
        spans.push(Span::styled(prefix, base_style));
        spans.push(Span::styled(t, label_style));
    } else {
        spans.push(Span::styled(workspace_prefix.to_string(), base_style));
        spans.push(Span::styled(file_label.to_string(), label_style));
        if !tether_mark.is_empty() {
            spans.push(Span::styled(
                tether_mark.to_string(),
                base_style.fg(c(th().fg_muted)),
            ));
        }
        used += workspace_prefix.width() + file_label.width() + tether_mark.width();
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
        // Status message (now the connection indicator) after a 3-space separator — matching the
        // file→git gap — truncated to whatever's left.
        if !status.is_empty() {
            let separator = "   ";
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
    let t = th();
    c(match kind {
        BufferStatusKind::ExternallyDeleted => t.state_deleted,
        BufferStatusKind::ExternallyModified => t.state_changed,
        BufferStatusKind::Unsaved => t.state_unsaved,
    })
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
    let bg = Style::default().bg(c(th().bg_panel));
    let meta = bg.fg(c(th().accent_alt)); // branch / base: the secondary accent, distinct from the body-text path
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
        (
            '+',
            c(th().git_added),
            status.unstaged.added,
            status.staged.added,
        ),
        (
            '~',
            c(th().git_modified),
            status.unstaged.modified,
            status.staged.modified,
        ),
        (
            '-',
            c(th().git_deleted),
            status.unstaged.deleted,
            status.staged.deleted,
        ),
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
    let bg = Style::default().bg(c(th().bg_panel));
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
        c(th().warning)
    } else {
        lsp_status_color(&status.status)
    };
    Some(Span::styled(
        "•".to_string(),
        Style::default().bg(c(th().bg_panel)).fg(color),
    ))
}

/// State colour for a language-server's status dot (`•`) — shared by the status bar, the LSP
/// picker rows, and the detail title. The transitional states read as "busy" (the loop is
/// event-driven, so the colour changes when a `lsp/status_changed` arrives rather than animating).
fn lsp_status_color(status: &LspStatus) -> Color {
    c(th().lsp_status(status))
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
    // `get` (not indexing): `pos.col` is a byte column into the *current* text, while these rows
    // are whatever the last viewport push rendered. An edit that moves the cursor to a different
    // line — a block move especially — adopts the new cursor from its own RPC response one round
    // trip before the matching rows arrive, so for that frame the column can land mid-char in the
    // stale line and slicing there would panic. Same +1 approximation as the off-window case.
    let char_bytes = row_text
        .get(row_local..)
        .and_then(|s| s.chars().next())
        .map_or(1, |c| c.len_utf8() as u32);
    LogicalPosition {
        line: pos.line,
        col: pos.col + char_bytes,
    }
}

fn place_terminal_cursor(f: &mut Frame, state: &AppState, buffer_area: Rect, status_area: Rect) {
    // Settings overlay takes precedence over every other cursor target. We only place the caret
    // when a text field is focused — the name field (index 0) or either input row; on a root or
    // project row the cursor is hidden (no `set_cursor_position` call → ratatui hides it for this
    // frame).
    if let Some(settings) = state.workspace_settings.as_ref() {
        use crate::app::SettingsRowView;
        if settings.selected == 0 {
            place_settings_name_cursor(f, settings, buffer_area);
        } else if matches!(
            settings.focused(),
            Some(SettingsRowView::AddRoot | SettingsRowView::AddProject)
        ) {
            place_settings_input_cursor(f, settings, buffer_area);
        }
        return;
    }
    // The app-settings overlay is toggle-only (no text field): returning without placing a caret
    // hides the terminal cursor for this frame, so none blinks in the buffer behind it.
    if state.app_settings.is_some() {
        return;
    }
    let ed = state.ed();
    if matches!(ed.mode, EditorMode::Search)
        && ed.search.chip_selected.is_none()
        && state.save_prompt.is_none()
        && !state.picker.open
    {
        // Park the terminal cursor on the status row, just past the prefix + option chips + the
        // typed query up to the input cursor (so Left/Right navigate within the query, not always
        // at the end).
        let (_, lead_w) = search_prompt_lead(&ed.search);
        let typed_w = ed.search.query.width_to_cursor() as u16;
        let col = status_area
            .x
            .saturating_add((lead_w + typed_w).min(status_area.width.saturating_sub(1)));
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
        // The span builder reports the caret offset of the focused segment (root or path), so the
        // terminal cursor lands in sync with the rendered text.
        let (_, cursor_off) = draw_save_prompt_spans(prompt, state, status_area.width as usize);
        let max_col = status_area
            .x
            .saturating_add(status_area.width.saturating_sub(1));
        let col = status_area.x.saturating_add(cursor_off).min(max_col);
        f.set_cursor_position((col, status_area.y));
        return;
    }
    if let Some(input) = state.open_path_prompt.as_ref() {
        let (_, cursor_off) = draw_open_path_prompt_spans(input, status_area.width as usize);
        let max_col = status_area
            .x
            .saturating_add(status_area.width.saturating_sub(1));
        let col = status_area.x.saturating_add(cursor_off).min(max_col);
        f.set_cursor_position((col, status_area.y));
        return;
    }
    // The reading view has no text caret — focus highlighting is the cursor's rendering; not
    // calling `set_cursor_position` leaves the terminal cursor hidden for this frame.
    if state.read.is_some() {
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

    /// Build a grep result window and its server-pushed group spans: `groups` is a list of
    /// (file, hit_count). Each file contributes one span (header row) plus `hit_count`
    /// selectable item rows.
    fn grep_items(groups: &[(&str, usize)]) -> (Vec<PickerItem>, Vec<GroupSpan>) {
        let mut items = Vec::new();
        let mut spans = Vec::new();
        for (fi, (path, n)) in groups.iter().enumerate() {
            spans.push(GroupSpan {
                start: items.len() as u32,
                header: GroupHeader::File {
                    path_index: fi as u32,
                    relative_path: (*path).to_string(),
                },
                count: None,
                expanded: None,
            });
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
        (items, spans)
    }

    /// The §9 group-run reveal (docs/picker-groups.md): frame the freshly-opened run — the
    /// minimal move that shows its last row, capped so the header never leaves the pane top.
    #[test]
    fn run_reveal_frames_the_expanded_run() {
        // Pane of 6 rows; run header at view row 8 with 3 items (rows 9..=11).
        // Below the pane and it fits: bottom-align its last row.
        assert_eq!(picker_scroll_for_run(4, 6, 8, 3), 6);
        // Already fully visible (either alignment): no movement.
        assert_eq!(picker_scroll_for_run(6, 6, 8, 3), 6);
        assert_eq!(picker_scroll_for_run(7, 6, 8, 3), 7);
        // Header above the pane (a backward step): align it to the top.
        assert_eq!(picker_scroll_for_run(10, 6, 8, 3), 8);
        // Run taller than the pane: cap at the header — its first items show, the rest
        // stays below the fold (never past the point where the first item would hide).
        assert_eq!(picker_scroll_for_run(0, 6, 2, 10), 2);
        // And sitting at the cap already is stable.
        assert_eq!(picker_scroll_for_run(2, 6, 2, 10), 2);
        // Degenerate one-row pane: still lands on the header.
        assert_eq!(picker_scroll_for_run(5, 1, 3, 2), 3);
    }

    /// Emulate pressing `j` down the whole list — as `sync_picker` does: `selected` increments and
    /// `picker_row_scroll_for_selected` recomputes the view-row scroll from the previous one — and
    /// assert the scroll stays well-behaved at every step. This is the direct regression for the
    /// grep / keybindings scroll bug: item-index scroll jumped a header + gap at every group
    /// boundary, landing the selection at the top (reset) or with rows beneath it.
    fn assert_smooth_walk_down(items: &[PickerItem], groups: &[GroupSpan], pane: usize, pin: bool) {
        let rows = picker_window_rows(items.len(), groups, false);
        let sel_row_of = |sel: usize| {
            rows.iter()
                .position(|r| matches!(r, PickerRow::Item(i) if *i == sel))
                .unwrap()
        };
        let max_top = rows.len().saturating_sub(pane);
        let mut top = 0usize;
        for sel in 0..items.len() {
            let next = picker_row_scroll_for_selected(&rows, sel, top, pane, pin);
            let sel_row = sel_row_of(sel);
            // The selection is on screen and clear of the pinned top row (never hidden under it).
            assert!(
                sel_row >= next + pin as usize && sel_row < next + pane,
                "sel {sel} (row {sel_row}) outside [{}, {}) pin={pin} pane={pane}",
                next + pin as usize,
                next + pane
            );
            // Moving down never scrolls the view up.
            assert!(
                next >= top,
                "sel {sel}: scrolled up {top} -> {next} (pane {pane})"
            );
            // When a down-move scrolls, the selection lands on the bottom edge — never leaving
            // rows dangling below it — unless clamped at the list end (the last-screenful case).
            if next > top {
                assert!(
                    sel_row == next + pane - 1 || next == max_top,
                    "sel {sel}: scrolled to {next} but selection at row {sel_row}, not bottom edge {} (pane {pane})",
                    next + pane - 1
                );
            }
            top = next;
        }
    }

    /// Flat pickers (files): view-row scroll collapses to item scroll — one row per move, selection
    /// glued to the edge. This behaviour was already correct; the test pins it as the baseline the
    /// grouped kinds must match.
    #[test]
    fn flat_scroll_moves_one_row_at_a_time() {
        let items = flat_items(40);
        for pane in 3..=12 {
            assert_smooth_walk_down(&items, &[], pane, false);
        }
    }

    /// Grouped pickers (grep / keybindings) now keep the selection on the edge across group
    /// boundaries instead of resetting it to the top or leaving rows below it. Exercises the exact
    /// layouts and pane heights the item-index math jumped 2–3 rows on.
    #[test]
    fn grouped_scroll_stays_at_the_edge_across_group_boundaries() {
        let layouts: &[&[(&str, usize)]] = &[
            &[("a", 4), ("b", 4), ("c", 4)],
            &[("a", 1), ("b", 5), ("c", 2), ("d", 6)],
            &[("a", 3), ("b", 1), ("c", 1), ("d", 3), ("e", 2)],
            &[("a", 7), ("b", 2), ("c", 5)],
        ];
        for layout in layouts {
            let (items, spans) = grep_items(layout);
            for pane in 4..=14 {
                assert_smooth_walk_down(&items, &spans, pane, true);
            }
        }
    }

    fn flat_items(n: usize) -> Vec<PickerItem> {
        (0..n)
            .map(|i| PickerItem::File {
                path_index: i as u32,
                relative_path: format!("f{i}.rs"),
                match_indices: Vec::new(),
                git_status: None,
            })
            .collect()
    }

    /// A recentering refetch (grep scrolled past the 90-item cache) rebuilds the fetched window,
    /// shifting the whole view-row space. `picker_scroll_step` reseeds the scroll from the
    /// selection's remembered pane row so it continues smoothly instead of snapping. Crucially it
    /// preserves that anchor through the *empty-items* frame the refetch produces — the fast-scroll
    /// "selection jumps to the top" regression — which this drives explicitly.
    #[test]
    fn scroll_survives_a_recentering_refetch() {
        let pane = 18usize;

        // Steady state: flat window [0, 90), the selection walked to the last cached item (89),
        // parked on the bottom row (top 72, pane row 17, offset 0).
        let steady = PickerScroll {
            top: 72,
            sel_pane: 17,
            offset: 0,
        };
        assert_eq!(
            picker_scroll_step(
                &picker_window_rows(90, &[], false),
                89,
                pane,
                false,
                0,
                steady
            ),
            steady,
            "already settled at the bottom — no change"
        );

        // The move leaves the cache: the core clears `items` and refetches at 90 - 45 = 45. This
        // sync sees an *empty* window (a fast scroll can pile several up). The anchor must survive:
        // `sel_pane` and `offset` unchanged, only `top` clamped for the (empty) render.
        let empty = picker_window_rows(0, &[], false);
        let mid = picker_scroll_step(&empty, 45, pane, false, 45, steady);
        assert_eq!(
            mid,
            PickerScroll {
                top: 0,
                sel_pane: 17,
                offset: 0
            },
            "empty refetch frame must preserve the continuity anchor, not collapse it"
        );

        // Window B [45, 135) lands; selection is window-relative 45. Reseeded from the preserved
        // pane row (17), the selection stays on the bottom edge — not snapped to the top.
        let rows_b = picker_window_rows(90, &[], false);
        let landed = picker_scroll_step(&rows_b, 45, pane, false, 45, mid);
        assert_eq!(landed.top, 28);
        assert_eq!(
            45 - landed.top,
            pane - 1,
            "selection stays on the bottom edge"
        );
    }

    /// The empty-frame guard also holds for the grouped (pinned) kinds and when the anchor sits
    /// mid-pane: a refetch's empty frame must not drag the selection to the pane top.
    #[test]
    fn fast_scroll_refetch_does_not_snap_grouped_selection_to_top() {
        let pane = 12usize;
        let (_, spans) = grep_items(&[("a.rs", 90)]);
        // Selection parked mid-pane (pane row 6) in window [0, 90).
        let steady = PickerScroll {
            top: 20,
            sel_pane: 6,
            offset: 0,
        };
        // Empty refetch frame (0 items → no rows regardless of spans): anchor must survive.
        let mid = picker_scroll_step(
            &picker_window_rows(0, &[], false),
            45,
            pane,
            true,
            45,
            steady,
        );
        assert_eq!(mid.sel_pane, 6, "anchor preserved through the empty frame");
        // Real window lands; the selection keeps its mid-pane row rather than snapping to row 1.
        let rows = picker_window_rows(90, &spans, false);
        let landed = picker_scroll_step(&rows, 45, pane, true, 45, mid);
        let sel_row = rows
            .iter()
            .position(|r| matches!(r, PickerRow::Item(i) if *i == 45))
            .unwrap();
        assert!(
            sel_row - landed.top > 1,
            "selection kept its mid-pane row, not snapped to the top (pane row {})",
            sel_row - landed.top
        );
    }

    /// Expanding a fetched window into view rows: a header before each group run, a blank gap
    /// above each *interior* header (never the first), items in between. Flat windows map one item
    /// to one row with no headers or gaps.
    #[test]
    fn window_rows_expand_headers_and_gaps() {
        let (items, spans) = grep_items(&[("a.rs", 2), ("b.rs", 2)]);
        assert_eq!(
            picker_window_rows(items.len(), &spans, false),
            vec![
                PickerRow::Header(0),
                PickerRow::Item(0),
                PickerRow::Item(1),
                PickerRow::Gap,
                PickerRow::Header(1),
                PickerRow::Item(2),
                PickerRow::Item(3),
            ]
        );
        assert_eq!(
            picker_window_rows(3, &[], false),
            vec![PickerRow::Item(0), PickerRow::Item(1), PickerRow::Item(2)]
        );
    }

    /// The pinned (sticky) header for a scroll position is the governing group — the last group
    /// starting at or before the top view row — so scrolling mid-group keeps its header pinned.
    #[test]
    fn governing_group_follows_the_scroll() {
        // rows: 0=Hdr a, 1=Item0, 2=Item1, 3=Gap, 4=Hdr b, 5=Item2, 6=Item3
        let (items, spans) = grep_items(&[("a.rs", 2), ("b.rs", 2)]);
        let rows = picker_window_rows(items.len(), &spans, false);
        assert_eq!(picker_governing_group(&rows, 0), Some(0)); // on a's header
        assert_eq!(picker_governing_group(&rows, 2), Some(0)); // mid a → still a
        assert_eq!(picker_governing_group(&rows, 3), Some(0)); // the gap still belongs to a
        assert_eq!(picker_governing_group(&rows, 4), Some(1)); // on b's header
        assert_eq!(picker_governing_group(&rows, 6), Some(1)); // mid b
                                                               // Flat windows have no groups to pin.
        assert_eq!(
            picker_governing_group(&picker_window_rows(3, &[], false), 1),
            None
        );
    }

    /// The pin gate mirrors the native/web clients: file-grouped kinds and Keybindings pin;
    /// References (which still sends section spans) does not.
    #[test]
    fn only_the_grouped_kinds_pin_a_header() {
        assert!(pins_group_header(PickerKind::Grep));
        assert!(pins_group_header(PickerKind::GitChanges));
        assert!(pins_group_header(PickerKind::Keybindings));
        assert!(!pins_group_header(PickerKind::References));
        assert!(!pins_group_header(PickerKind::Files));
    }

    /// Terminal size the picker render tests draw into.
    const TEST_COLS: u16 = 60;
    const TEST_ROWS: u16 = 24;

    /// A bare `AppState` (no editor, so the picker overlay is all there is to draw) around a picker.
    fn picker_app(picker: crate::picker::PickerState) -> AppState {
        use crate::app::StatusMessage;
        AppState {
            workspace_name: "demo".into(),
            workspace_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            tether: None,
            viewport_cols: TEST_COLS as u32,
            viewport_rows: TEST_ROWS as u32,
            should_quit: false,
            status: StatusMessage::default(),
            toasts: Vec::new(),
            hint: None,
            conn: ConnState::Connected,
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker,
            save_prompt: None,
            open_path_prompt: None,
            confirm_prompt: None,
            app_info: None,
            editor: None,
            read: None,
            workspace_settings: None,
            app_settings: None,
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        }
    }

    fn app_info_app() -> AppState {
        let mut st = picker_app(crate::picker::PickerState::default());
        st.app_info = Some(crate::app::AppInfoView {
            sections: aether_client::app_info::sections(
                Some(&aether_protocol::app::AppInfo {
                    version: "0.9.9".into(),
                    commit: Some("abc1234".into()),
                    commit_dirty: false,
                    debug_build: false,
                    appimage: None,
                    profile: "dev".into(),
                    port: Some(2385),
                    pid: 42,
                    started_at_unix_ms: 0,
                    uptime_secs: 90,
                    idle_timeout_secs: None,
                    clients: 1,
                    buffers_open: 2,
                    buffers_unsaved: 0,
                    workspaces_active: 1,
                    paths: Default::default(),
                }),
                &aether_client::session::ConnState::Connected,
            ),
            scroll: Default::default(),
        });
        st
    }

    /// The dialog draws into the same box the pickers use, and its rows land on screen.
    #[test]
    fn app_info_dialog_renders_in_the_picker_box() {
        let rows = render_rows(&app_info_app());
        let screen = rows.join("\n");
        for needle in ["Aether", "Version", "0.9.9", "Instance", "Profile", "dev"] {
            assert!(screen.contains(needle), "{needle:?} missing from\n{screen}");
        }
    }

    /// The mouse hit box the shell tests presses against must be the box that was actually drawn —
    /// otherwise a click just inside the border would read as a backdrop press and dismiss.
    #[test]
    fn app_info_rect_matches_the_drawn_box() {
        let st = app_info_app();
        let rect = app_info_rect(&st).expect("open dialog has a rect");
        assert_eq!(
            rect,
            picker_box_rect(Rect::new(0, 0, TEST_COLS, TEST_ROWS)),
            "hit box tracks the renderer's geometry"
        );
        // The dialog's own border column is inside the box (a press there is swallowed, not a
        // backdrop dismiss).
        assert!(rect.width >= 4 && rect.height >= 3);

        let mut closed = app_info_app();
        closed.app_info = None;
        assert!(app_info_rect(&closed).is_none());
    }

    /// Render the whole picker overlay to an in-memory terminal and read the rows back.
    fn render_rows(state: &AppState) -> Vec<String> {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(TEST_COLS, TEST_ROWS)).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect::<String>()
            })
            .collect()
    }

    fn render_picker_rows(picker: crate::picker::PickerState) -> Vec<String> {
        render_rows(&picker_app(picker))
    }

    /// The screen cell (row, col) where `needle` is rendered — the pointer position a user aiming
    /// at that text would click. Panics if it isn't on screen.
    fn cell_of(rows: &[String], needle: &str) -> (u16, u16) {
        rows.iter()
            .enumerate()
            .find_map(|(y, line)| line.find(needle).map(|x| (y as u16, x as u16)))
            .unwrap_or_else(|| panic!("{needle:?} not rendered in\n{}", rows.join("\n")))
    }

    /// Hit-test a press at `(row, col)` against the picker overlay as drawn for `state`.
    fn hit_at(state: &AppState, (row, col): (u16, u16)) -> PickerHit {
        picker_hit(state, TEST_COLS, TEST_ROWS, row, col)
    }

    /// End-to-end render check for the sticky header: with the window scrolled into the *middle*
    /// of the first group (so its header has scrolled off), the governing group's header is still
    /// pinned over the pane's top row, and the row it covers (a grep hit) is not shown there. This
    /// exercises the real `draw_picker_results` overlay path the scroll-math unit tests can't.
    #[test]
    fn sticky_header_pins_over_the_top_row_when_scrolled_mid_group() {
        let hit = |file: &str, line: u32, preview: &str| PickerItem::GrepHit {
            path_index: 0,
            relative_path: file.to_string(),
            line,
            col: 0,
            preview: preview.to_string(),
            match_indices: Vec::new(),
        };
        // One group, a.rs, with eight identifiable hits.
        let items: Vec<PickerItem> = (0..8).map(|n| hit("a.rs", n, &format!("HIT{n}"))).collect();
        let groups = vec![GroupSpan {
            start: 0,
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "a.rs".into(),
            },
            count: None,
            expanded: None,
        }];
        let picker = crate::picker::PickerState {
            open: true,
            kind: Some(PickerKind::Grep),
            items,
            groups,
            total_matches: 8,
            selected: 4,
            // View rows: 0=Hdr a.rs, 1=HIT0 … 8=HIT7. Scroll to view row 3 (HIT2) — the a.rs
            // header has scrolled above the pane, so only the pin can put it back on the top row.
            visible_start: 3,
            ..Default::default()
        };
        let rows = render_picker_rows(picker);

        let content: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains("HIT") || r.contains("a.rs"))
            .collect();
        let first = content.first().expect("some picker rows rendered");
        assert!(
            first.contains("a.rs") && !first.contains("HIT"),
            "top results row should be the pinned a.rs header, got {first:?}"
        );
        // The covered row (HIT2) is gone; the rows just below the pin are the following hits, and
        // the selected HIT4 is on screen.
        let joined = rows.join("\n");
        assert!(
            !joined.contains("HIT2"),
            "the row under the pin (HIT2) should be covered"
        );
        assert!(
            joined.contains("HIT4"),
            "the selected hit should be visible"
        );
    }

    /// A press on a flat picker's row resolves to *that* row: every rendered entry hit-tests back to
    /// its own window index (the index the shell turns into the core's absolute selection). Driven
    /// through the real render, so the hit-test and the painter are checked against each other.
    #[test]
    fn picker_click_resolves_to_the_row_under_the_pointer() {
        let items = flat_items(10);
        let state = picker_app(crate::picker::PickerState {
            open: true,
            kind: Some(PickerKind::Files),
            total_matches: items.len() as u32,
            items,
            ..Default::default()
        });
        let rows = render_rows(&state);
        for i in 0..10 {
            let name = format!("f{i}.rs");
            assert_eq!(
                hit_at(&state, cell_of(&rows, &name)),
                PickerHit::Item(i),
                "clicking {name} should select row {i}"
            );
        }
    }

    /// In a collapsible picker (docs/picker-groups.md) the headers are real, selectable
    /// `Group` window rows: a press on a header selects it (Enter/click then toggles), a press
    /// on a hit selects that hit, and the window maps 1:1 — no gap rows, no skipped indices.
    #[test]
    fn picker_click_selects_group_header_rows() {
        let group = |path: &str, count: u32, expanded: bool| PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: path.into(),
            },
            count,
            expanded,
        };
        let hit = |line: u32| PickerItem::GrepHit {
            path_index: 0,
            relative_path: "a.rs".into(),
            line,
            col: 0,
            preview: format!("HIT{line}"),
            match_indices: Vec::new(),
        };
        // Row space: [0]=a.rs hdr (expanded), [1..2]=its hits, [3]=b.rs hdr (collapsed).
        let items = vec![
            group("a.rs", 2, true),
            hit(0),
            hit(1),
            group("b.rs", 2, false),
        ];
        let spans = |start: u32, path: &str, expanded: bool| GroupSpan {
            start,
            header: GroupHeader::File {
                path_index: 0,
                relative_path: path.into(),
            },
            count: Some(2),
            expanded: Some(expanded),
        };
        let state = picker_app(crate::picker::PickerState {
            open: true,
            kind: Some(PickerKind::Grep),
            total_matches: 4,
            total_display_rows: Some(4),
            items,
            groups: vec![spans(0, "a.rs", true), spans(3, "b.rs", false)],
            ..Default::default()
        });
        let rows = render_rows(&state);
        assert_eq!(hit_at(&state, cell_of(&rows, "HIT0")), PickerHit::Item(1));
        assert_eq!(hit_at(&state, cell_of(&rows, "HIT1")), PickerHit::Item(2));
        assert_eq!(
            hit_at(&state, cell_of(&rows, "a.rs")),
            PickerHit::Item(0),
            "the top header row renders itself (no stamp) and is selectable"
        );
        assert_eq!(
            hit_at(&state, cell_of(&rows, "b.rs")),
            PickerHit::Item(3),
            "a collapsed group's header is a selectable row"
        );
    }

    /// Scrolled inside an expanded run, the run's header (long gone above) is stamped over the
    /// pane's top row — a press there hits the stamp, not the invisible hit beneath. The stamp
    /// carries the `Group` row's dressing (count), and only exists mid-run: at the run's own
    /// header row the row renders itself (see `picker_click_selects_group_header_rows`).
    #[test]
    fn picker_click_on_the_pinned_header_selects_nothing() {
        let mut items: Vec<PickerItem> = vec![PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "a.rs".into(),
            },
            count: 8,
            expanded: true,
        }];
        items.extend((0..8).map(|n| PickerItem::GrepHit {
            path_index: 0,
            relative_path: "a.rs".into(),
            line: n,
            col: 0,
            preview: format!("HIT{n}"),
            match_indices: Vec::new(),
        }));
        let state = picker_app(crate::picker::PickerState {
            open: true,
            kind: Some(PickerKind::Grep),
            items,
            groups: vec![GroupSpan {
                start: 0,
                header: GroupHeader::File {
                    path_index: 0,
                    relative_path: "a.rs".into(),
                },
                count: Some(8),
                expanded: Some(true),
            }],
            total_matches: 8,
            total_display_rows: Some(9),
            selected: 5,
            visible_start: 3, // top view row = HIT2 (item 3) — mid-run, so the stamp covers it
            ..Default::default()
        });
        let rows = render_rows(&state);
        assert_eq!(
            hit_at(&state, cell_of(&rows, "a.rs")),
            PickerHit::Chrome,
            "the pinned header must not select the hit it covers"
        );
        // The rows below the stamp are still live: HIT3 (item 4) is the first uncovered row.
        assert_eq!(hit_at(&state, cell_of(&rows, "HIT3")), PickerHit::Item(4));
    }

    /// Presses outside the results pane: the query row (and the rest of the box's chrome) is
    /// swallowed, while anything beyond the box is a backdrop press — the shell's click-away.
    #[test]
    fn picker_click_separates_chrome_from_backdrop() {
        let items = flat_items(4);
        let state = picker_app(crate::picker::PickerState {
            open: true,
            kind: Some(PickerKind::Files),
            total_matches: items.len() as u32,
            items,
            ..Default::default()
        });
        let rows = render_rows(&state);
        assert_eq!(
            hit_at(&state, cell_of(&rows, "Find files…")),
            PickerHit::Chrome,
            "the query row is chrome, not a result row"
        );
        // A four-item box collapses to its content, so everything below its bottom border — down to
        // the screen's last row — is backdrop. The border itself still belongs to the box.
        let (bottom, _) = cell_of(&rows, "╰");
        assert_eq!(
            hit_at(&state, (bottom, TEST_COLS / 2)),
            PickerHit::Chrome,
            "the bottom border is the box, not the backdrop"
        );
        assert_eq!(
            hit_at(&state, (bottom + 1, TEST_COLS / 2)),
            PickerHit::Backdrop
        );
        assert_eq!(
            hit_at(&state, (TEST_ROWS - 1, TEST_COLS - 1)),
            PickerHit::Backdrop
        );
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
            Style::default()
                .fg(c(th().accent))
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        );
        dim_backdrop(&mut buf, area);
        let cell = buf.cell((0, 0)).expect("cell present");
        assert_eq!(cell.symbol(), "c", "glyph is preserved");
        assert_eq!(cell.fg, c(th().fg_faint), "foreground muted to grey");
        assert_eq!(cell.bg, c(th().bg), "background flattened to base");
        // Emphasis is preserved — only the colour is flattened — so italic/bold text keeps reading
        // as italic/bold behind the dialog.
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "bold emphasis preserved"
        );
        assert!(
            cell.modifier.contains(Modifier::ITALIC),
            "italic emphasis preserved"
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
        assert!(lines[0]
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
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
        assert_eq!(heading.spans[0].style.fg, Some(c(th().fg_bright)));
        // List bullets rendered.
        assert!(texts.iter().any(|t| t.starts_with("• one")));
        assert!(texts.iter().any(|t| t.starts_with("• two")));
        // Inline code span carries the code background.
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.as_ref() == "inline" && s.style.bg == Some(c(th().md_code_bg))));
        // Fenced code line gets the code background and is width-padded.
        assert!(lines.iter().any(|l| l
            .spans
            .iter()
            .any(|s| s.style.bg == Some(c(th().md_code_bg)))
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
        assert_eq!(
            hover_border_color(&HoverBody::Blocks(vec![blk(None)])),
            c(th().accent)
        );
        // Markdown hover → frost blue.
        assert_eq!(
            hover_border_color(&HoverBody::Markdown(vec![])),
            c(th().accent)
        );
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
        assert_eq!(label.style.fg, Some(c(th().fg_faint)));
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
        assert_eq!(spans[0].style.fg, Some(c(th().ok))); // ready → green dot
                                                         // At the workspace root the tail is just the language — no separator.
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
        assert_eq!(single[0].style.fg, Some(c(th().fg_faint))); // stopped → dim dot
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
            truncate_path_with_indices("a long descriptive name", &[], 8).0,
            "…ve name"
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
        // Grep counts the server-reported display rows (per-file headers included). With no
        // window spans yet (`groups` empty) there's nothing to space.
        p.synthetic_create_idx = None;
        p.kind = Some(PickerKind::Grep);
        p.total_display_rows = Some(8);
        assert_eq!(picker_content_rows(&p), 8);
        // A grouped result set also needs the inter-group gap rows: 8 display rows − 5 matches
        // = 3 headers → 2 gaps between the 3 groups → 10 rows total.
        p.groups = vec![GroupSpan {
            start: 0,
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "a.rs".into(),
            },
            count: None,
            expanded: None,
        }];
        assert_eq!(picker_content_rows(&p), 10);
        // A single group (display rows = matches + 1) adds no gaps — and never underflows.
        p.total_matches = 7;
        assert_eq!(picker_content_rows(&p), 8);
        p.groups.clear();
        // An empty async picker reserves a row for its "Finding…" loading line...
        p.kind = Some(PickerKind::References);
        p.total_matches = 0;
        p.ticking = true;
        assert_eq!(picker_content_rows(&p), 1);
        // ...and a settled empty picker reserves a row for the core's empty note (synced into
        // `empty_note`) — for any kind, not just the async ones. (The Files case above, with no note,
        // reserves nothing and just counts its rows.)
        p.ticking = false;
        p.kind = Some(PickerKind::Diagnostics);
        p.empty_note = Some("No diagnostics".into());
        assert_eq!(picker_content_rows(&p), 1);
    }

    // ---- preview_row_spans ----

    #[test]
    fn grep_hit_line_number_right_aligned() {
        let spans = preview_row_spans(41, "let x = 1;", &[], false, 30);
        let text = spans_text(&spans);
        assert!(text.starts_with("let x = 1;"));
        assert!(text.ends_with("42"));
        assert_eq!(spans_total_width(&spans), 30);
        // The number is dim; the padding before it carries at least the 2-col gap.
        assert!(text.contains("let x = 1;  "));
        let num = spans.last().expect("line-number span");
        assert_eq!(num.style.fg, Some(c(th().fg_faint)));
    }

    #[test]
    fn reference_row_preview_leads_with_right_aligned_location() {
        // Matching the native client: the code preview leads, the dim `path:line` location is
        // right-aligned at the row's edge (not a left prefix). Leading indentation is stripped.
        let spans = reference_item_spans("src/lib.rs", 4, "    helper();", &[], false, 40);
        let text = spans_text(&spans);
        assert!(text.starts_with("helper();"), "preview leads: {text:?}");
        assert!(
            text.ends_with("src/lib.rs:5"),
            "location right-aligned: {text:?}"
        );
        assert_eq!(spans_total_width(&spans), 40);
        let loc = spans.last().expect("location span");
        assert_eq!(loc.style.fg, Some(c(th().fg_faint)), "the location is dim");
    }

    #[test]
    fn symbol_row_name_leads_with_detail_then_right_aligned_kind() {
        use aether_protocol::picker::SymbolKind;
        let spans = symbol_item_spans(
            SymbolRow {
                name: "buffer_id",
                kind: SymbolKind::Field,
                detail: "Option<BufferId>",
                depth: 0,
                context: false,
            },
            &[],
            false,
            40,
        );
        let text = spans_text(&spans);
        assert!(text.starts_with("buffer_id"), "name leads: {text:?}");
        assert!(
            text.contains("Option<BufferId>"),
            "detail beside name: {text:?}"
        );
        assert!(
            text.trim_end().ends_with("field"),
            "kind right-aligned: {text:?}"
        );
        assert_eq!(spans_total_width(&spans), 40);
    }

    #[test]
    fn symbol_row_shows_name_with_empty_detail() {
        // Top-level structs/impls come back with an empty `detail`; the name must still render
        // (regression guard: the row isn't reduced to just the kind tag).
        use aether_protocol::picker::SymbolKind;
        let spans = symbol_item_spans(
            SymbolRow {
                name: "BufferOpenParams",
                kind: SymbolKind::Struct,
                detail: "",
                depth: 0,
                context: false,
            },
            &[],
            false,
            40,
        );
        let text = spans_text(&spans);
        assert!(
            text.starts_with("BufferOpenParams"),
            "name renders: {text:?}"
        );
        assert!(text.trim_end().ends_with("struct"));
    }

    #[test]
    fn symbol_context_row_renders_dim() {
        use aether_protocol::picker::SymbolKind;
        // A context (ancestor) row dims its name too — the name span is the dim fg, not the bright
        // full body foreground a normal row uses.
        let ctx = symbol_item_spans(
            SymbolRow {
                name: "Widget",
                kind: SymbolKind::Struct,
                detail: "",
                depth: 0,
                context: true,
            },
            &[],
            false,
            40,
        );
        let name = ctx.iter().find(|s| s.content.contains("Widget")).unwrap();
        assert_eq!(name.style.fg, Some(picker_dim_fg(false)));
        // A normal (match) row keeps the bright name colour.
        let normal = symbol_item_spans(
            SymbolRow {
                name: "Widget",
                kind: SymbolKind::Struct,
                detail: "",
                depth: 0,
                context: false,
            },
            &[],
            false,
            40,
        );
        let name = normal
            .iter()
            .find(|s| s.content.contains("Widget"))
            .unwrap();
        assert_eq!(name.style.fg, Some(c(th().fg)));
    }

    #[test]
    fn toast_accent_color_matches_kind() {
        use crate::app::StatusKind;
        // Matches the web/native toast border colours.
        assert_eq!(toast_accent_color(StatusKind::Info), c(th().info));
        assert_eq!(toast_accent_color(StatusKind::Success), c(th().ok));
        assert_eq!(toast_accent_color(StatusKind::Warning), c(th().warning));
        assert_eq!(toast_accent_color(StatusKind::Error), c(th().error));
    }

    #[test]
    fn picker_dim_spans_stay_muted_on_highlighted_row() {
        // The faint shade is illegible on the selection background — highlighted rows lift
        // their dim spans (here: the grep line number, the file row's root label) to `fg_muted`:
        // legible on the selection band, but still visibly dimmer than the full-foreground
        // primary text (a root label matching the path's colour reads as part of the path).
        let num = preview_row_spans(41, "let x = 1;", &[], true, 30);
        assert_eq!(num.last().unwrap().style.fg, Some(c(th().fg_muted)));
        let labels = vec!["alpha".to_string(), "beta".to_string()];
        let file = file_item_spans(1, "src/main.rs", &[], None, &labels, true, 40);
        assert_eq!(file.last().unwrap().style.fg, Some(c(th().fg_muted)));
        assert_ne!(
            file.last().unwrap().style.fg,
            file[1].style.fg,
            "root label must stay distinct from the path on the selected row"
        );
    }

    #[test]
    fn grep_hit_truncates_long_preview_keeping_line_number() {
        let preview = "a very long line of code that cannot possibly fit in the row";
        let spans = preview_row_spans(99, preview, &[], false, 24);
        let text = spans_text(&spans);
        assert!(text.contains('…'));
        assert!(text.ends_with("100"));
        assert_eq!(spans_total_width(&spans), 24);
    }

    #[test]
    fn grep_hit_strips_leading_whitespace_and_shifts_matches() {
        // Match on "hel" at chars 4..7 of the untrimmed preview; after stripping the 4-char
        // indent the highlight must land on the same letters.
        let spans = preview_row_spans(0, "    helper();", &[4, 5, 6], false, 40);
        let text = spans_text(&spans);
        assert!(text.starts_with("helper();"));
        let hl: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(c(th().match_highlight)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, "hel");
    }

    #[test]
    fn grep_hit_drops_matches_inside_stripped_whitespace() {
        let spans = preview_row_spans(0, "    x", &[1, 2], false, 40);
        assert!(spans
            .iter()
            .all(|s| s.style.fg != Some(c(th().match_highlight))));
        assert!(spans_text(&spans).starts_with("x "));
    }

    #[test]
    fn jumplist_row_renders_identically_to_a_grep_hit() {
        // The user-visible ask: a Jumplist row strips leading whitespace and shows a right-aligned
        // line number, exactly like grep. Same (line, text, matches) → byte-identical spans.
        let item = PickerItem::JumplistEntry {
            index: 0,
            line: 40,
            display: "    helper();".into(),
            match_indices: vec![4, 5, 6],
        };
        let spans = picker_item_spans(&item, &[], None, false, 40);
        let text = spans_text(&spans);
        assert!(text.starts_with("helper();"), "indent stripped: {text:?}");
        assert!(
            text.ends_with("41"),
            "1-based line number right-aligned: {text:?}"
        );
        let hl: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(c(th().match_highlight)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, "hel", "matches shift onto the same letters");
        // Identical to the equivalent grep hit — the shared renderer guarantees consistency.
        let grep = preview_row_spans(40, "    helper();", &[4, 5, 6], false, 40);
        assert_eq!(text, spans_text(&grep));
    }

    /// The Buffers picker marks the session's tether with the status bar's dim ` *` after the
    /// path (docs/tether.md — closing that row exits the client); other rows are unmarked.
    #[test]
    fn buffers_picker_marks_the_tethered_row() {
        let item = |id: u64| PickerItem::Buffer {
            buffer_id: id,
            display: "notes.md".into(),
            status: aether_protocol::picker::BufferDirtyState::Clean,
            path_index: None,
            relative_path: None,
            match_indices: vec![],
            transient: false,
        };
        let spans = picker_item_spans(&item(7), &[], Some(7), false, 40);
        assert!(
            spans_text(&spans).starts_with("notes.md *"),
            "tethered row carries the mark: {:?}",
            spans_text(&spans)
        );

        let spans = picker_item_spans(&item(8), &[], Some(7), false, 40);
        assert!(
            !spans_text(&spans).contains('*'),
            "a different buffer stays unmarked"
        );
    }

    // ---- keybinding_item_spans ----

    #[test]
    fn keybinding_row_reads_left_to_right_with_right_aligned_chord() {
        // A default mode (Normal) is elided, and the group renders as the section header, not
        // row text. Index 17 in the composed haystack `Delete word Ctrl-w` is the `w` of the
        // chord — the highlight must land on that char after the per-segment rebase.
        let spans = keybinding_item_spans(
            KeybindingRow {
                desc: "Delete word",
                mode: "Normal",
                keys: "Ctrl-w",
            },
            &[17],
            false,
            50,
        );
        let text = spans_text(&spans);
        assert!(
            text.starts_with("Delete word "),
            "description leads, mode elided: {text:?}"
        );
        assert!(text.ends_with("Ctrl-w"), "chord right-aligned: {text:?}");
        assert_eq!(spans_total_width(&spans), 50);
        let hl: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(c(th().match_highlight)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, "w", "keys-segment match styles the right char");
        let desc = spans.first().expect("desc span");
        assert_eq!(desc.style.fg, Some(c(th().fg)));
        // The chord's unmatched chars are frost blue, matching the native client.
        let chord = spans
            .iter()
            .find(|s| s.content.contains("Ctrl-"))
            .expect("chord span");
        assert_eq!(chord.style.fg, Some(c(th().accent)));
    }

    #[test]
    fn keybinding_row_spells_out_insert_and_search_modes() {
        // Insert/Search rows keep their dim mode tag; haystack
        // `Delete word (Insert) Ctrl-w` puts the chord's `w` at 26.
        let spans = keybinding_item_spans(
            KeybindingRow {
                desc: "Delete word",
                mode: "Insert",
                keys: "Ctrl-w",
            },
            &[26],
            false,
            50,
        );
        let text = spans_text(&spans);
        assert!(
            text.starts_with("Delete word (Insert)"),
            "mode shown for Insert: {text:?}"
        );
        assert_eq!(spans_total_width(&spans), 50);
        let hl: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(c(th().match_highlight)))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, "w", "keys-segment match survives the mode prefix");
        let mode = spans
            .iter()
            .find(|s| s.content.contains("Insert"))
            .expect("mode span");
        assert_eq!(mode.style.fg, Some(picker_dim_fg(false)));
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
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
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

    /// A transient buffer italicises the workspace/file segment (no explicit marker text); a
    /// permanent one doesn't.
    #[test]
    fn editor_status_spans_italicise_transient_label() {
        let status = crate::app::StatusMessage::default();
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: true,
                tethered: false,
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
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
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

    /// The tether (docs/tether.md) appends a dim ` *` after the file label — upright even when the
    /// transient italic is on the label — and narrow rows drop it rather than cutting the name.
    #[test]
    fn editor_status_spans_mark_tethered_buffer() {
        let status = crate::app::StatusMessage::default();
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: true,
                tethered: true,
            },
            None,
            Vec::new(),
            &status,
            vec![],
            30,
        );
        assert!(spans_text(&spans).contains("file.rs *"));
        let mark = spans
            .iter()
            .find(|s| s.content == " *")
            .expect("tether mark span");
        assert_eq!(mark.style.fg, Some(c(th().fg_muted)));
        assert!(!mark.style.add_modifier.contains(Modifier::ITALIC));

        // Untethered: no mark.
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
            },
            None,
            Vec::new(),
            &status,
            vec![],
            30,
        );
        assert!(!spans_text(&spans).contains('*'));
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
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
            },
            Some(dot),
            Vec::new(),
            &status,
            vec![],
            30,
        );
        // The dot leads the row, before the workspace name, in the unsaved (frost-blue) colour.
        let text = spans_text(&spans);
        assert!(text.starts_with(&format!("{BUFFER_STATUS_DOT} [proj] file.rs")));
        let dot_span = spans
            .iter()
            .find(|s| s.content.contains(BUFFER_STATUS_DOT))
            .expect("status dot span present");
        assert_eq!(dot_span.style.fg, Some(c(th().state_unsaved)));
        assert_eq!(spans_total_width(&spans), 30);
    }

    #[test]
    fn editor_status_spans_renders_status_with_color() {
        let status = crate::app::StatusMessage::success("saved (rev 1)");
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
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
        assert_eq!(status_span.style.fg, Some(c(th().accent)));
    }

    #[test]
    fn editor_status_spans_connection_indicator_left_with_three_space_gap() {
        // The connection indicator rides the left status slot: capitalised, no icon, a 3-space gap
        // after the file label (matching the file→git gap), yellow for reconnecting.
        let status = crate::app::StatusMessage {
            text: "Reconnecting...".to_string(),
            kind: crate::app::StatusKind::Warning,
        };
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
            },
            None,
            Vec::new(),
            &status,
            vec![Span::raw("12:5")],
            60,
        );
        let text = spans_text(&spans);
        assert!(
            text.contains("file.rs   Reconnecting..."),
            "3-space gap before the indicator: {text:?}"
        );
        let span = spans
            .iter()
            .find(|s| s.content.contains("Reconnecting"))
            .expect("indicator span present");
        assert_eq!(
            span.style.fg,
            Some(c(th().warning)),
            "reconnecting is yellow"
        );
    }

    #[test]
    fn editor_status_spans_drops_status_when_left_pre_alone_overflows() {
        // total=12, right=4, gap=1 → left_max=7. "[proj] file.rs" (14) > 7: the file label gets
        // the budget (and fits exactly), the workspace prefix — which can't fit whole alongside it
        // — is dropped, and the status is dropped entirely.
        let status = crate::app::StatusMessage::error("save failed: disk full");
        let spans = build_editor_status_spans(
            StatusLabel {
                workspace_prefix: "[proj] ",
                file_label: "file.rs",
                transient: false,
                tethered: false,
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
                workspace_prefix: "[proj] ",
                file_label: "src/deeply/nested/module/file.rs",
                transient: false,
                tethered: false,
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

    /// (char, fg, bg, bold) per column from `build_spans` (ASCII input → col == byte).
    fn cells_of(spans: &[Span<'static>]) -> Vec<(char, Option<Color>, Option<Color>, bool)> {
        let mut out = Vec::new();
        for s in spans {
            let bold = s.style.add_modifier.contains(Modifier::BOLD);
            for c in s.content.chars() {
                out.push((c, s.style.fg, s.style.bg, bold));
            }
        }
        out
    }

    #[test]
    fn sneak_targets_on_visual_row_skips_targets_off_the_row() {
        // A word on a later wrapped row (byte 200) must not panic or appear when rendering an
        // earlier row [0,17) — regression for the `clamp(min > max)` panic.
        let targets = [SneakTarget {
            start: 200,
            end: 208,
            prefix_end: 201,
            label: Some('j'),
        }];
        assert!(sneak_targets_on_visual_row(0, 17, &targets).is_empty());

        // A word starting within the row is clamped and kept, with its chip.
        let here = [SneakTarget {
            start: 4,
            end: 9,
            prefix_end: 6,
            label: Some('k'),
        }];
        assert_eq!(
            sneak_targets_on_visual_row(0, 17, &here),
            vec![(4, 9, 6, Some('k'))]
        );
    }

    #[test]
    fn build_spans_paints_diff_emphasis_under_search_and_selection() {
        // Emphasis on [0,6), a search match on [2,4), selection on [4,5): both paint over the
        // emphasis fill, which keeps the remaining cells.
        let emph_bg = c(th().git_modified_emph_bg);
        let cells = cells_of(&build_spans(
            "abcdef",
            &[],
            Some((4, 5)),
            &[(2, 4)],
            &[(0, 6)],
            emph_bg,
            &[],
            &[],
            &[],
            80,
        ));
        assert_eq!(cells[0].2, Some(emph_bg), "plain emphasis cell");
        assert_eq!(cells[2].2, Some(c(th().fill_dim)), "search wins over emphasis");
        assert_eq!(cells[4].2, Some(c(th().bg_visual)), "selection wins over emphasis");
        assert_eq!(cells[5].2, Some(emph_bg));
    }

    #[test]
    fn deleted_virtual_row_spans_split_on_emphasis() {
        use aether_protocol::viewport::EmphasisRange;
        // "old code" with "code" emphasized: base red fill outside, stronger fill inside, padding
        // after the text keeps the base fill.
        let spans = deleted_virtual_row_spans(
            "old code",
            12,
            DiffStage::Unstaged,
            &[EmphasisRange { start: 4, end: 8 }],
        );
        let cells = cells_of(&spans);
        assert_eq!(cells.len(), 12, "padded to the content width");
        assert_eq!(cells[0].2, Some(c(th().git_deleted_bg)));
        assert_eq!(cells[4].2, Some(c(th().git_deleted_emph_bg)));
        assert_eq!(cells[7].2, Some(c(th().git_deleted_emph_bg)));
        assert_eq!(cells[8].2, Some(c(th().git_deleted_bg)), "padding on base fill");
        // Emphasized runs switch to the normal fg (legible on the vivid fill); the rest keeps
        // the deleted red.
        assert_eq!(cells[0].1, Some(c(th().git_deleted)));
        assert_eq!(cells[4].1, Some(c(th().fg)));
        assert_eq!(cells[8].1, Some(c(th().git_deleted)));
    }

    #[test]
    fn deleted_virtual_row_spans_expand_tabs_inside_emphasis() {
        use aether_protocol::viewport::EmphasisRange;
        // Emphasis over the tab (byte 1): its expanded cells all take the emphasis fill, and the
        // byte→cell mapping stays anchored to original byte positions.
        let spans = deleted_virtual_row_spans(
            "a\tb",
            10,
            DiffStage::Unstaged,
            &[EmphasisRange { start: 1, end: 2 }],
        );
        let cells = cells_of(&spans);
        assert_eq!(cells[0].2, Some(c(th().git_deleted_bg)));
        for i in 1..(1 + TAB_WIDTH as usize) {
            assert_eq!(cells[i].2, Some(c(th().git_deleted_emph_bg)), "tab cell {i}");
        }
        assert_eq!(cells[1 + TAB_WIDTH as usize].2, Some(c(th().git_deleted_bg)));
    }

    #[test]
    fn build_spans_paints_sneak_label_and_bands_the_prefix() {
        // "function", whole word [0,8), query "fu" → prefix [0,2), label 'j'.
        let sneak = [(0u32, 8u32, 2u32, Some('j'))];
        let cells = cells_of(&build_spans(
            "function",
            &[],
            None,
            &[],
            &[],
            c(th().git_modified_emph_bg),
            &[],
            &[],
            &sneak,
            80,
        ));
        // Col 0: the label glyph, dark-on-yellow.
        assert_eq!(cells[0].0, 'j', "label glyph painted over the first cell");
        assert_eq!(
            cells[0].2,
            Some(c(th().match_highlight)),
            "label on Aurora yellow"
        );
        // Col 1: the rest of the prefix — the typed char on the cooler band (not bold, not yellow).
        assert_eq!(cells[1].0, 'u', "prefix shows the typed char");
        assert_eq!(cells[1].2, Some(c(th().sneak_prefix_bg)), "cooler band");
        assert!(!cells[1].3, "not bold");
        // Col 2 onward: the candidate-word tint (the dim fill).
        assert_eq!(cells[2].0, 'n');
        assert_eq!(cells[2].2, Some(c(th().fill_dim)));
    }

    #[test]
    fn build_spans_underlines_diagnostic_in_severity_color() {
        let diags = [(2u32, 4u32, DiagnosticSeverity::Warning)];
        let cells = underline_cols(&build_spans("abcdef", &[], None, &[], &[], c(th().git_modified_emph_bg), &[], &diags, &[], 80));
        for (col, (underlined, color)) in cells.into_iter().enumerate() {
            if col == 2 || col == 3 {
                assert!(underlined, "cell {col} underlined");
                assert_eq!(color, Some(c(th().warning)), "cell {col} warning-yellow");
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
        let cells = underline_cols(&build_spans("xyz", &[], None, &[], &[], c(th().git_modified_emph_bg), &[], &diags, &[], 80));
        assert_eq!(cells[1].1, Some(c(th().error)), "overlap shows error red");
        assert_eq!(
            cells[0].1,
            Some(c(th().fg)),
            "non-overlap keeps hint colour"
        );
    }

    #[test]
    fn lsp_status_color_maps_states() {
        assert_eq!(lsp_status_color(&LspStatus::Ready), c(th().ok));
        assert_eq!(lsp_status_color(&LspStatus::Initializing), c(th().warning));
        assert_eq!(lsp_status_color(&LspStatus::Restarting), c(th().warning));
        assert_eq!(
            lsp_status_color(&LspStatus::Crashed {
                code: None,
                message: String::new()
            }),
            c(th().error)
        );
        assert_eq!(lsp_status_color(&LspStatus::Stopped), c(th().fg_faint));
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
            c(th().cursor_line_added_bg)
        );
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Modified), Unstaged),
            c(th().cursor_line_modified_bg)
        );
        assert_ne!(
            cursor_line_bg(Some(DiffMarker::Added), Unstaged),
            c(th().git_added_bg)
        );
        // A staged line keeps its dimmer identity under the cursor instead of flaring back up to
        // the unstaged brightness.
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Added), Staged),
            c(th().cursor_line_staged_added_bg)
        );
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Modified), Staged),
            c(th().cursor_line_staged_modified_bg)
        );
        // Deleted (no real-line tint) and unchanged lines fall back to the plain cursorline.
        assert_eq!(
            cursor_line_bg(Some(DiffMarker::Deleted), Unstaged),
            c(th().cursor_line_bg)
        );
        assert_eq!(cursor_line_bg(None, Unstaged), c(th().cursor_line_bg));
    }

    // ---- theme ----

    /// Role-based theming: at Dark (the thread default) the accessors resolve to the historic
    /// constants — dark renders pixel-identical to the pre-theme TUI — while Light resolves the
    /// same roles differently. Sets the mode explicitly on entry and restores Dark on exit so
    /// the thread-local can't inherit or leak a mode when tests share a thread.
    #[test]
    fn theme_mode_flips_the_palette() {
        set_theme_mode(ThemeMode::Dark);
        assert_eq!(c(th().bg), Color::Rgb(46, 52, 64)); // NORD0
        let dark_comment = theme_for("comment");
        assert_eq!(dark_comment.fg, Some(Color::Rgb(123, 136, 161))); // NORD3_BRIGHTER
        assert!(dark_comment.add_modifier.contains(Modifier::ITALIC));

        set_theme_mode(ThemeMode::Light);
        assert_eq!(
            c(th().bg),
            Color::Rgb(0xec, 0xef, 0xf4),
            "light bg swaps to Snow Storm"
        );
        assert_ne!(theme_for("comment").fg, dark_comment.fg);
        assert!(
            theme_for("comment").add_modifier.contains(Modifier::ITALIC),
            "attributes ride the mode flip"
        );
        set_theme_mode(ThemeMode::Dark);
    }
}
