//! Ratatui rendering. The buffer fills the screen except for the bottom status row.

use crate::app::{
    grep_counter_label, search_counter_label, search_match_count_label, AppState, EditorMode,
};
use aether_protocol::cursor::CursorState;
use aether_protocol::picker::PickerItem;
use aether_protocol::search::SearchMatchRange;
use aether_protocol::viewport::{Highlight, VisualRow, WrapMode};
use aether_protocol::LogicalPosition;
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
const NORD4: Color = Color::Rgb(216, 222, 233); // Snow Storm — main foreground
const NORD7: Color = Color::Rgb(143, 188, 187); // Frost — types
const NORD8: Color = Color::Rgb(136, 192, 208); // Frost — functions, accents
const NORD9: Color = Color::Rgb(129, 161, 193); // Frost — keywords, operators
const NORD10: Color = Color::Rgb(94, 129, 172); // Frost — deep blue (active selection bg)
const NORD11: Color = Color::Rgb(191, 97, 106); // Aurora red — error text
const NORD12: Color = Color::Rgb(208, 135, 112); // Aurora orange — attributes, macros
const NORD13: Color = Color::Rgb(235, 203, 139); // Aurora yellow — string escapes
const NORD14: Color = Color::Rgb(163, 190, 140); // Aurora green — strings
const NORD15: Color = Color::Rgb(180, 142, 173); // Aurora purple — numbers, constants

pub fn draw(f: &mut Frame, state: &AppState) {
    // The status row carries activation feedback, save-as / new-file prompts, and the dirty +
    // cursor indicator for an active editor. The add-root prompt lives *inside* the settings
    // overlay, not here. We show the row when an editor exists, or when a transient status
    // message is up (e.g. after activating a project) — otherwise hide so the no-project view
    // gets full vertical space.
    let show_status = state.has_editor() || !state.status.is_empty();
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
    // The settings overlay needs a caret on its input row even when no editor exists (e.g. right
    // after `project/create`). Fall back to a zero Rect for the status area in that case — the
    // settings branch in `place_terminal_cursor` doesn't read it.
    if state.has_editor() || state.project_settings.is_some() {
        let buffer_area = chunks[0];
        let status_area = chunks.get(1).copied().unwrap_or(Rect::default());
        place_terminal_cursor(f, state, buffer_area, status_area);
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

/// Header block: `Project Settings (<name>)` heading, a blank row, `Project roots:` label.
/// Degrades gracefully when the header area is shorter than 3 rows.
fn draw_settings_header(
    f: &mut Frame,
    settings: &crate::app::ProjectSettingsState,
    area: Rect,
) {
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
    let mut lines: Vec<Line> = Vec::with_capacity(3);
    if area.height >= 1 {
        let heading = format!("Project Settings ({})", settings.project_name);
        let heading = truncate_right(&heading, area.width as usize);
        lines.push(Line::from(Span::styled(heading, heading_style)));
    }
    if area.height >= 2 {
        lines.push(Line::from(""));
    }
    if area.height >= 3 {
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
    let header_h = 3u16.min(content.height);
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
/// project picker); the pending-delete row swaps the path for a red `remove "<path>"? [y/N]`
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
    let start = settings
        .selected
        .saturating_sub(max.saturating_sub(1))
        .min(total_items.saturating_sub(max));
    let area_w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for i in start..(start + max).min(total_items) {
        let highlighted = i == settings.selected;
        // 1-col indent so list items sit visually under the section label.
        let leading = Span::styled(" ", base_style);
        let text_budget = area_w.saturating_sub(1);
        if i < settings.roots.len() {
            let root = &settings.roots[i];
            let pending = settings.pending_delete && i == settings.selected;
            if pending {
                const PREFIX: &str = "remove \"";
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
            let dirty_marker = if root_has_dirty_buffer(state, root) {
                " [+]"
            } else {
                ""
            };
            let body = format!("{root}{dirty_marker}");
            let truncated = truncate_middle(&body, text_budget);
            let bg = if highlighted { NORD2 } else { NORD0 };
            let path_style = Style::default().fg(NORD4).bg(bg);
            lines.push(Line::from(vec![leading, Span::styled(truncated, path_style)]));
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
    let start = settings
        .selected
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

/// Mirror of the editor's `[+]` marker, applied per-root: a root has a dirty buffer if any
/// open editor (any client, since AppState only sees its own — but file_path is informative
/// enough) has a path under it and unsaved changes. This client only knows about its own
/// active editor, so the marker reflects "your active buffer is under this root and is dirty."
/// Server-side dirty buffers from other clients won't show. Acceptable for v1.
fn root_has_dirty_buffer(state: &AppState, root: &str) -> bool {
    let Some(ed) = state.editor.as_ref() else {
        return false;
    };
    if !ed.dirty_marker_visible() {
        return false;
    }
    let Some(path) = ed.file_path.as_deref() else {
        return false;
    };
    let root_path = std::path::Path::new(root);
    let buf_path = std::path::Path::new(path);
    buf_path == root_path || buf_path.starts_with(root_path)
}

/// Empty no-project view: a centered hint telling the user how to open the project picker.
/// Drawn instead of the buffer pane when `state.editor` is `None`. Fills the full pane in the
/// editor's NORD0 background so the no-project state visually matches an open editor instead of
/// falling through to the terminal's default colors.
fn draw_no_project_view(f: &mut Frame, _state: &AppState, area: Rect) {
    let base = Style::default().bg(NORD0).fg(NORD4);
    f.render_widget(Paragraph::new("").style(base), area);
    let hint = vec![
        Line::from(Span::styled(
            "no project active",
            base.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("", base)),
        Line::from(Span::styled("Space p   pick a project", base)),
        Line::from(Span::styled("Space q   quit", base)),
    ];
    let para = Paragraph::new(hint)
        .alignment(ratatui::layout::Alignment::Center)
        .style(base);
    let inner_height = 4u16;
    let top_pad = area.height.saturating_sub(inner_height) / 2;
    let target = Rect {
        x: area.x,
        y: area.y + top_pad,
        width: area.width,
        height: inner_height.min(area.height.saturating_sub(top_pad)),
    };
    f.render_widget(para, target);
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
    let width = width.min(area.width).max(1);
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

fn draw_picker_overlay(f: &mut Frame, state: &AppState, area: Rect) {
    let box_area = picker_box_rect(area);
    if box_area.width < 4 || box_area.height < 4 {
        return; // Too small to draw anything meaningful.
    }
    f.render_widget(Clear, box_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(NORD4))
        .style(Style::default().bg(NORD0).fg(NORD4));
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);

    // Inner layout: input row, separator row (full-width, ties into the borders), results. The
    // input and results panes get one column of horizontal padding so text isn't flush with the
    // border; the separator deliberately uses the full inner width.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    draw_picker_input_row(f, state, pad_horizontal(rows[0]));
    draw_picker_separator(f, box_area, rows[1]);
    draw_picker_results(f, state, pad_horizontal(rows[2]));
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
/// flush with the typed query (cursor lands just after the prefix). If the row is too narrow
/// to hold the counts, they get dropped first so the query stays visible.
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
    let prefix_has_content = prefix_w > 0;

    let (left_text, left_style, left_w) = if state.picker.query.is_empty() {
        // Suppress the placeholder when the explorer prefix is already telling the user where
        // they are — the path *is* the context. Other pickers keep their placeholder.
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
        String::new()
    } else {
        let suffix = if state.picker.ticking { " …" } else { "" };
        // Position-in-results / total: "you're on item N of M". `selected` is a cache index;
        // `offset + selected + 1` is the 1-based position in the full result set.
        let position = state.picker.offset as u64 + state.picker.selected as u64 + 1;
        let position = position.min(state.picker.total_matches as u64);
        format!("{}/{}{}", position, state.picker.total_matches, suffix)
    };
    let counts_w = counts.width();

    let mut spans: Vec<Span<'static>> = Vec::new();
    if !label_text.is_empty() {
        spans.push(Span::styled(label_text, label_style));
    }
    if !path_text.is_empty() {
        spans.push(Span::styled(path_text, path_style));
    }
    spans.push(Span::styled(left_text, left_style));
    let used = prefix_w + left_w;
    if !counts.is_empty() && used + counts_w + 1 <= total_width {
        let pad = total_width.saturating_sub(used + counts_w);
        spans.push(Span::styled(" ".repeat(pad), base_style));
        spans.push(Span::styled(counts, base_style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(base_style), area);
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
    // Over budget. Sacrifice the label first (the path is more useful), then left-truncate the
    // path with a leading `…` if it's still over budget on its own.
    let path_w = path_part.width();
    if path_w <= max {
        return (String::new(), path_part);
    }
    let chars: Vec<char> = path_part.chars().collect();
    let budget = max.saturating_sub(1); // 1 cell for the leading `…`
    let mut kept_w = 0;
    let mut kept_start = chars.len();
    for (i, c) in chars.iter().enumerate().rev() {
        let cw = UnicodeWidthChar::width(*c).unwrap_or(0);
        if kept_w + cw > budget {
            break;
        }
        kept_w += cw;
        kept_start = i;
    }
    let kept: String = chars[kept_start..].iter().collect();
    (String::new(), format!("…{kept}"))
}

fn picker_placeholder(kind: Option<aether_protocol::picker::PickerKind>) -> &'static str {
    match kind {
        Some(aether_protocol::picker::PickerKind::Files) => "Search files…",
        Some(aether_protocol::picker::PickerKind::Buffers) => "Switch buffer…",
        Some(aether_protocol::picker::PickerKind::Grep) => "Grep workspace…",
        Some(aether_protocol::picker::PickerKind::Explorer) => "Filter entries…",
        Some(aether_protocol::picker::PickerKind::Projects) => "Switch project…",
        None => "Search…",
    }
}

/// Horizontal line under the input. Extends the line *into* the side borders with tee characters
/// so the separator visually ties into the outer block — done by writing directly to the frame
/// buffer because the block has already been rendered.
fn draw_picker_separator(f: &mut Frame, box_area: Rect, area: Rect) {
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
    // Reserve the rightmost column for the scroll indicator when the result set is taller than
    // the visible window. Otherwise use the full width for paths.
    let needs_scrollbar = state.picker.total_matches as u16 > area.height;
    let text_width = if needs_scrollbar {
        area.width.saturating_sub(1)
    } else {
        area.width
    };
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
        let highlighted = i == state.picker.selected;
        let mut spans = picker_item_spans(
            item,
            &state.root_labels,
            highlighted,
            text_width as usize,
        );
        // Italicise the synthetic "+ Create …" row so it reads as an action affordance rather
        // than a real entry. Applied uniformly across all spans of the row (including any
        // fuzzy-match-highlight spans), since the synthetic never has match indices anyway.
        if Some(i) == state.picker.synthetic_create_idx {
            for span in spans.iter_mut() {
                span.style = span.style.add_modifier(Modifier::ITALIC);
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
            x: area.x + text_width,
            y: area.y,
            width: 1,
            height: area.height,
        };
        draw_picker_scrollbar(f, state, scrollbar);
    }
}

fn draw_picker_scrollbar(f: &mut Frame, state: &AppState, area: Rect) {
    let total = state.picker.total_matches.max(1) as u64;
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
    let track_h = area.height as u64;
    if track_h == 0 {
        return;
    }
    // Thumb spans `window / total` of the track, at least 1 cell. Position is `offset / total`.
    let thumb_h = ((window * track_h + total - 1) / total).max(1).min(track_h) as u16;
    let max_thumb_y = (track_h as u16).saturating_sub(thumb_h);
    let thumb_y = ((offset * track_h) / total) as u16;
    let thumb_y = thumb_y.min(max_thumb_y);

    let buf = f.buffer_mut();
    let thumb_style = Style::default().fg(NORD8).bg(NORD0);
    let track_style = Style::default().fg(NORD3).bg(NORD0);
    for i in 0..(area.height) {
        let in_thumb = i >= thumb_y && i < thumb_y + thumb_h;
        let glyph = if in_thumb { "█" } else { "│" };
        let style = if in_thumb { thumb_style } else { track_style };
        buf.set_string(area.x, area.y + i, glyph, style);
    }
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
    } = item
    {
        return dir_entry_spans(name, *is_dir, match_indices, highlighted, max_width);
    }
    // File rows get a leading dim `{label}: ` prefix; everything else falls through with the
    // legacy single-string display.
    if let PickerItem::File {
        path_index,
        relative_path,
        match_indices,
    } = item
    {
        return file_item_spans(
            *path_index,
            relative_path,
            match_indices,
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
        return root_item_spans(*path_index, match_indices, root_labels, highlighted, max_width);
    }

    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);

    // Trailing dirty marker for buffer items — matches the status bar's `[+]` indicator. Goes
    // after the display so it doesn't shift `match_indices` (which index into the display).
    let (display_raw, match_indices, dirty_suffix) = match item {
        PickerItem::Buffer {
            display,
            dirty,
            match_indices,
            ..
        } => (
            display.as_str(),
            match_indices.as_slice(),
            if *dirty { " [+]" } else { "" },
        ),
        PickerItem::Project {
            name,
            match_indices,
        } => (name.as_str(), match_indices.as_slice(), ""),
        PickerItem::File { .. }
        | PickerItem::GrepHit { .. }
        | PickerItem::DirEntry { .. }
        | PickerItem::Root { .. } => unreachable!("handled above"),
    };

    let text_budget = max_width.saturating_sub(dirty_suffix.len());
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
    if !dirty_suffix.is_empty() {
        spans.push(Span::styled(dirty_suffix.to_string(), base.fg(NORD13)));
    }
    spans
}

/// Header row above each file's hits in the Grep picker. Renders as `{label}: {relative}` with
/// the label in NORD4 (white, matches the label style on File rows) and the separator + path in
/// NORD8 (frost blue) so the file path stays the dominant visual element. Non-selectable; the
/// picker cursor lives on the GrepHit rows below.
fn grep_file_header_spans(
    path_index: u32,
    relative_path: &str,
    root_labels: &[String],
    max_width: usize,
) -> Vec<Span<'static>> {
    let label_style = Style::default()
        .fg(NORD4)
        .bg(NORD0)
        .add_modifier(Modifier::BOLD);
    let path_style = Style::default()
        .fg(NORD8)
        .bg(NORD0)
        .add_modifier(Modifier::BOLD);
    let label = root_label_or_blank(root_labels, path_index);
    if label.is_empty() {
        let (display, _) = truncate_path_with_indices(relative_path, &[], max_width);
        return vec![Span::styled(display, path_style)];
    }
    // Reserve space for `{label}: ` first; whatever remains goes to the path.
    let prefix = format!("{label}: ");
    let prefix_w = prefix.width();
    if prefix_w >= max_width {
        // Box is too narrow to hold even the label — fall back to truncating the combined form.
        let display_raw = format!("{prefix}{relative_path}");
        let (display, _) = truncate_path_with_indices(&display_raw, &[], max_width);
        return vec![Span::styled(display, label_style)];
    }
    let path_budget = max_width - prefix_w;
    let (path_display, _) = truncate_path_with_indices(relative_path, &[], path_budget);
    vec![
        Span::styled(prefix, label_style),
        Span::styled(path_display, path_style),
    ]
}

/// File picker row: `{label}: {relative}` with the label in a dim foreground (NORD3) and the
/// relative path styled like other picker items, including the fuzzy-match highlight. The
/// label is plain text — match indices in the protocol always index into `relative_path` only.
fn file_item_spans(
    path_index: u32,
    relative_path: &str,
    match_indices: &[u32],
    root_labels: &[String],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let base = Style::default().fg(NORD4).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(NORD3).bg(bg);
    let label = root_label_or_blank(root_labels, path_index);
    let prefix = if label.is_empty() {
        String::new()
    } else {
        format!("{label}: ")
    };
    let prefix_w = prefix.width();
    let relative_budget = max_width.saturating_sub(prefix_w);
    let (display, indices) =
        truncate_path_with_indices(relative_path, match_indices, relative_budget);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, label_style));
    }
    push_styled_with_match_indices(&mut spans, &display, &indices, base, match_style);
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

/// One Grep hit row: indented under the file header, line number left-padded to a small fixed
/// width in a dim color, then the preview with `match_indices` highlighted the same way the
/// fuzzy-match-tinted Files/Buffers rows are.
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
    let line_style = base.fg(NORD3);
    let indent = "  ";
    // Line numbers in this codebase happily fit in 5 cols; widen as needed for huge files.
    let line_str = format!("{:>5} ", line + 1);
    let prefix_w = indent.width() + line_str.width();
    let preview_budget = max_width.saturating_sub(prefix_w);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(indent.to_string(), base),
        Span::styled(line_str, line_style),
    ];

    // Truncate the preview from the right when it overflows; drop match indices that fall past
    // the cut. Centering on the first match would be a nicer follow-up; for now long lines just
    // show their head, which is usually where the interesting prefix is anyway.
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

/// One Explorer entry row: leaf name with a trailing `/` for directories, NORD8 (frost blue)
/// for directories, fuzzy-match highlights overlaid the same way the Files picker does. The
/// `/` suffix is appended *after* the name proper so `match_indices` (which index into the
/// name) don't have to know about it.
fn dir_entry_spans(
    name: &str,
    is_dir: bool,
    match_indices: &[u32],
    highlighted: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let bg = if highlighted { NORD2 } else { NORD0 };
    let fg = if is_dir { NORD8 } else { NORD4 };
    let base = Style::default().fg(fg).bg(bg);
    let match_style = base.fg(NORD13).add_modifier(Modifier::BOLD);
    let suffix = if is_dir { "/" } else { "" };
    let text_budget = max_width.saturating_sub(suffix.len());
    let (display, indices) = truncate_path_with_indices(name, match_indices, text_budget);

    let mut spans: Vec<Span<'static>> = Vec::new();
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
fn truncate_path_with_indices(
    path: &str,
    match_indices: &[u32],
    max_width: usize,
) -> (String, Vec<u32>) {
    if max_width == 0 {
        return (String::new(), Vec::new());
    }
    let total_w = path.width();
    if total_w <= max_width {
        return (path.to_string(), match_indices.to_vec());
    }
    // Keep characters from the end until we've filled max_width - 1 (leave 1 cell for `…`).
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
    let top = state.ed().scroll_logical_line;
    let selection = ordered_selection(&state.ed().cursor);
    let viewport_rows = area.height as usize;
    let viewport_cols = area.width;
    // Horizontal scroll only kicks in for wrap-off; soft-wrapped content always fits horizontally.
    let scroll_col = if matches!(state.ed().wrap, WrapMode::None) {
        state.ed().scroll_col
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_rows);
    let mut logical_line = top;

    'outer: loop {
        if lines.len() >= viewport_rows {
            break;
        }
        let local_idx = (logical_line as i64) - (state.ed().window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.ed().lines.len() as i64 {
            break;
        }
        let render = &state.ed().lines[local_idx as usize];

        let last_vrow_idx = render.visual_rows.len().saturating_sub(1);
        for (vrow_idx, vrow) in render.visual_rows.iter().enumerate() {
            if lines.len() >= viewport_rows {
                break 'outer;
            }
            let is_last_vrow_of_line = vrow_idx == last_vrow_idx;
            let segment = match vrow.segments.first() {
                Some(s) => s,
                None => {
                    // Empty line — paint a trailing cell when the selection continues past
                    // this line (the line's newline char is conceptually in the range).
                    let empty_newline_selected = is_last_vrow_of_line
                        && selection
                            .is_some_and(|(s, e)| s.line <= logical_line && e.line > logical_line);
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if empty_newline_selected {
                        spans.push(Span::styled("↵", Style::default().bg(NORD10).fg(NORD3)));
                    }
                    lines.push(Line::from(spans));
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
            let brackets_on_row = bracket_positions_on_visual_row(
                logical_line,
                vrow.byte_offset,
                row_text_len,
                state.ed().cursor.match_bracket,
            );

            // Apply horizontal scroll to the row's text + highlights + selection. Skips zero
            // bytes when scroll_col == 0 (the common case), so this is a no-op under soft wrap.
            let (clipped_text, clipped_highlights, clipped_sel, clipped_matches) = clip_horizontal(
                &segment.text,
                &segment.highlights,
                sel_on_row,
                &matches_on_row,
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
                body_width,
            ));
            if highlight_trailing_newline {
                spans.push(Span::styled("↵", Style::default().bg(NORD10).fg(NORD3)));
            }
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

/// `Some((lo, hi))` when the selection covers more than one char (range). `None` for a point
/// cursor — the block cursor alone visualises the 1-char "selection", so we don't draw the
/// extra range highlight.
fn ordered_selection(cursor: &CursorState) -> Option<(LogicalPosition, LogicalPosition)> {
    if cursor.is_point() {
        return None;
    }
    let p = cursor.position;
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

    let mut byte_in_match: Vec<bool> = vec![false; trunc_len];
    for (s, e) in matches {
        let s = (*s as usize).min(trunc_len);
        let e = (*e as usize).min(trunc_len);
        for i in s..e {
            byte_in_match[i] = true;
        }
    }

    let mut byte_is_match_bracket: Vec<bool> = vec![false; trunc_len];
    for &b in match_brackets {
        let idx = (b as usize).min(trunc_len);
        if idx < trunc_len {
            byte_is_match_bracket[idx] = true;
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
            Span::styled(state.status.text.clone(), status_message_style(&state.status)),
        ])
    } else if matches!(state.ed().mode, EditorMode::Search) {
        let prompt = format!("/{}", state.ed().search.query.text);
        let text = match search_match_count_label(state) {
            Some(count) => format!("{prompt}    {count}"),
            None => prompt,
        };
        Line::from(vec![Span::raw(text)])
    } else {
        let dirty_marker = state.buffer_status_marker();
        // Project / file / dirty / transient status sit on the left; counter (search and/or
        // grep, in that order) and cursor position sit on the right, with the counter to the
        // left of the position. When the row is narrow we truncate the right edge of the left
        // segment with `…` so the right segment stays whole and the position never gets
        // painted over.
        let counter_parts: Vec<String> =
            [search_counter_label(state), grep_counter_label(state)]
                .into_iter()
                .flatten()
                .collect();
        let pos = format_position(state);
        let right = if counter_parts.is_empty() {
            pos
        } else {
            format!("{}  {}", counter_parts.join(" "), pos)
        };

        let mut left_pre = format!("[{}] {}", state.project_name, state.ed().file_label);
        if !dirty_marker.is_empty() {
            left_pre.push(' ');
            left_pre.push_str(dirty_marker);
        }

        Line::from(build_editor_status_spans(
            &left_pre,
            &state.status,
            &right,
            area.width as usize,
        ))
    };
    let p = Paragraph::new(line).style(Style::default().bg(NORD1).fg(NORD4));
    f.render_widget(p, area);
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
    let ghost = prompt.ghost_suffix(&state.project_paths).unwrap_or_default();
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
        if used + counter_w + 1 <= total_width {
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

/// Build the spans for the default editor status row: `left_pre` on the left in the base
/// style, optional colored status message after a `    ` separator, then padding pushing the
/// right segment flush to the row edge. When the row is too narrow:
///   - the status text truncates first (`…`), preserving the project/file/dirty bit;
///   - if even `left_pre` can't fit, that gets truncated and the status is dropped entirely.
/// The right segment is never truncated — the cursor position is more useful than the message.
fn build_editor_status_spans(
    left_pre: &str,
    status: &crate::app::StatusMessage,
    right: &str,
    total_width: usize,
) -> Vec<Span<'static>> {
    let base_style = Style::default().bg(NORD1).fg(NORD4);
    let right_w = right.width();
    // Always keep at least one cell of gap between the left content and the right segment.
    let left_max = total_width.saturating_sub(right_w + 1);

    let left_pre_w = left_pre.width();
    let mut spans: Vec<Span<'static>> = Vec::new();

    if left_pre_w >= left_max {
        // Even the pre-status segment overflows — truncate it and drop the status.
        spans.push(Span::styled(truncate_to_width(left_pre, left_max), base_style));
    } else if status.is_empty() {
        spans.push(Span::styled(left_pre.to_string(), base_style));
    } else {
        // Pre-status fits; budget the rest for the status text + its leading separator.
        let separator = "    ";
        let separator_w = separator.width();
        let remaining = left_max.saturating_sub(left_pre_w + separator_w);
        let status_text = if status.text.width() <= remaining {
            status.text.clone()
        } else {
            truncate_to_width(&status.text, remaining)
        };
        spans.push(Span::styled(left_pre.to_string(), base_style));
        spans.push(Span::styled(separator.to_string(), base_style));
        spans.push(Span::styled(status_text, status_message_style(status)));
    }

    let used_w: usize = spans.iter().map(|s| s.content.width()).sum();
    let pad_w = total_width.saturating_sub(used_w + right_w);
    spans.push(Span::styled(" ".repeat(pad_w), base_style));
    spans.push(Span::styled(right.to_string(), base_style));
    spans
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
    // when the input row is focused; on a root row the cursor is hidden (no `set_cursor_position`
    // call → ratatui hides it for this frame).
    if let Some(settings) = state.project_settings.as_ref() {
        if settings.selected == settings.roots.len() {
            place_settings_input_cursor(f, settings, buffer_area);
        }
        return;
    }
    let ed = state.ed();
    if matches!(ed.mode, EditorMode::Search)
        && state.save_prompt.is_none()
        && !state.picker.open
    {
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
        // Place the cursor inside the picker overlay's input row, at the current insertion
        // point within the query (or at the start, on the placeholder, when empty). For the
        // Explorer picker we offset by the dir-context prefix width — the prefix sits before
        // the typed query and the cursor needs to land after it.
        let box_area = picker_box_rect(buffer_area);
        if box_area.width >= 4 && box_area.height >= 4 {
            // Inner = inside the borders; inner padding adds another column on each side.
            let text_x = box_area.x + 2;
            let text_y = box_area.y + 1;
            let text_w = box_area.width.saturating_sub(4);
            let (label_text, path_text) = explorer_input_prefix(state, text_w as usize);
            let prefix_w = (label_text.width() + path_text.width()) as u16;
            let typed_w = state.picker.query.width_to_cursor() as u16;
            let col = text_x
                .saturating_add(prefix_w)
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
    let row = buffer_area.y + visual_row as u16;
    let col = buffer_area
        .x
        .saturating_add(visual_col.min(buffer_area.width.saturating_sub(1)));
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

    let mut visual_offset: u32 = 0;
    for line_idx in top..=cursor.line {
        let local_idx = (line_idx as i64) - (state.ed().window_first_logical_line as i64);
        if local_idx < 0 || local_idx >= state.ed().lines.len() as i64 {
            return None;
        }
        let render = &state.ed().lines[local_idx as usize];
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
        let spans = build_editor_status_spans("[proj] file.rs", &status, "12:5", 30);
        let text = spans_text(&spans);
        assert!(text.starts_with("[proj] file.rs"));
        assert!(text.ends_with("12:5"));
        assert_eq!(spans_total_width(&spans), 30);
    }

    #[test]
    fn editor_status_spans_renders_status_with_color() {
        let status = crate::app::StatusMessage::success("saved (rev 1)");
        let spans = build_editor_status_spans("[proj] file.rs", &status, "12:5", 60);
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
    fn editor_status_spans_truncates_status_first_when_narrow() {
        // total=30, right="12:5" (4) + gap(1) = 5 reserved; left_max=25. left_pre="[proj] file.rs" (14)
        // + separator(4) = 18 used. Status budget = 25 - 18 = 7. So a long status truncates.
        let status = crate::app::StatusMessage::info("a much longer status message");
        let spans = build_editor_status_spans("[proj] file.rs", &status, "12:5", 30);
        let status_span = spans
            .iter()
            .find(|s| s.style.fg == Some(NORD4) && s.content.contains('…'))
            .expect("truncated status span present");
        // Truncated text width should fit within the remaining budget.
        assert!(status_span.content.width() <= 7);
        assert!(status_span.content.ends_with('…'));
        assert_eq!(spans_total_width(&spans), 30);
    }

    #[test]
    fn editor_status_spans_drops_status_when_left_pre_alone_overflows() {
        // total=12, right=4, gap=1 → left_max=7. left_pre="[proj] file.rs" (14) > 7, so it
        // gets truncated and the status is dropped entirely.
        let status = crate::app::StatusMessage::error("save failed: disk full");
        let spans = build_editor_status_spans("[proj] file.rs", &status, "12:5", 12);
        let text = spans_text(&spans);
        // No part of the status text should make it into the rendered line.
        assert!(
            !text.contains("save failed"),
            "status should have been dropped: {text:?}"
        );
        assert!(text.contains('…'));
        assert!(text.ends_with("12:5"));
        assert_eq!(spans_total_width(&spans), 12);
    }
}
