//! The buffer view: a custom widget that paints the server's render model onto a monospace
//! grid and reports input in grid terms.
//!
//! Pixels never leave this widget: it measures the cell size from the renderer's monospace
//! font, converts mouse positions to `(visual row, display col)` cells, and publishes
//! [`EditorEvent`]s; the app maps cells to buffer positions with `grid` and the loaded
//! `Window`. Scrolling is native-feel: the app owns a pixel offset into the full document
//! height (`total_visual_rows × cell height`), this widget just draws the loaded window at its
//! absolute position under that offset — the same virtual-scroll model as the web client.

use crate::grid;
use crate::theme;
use aether_protocol::cursor::CursorState;
use aether_protocol::viewport::{
    DiagnosticSeverity, DiffMarker, DiffStage, LogicalLineRender, Window,
};
use aether_protocol::LogicalPosition;
use iced::advanced::widget::{tree, Tree};
use iced::advanced::{layout, mouse, renderer, text, Clipboard, Layout, Shell, Widget};
use iced::keyboard;
use iced::{Color, Element, Event, Font, Length, Point, Rectangle, Size};

/// Breathing room above the first line / below the last, in px (web client's `BUFFER_PAD`).
pub const PAD: f32 = 8.0;
/// Change-bar gutter width, in cells (TUI's `GUTTER_WIDTH`).
pub const GUTTER_COLS: u32 = 1;

/// Scrollbar rail/thumb width in px — matches the picker's `SCROLLBAR_W` for a consistent look.
const SCROLLBAR_W: f32 = 5.0;
/// Smallest the thumb may shrink to on very long files, so it stays grabbable.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

const CONTINUATION_MARKER: &str = "↪ ";

/// The editor's text font — the bundled Fira Code (registered in `Settings.fonts`). Coding
/// ligatures are toggled by the *shaping* mode the code-text runs use ([`text::Shaping::Advanced`]
/// forms them, [`text::Shaping::Basic`] doesn't), keeping the same monospace metrics either way, so
/// the cell grid never shifts. The UI chrome stays on `Font::MONOSPACE`.
const EDITOR_FONT: Font = Font::with_name("Fira Code");

/// Line height for buffer text — the web client's `14px/1.4`; the measured cell height (and
/// therefore every row) includes this spacing.
const EDITOR_LINE_HEIGHT: text::LineHeight = text::LineHeight::Relative(1.4);

/// What the app gives the widget to draw — borrowed views of app state.
pub struct Content<'a> {
    pub window: Option<&'a Window>,
    pub cursor: CursorState,
    pub insert_mode: bool,
    /// A capture is armed (find-char target, leader chord, partially-typed count): the cursor
    /// renders as an underscore, matching the web client and the terminal's `awaiting_key`.
    pub awaiting_key: bool,
    /// The inline diff view is on: changed lines get background tints (and the server sends
    /// phantom deleted rows, which render regardless).
    pub diff_view: bool,
    pub scroll_px: f32,
    /// Horizontal scroll in px — only ever non-zero under `WrapMode::None`.
    pub scroll_x_px: f32,
    /// Cursor-line blame, drawn as dim virtual text after the line: `(line, "author · age")`.
    pub blame: Option<(u32, &'a str)>,
    pub tab_width: u32,
    /// Coding ligatures on: code-text runs shape with [`text::Shaping::Advanced`] (forming `=>`,
    /// `!=`, … from the Fira Code font); off uses `Basic` (no ligatures, same metrics).
    pub ligatures: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickKind {
    Single,
    Double,
    Triple,
}

/// Grid-level input + layout events the widget publishes to the app.
#[derive(Debug, Clone, Copy)]
pub enum EditorEvent {
    /// Cell metrics and content-area size are known (or changed). Fired once at startup and on
    /// every resize — the app's cue to subscribe / resize the server viewport.
    Layout {
        cell: Size,
        size: Size,
    },
    /// Wheel/trackpad scroll; positive = content scrolls down / right.
    Wheel {
        delta_px: f32,
        delta_x_px: f32,
    },
    /// Absolute vertical scroll request from dragging the scrollbar thumb. `offset_px` is the
    /// desired content offset (clamped app-side); applied without scroll easing.
    ScrollTo {
        offset_px: f32,
    },
    Pressed {
        row: i64,
        dcol: u32,
        kind: ClickKind,
        shift: bool,
    },
    Dragged {
        row: i64,
        dcol: u32,
    },
    Released,
}

pub struct EditorView<'a, Message> {
    content: Content<'a>,
    on_event: Box<dyn Fn(EditorEvent) -> Message + 'a>,
}

pub fn editor<'a, Message: 'a>(
    content: Content<'a>,
    on_event: impl Fn(EditorEvent) -> Message + 'a,
) -> EditorView<'a, Message> {
    EditorView {
        content,
        on_event: Box::new(on_event),
    }
}

#[derive(Default)]
struct State {
    cell: Option<Size>,
    published: Option<(Size, Size)>,
    modifiers: keyboard::Modifiers,
    last_click: Option<mouse::Click>,
    dragging: bool,
    last_drag: Option<(i64, u32)>,
    /// Dragging the scrollbar thumb (started by a press in the right-edge band); suppresses
    /// text selection until release.
    scrollbar_drag: bool,
    /// Cursor is over the scrollbar band. Tracked so a redraw can be requested on enter/leave —
    /// a custom widget won't otherwise repaint on plain cursor motion, so the hover highlight
    /// would lag.
    scrollbar_hover: bool,
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for EditorView<'a, Message>
where
    // The editor draws with the bundled Fira Code (`EDITOR_FONT: iced::Font`), so the renderer's
    // font type must be `iced::Font` — true for the app's real renderer.
    Renderer: text::Renderer<Font = Font>,
    // The editor draws its own scrollbar but styles it from the theme's scrollable catalog, so
    // it matches the picker/popover scrollbars (same source of truth) including hover/drag.
    Theme: iced::widget::scrollable::Catalog,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();

        // Measure the monospace cell once, then keep the app's idea of (cell, content size)
        // fresh — this is what drives viewport subscribe/resize.
        if state.cell.is_none() {
            state.cell = Some(measure_cell(renderer));
        }
        if let Some(cell) = state.cell {
            let current = (cell, bounds.size());
            if state.published != Some(current) {
                state.published = Some(current);
                tracing::debug!(?cell, size = ?bounds.size(), "editor layout published");
                shell.publish((self.on_event)(EditorEvent::Layout {
                    cell,
                    size: bounds.size(),
                }));
            }
        }

        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                state.modifiers = *m;
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if let (Some(cell), Some(_)) = (state.cell, cursor.position_over(bounds)) {
                    let (x, y) = match delta {
                        mouse::ScrollDelta::Lines { x, y } => {
                            (x * cell.width * 6.0, y * cell.height * 3.0)
                        }
                        mouse::ScrollDelta::Pixels { x, y } => (*x, *y),
                    };
                    if x != 0.0 || y != 0.0 {
                        shell.publish((self.on_event)(EditorEvent::Wheel {
                            delta_px: -y,
                            delta_x_px: -x,
                        }));
                        shell.capture_event();
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                // A press in the right-edge scrollbar band grabs the thumb instead of starting a
                // text selection: jump the thumb under the cursor, then drag it on move.
                if let Some(position) = cursor.position_over(bounds) {
                    if self.over_scrollbar(bounds, position.x) {
                        if let Some((thumb_h, max_scroll)) = self.scrollbar_metrics(state, bounds) {
                            state.scrollbar_drag = true;
                            let offset_px =
                                self.scroll_offset_for(thumb_h, max_scroll, bounds, position.y);
                            shell.publish((self.on_event)(EditorEvent::ScrollTo { offset_px }));
                            shell.capture_event();
                            return;
                        }
                    }
                }
                if let (Some(cell), Some(position)) = (state.cell, cursor.position_over(bounds)) {
                    let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
                    let kind = match click.kind() {
                        mouse::click::Kind::Single => ClickKind::Single,
                        mouse::click::Kind::Double => ClickKind::Double,
                        mouse::click::Kind::Triple => ClickKind::Triple,
                    };
                    state.last_click = Some(click);
                    state.dragging = true;
                    let (row, dcol) = self.cell_at(position, bounds, cell);
                    state.last_drag = Some((row, dcol));
                    shell.publish((self.on_event)(EditorEvent::Pressed {
                        row,
                        dcol,
                        kind,
                        shift: state.modifiers.shift(),
                    }));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // Track hover over the scrollbar band so the thumb's hover highlight repaints on
                // enter/leave (no redraw happens on bare cursor motion otherwise).
                let hovering = cursor
                    .position_over(bounds)
                    .is_some_and(|p| self.over_scrollbar(bounds, p.x))
                    && self.scrollbar_metrics(state, bounds).is_some();
                if hovering != state.scrollbar_hover {
                    state.scrollbar_hover = hovering;
                    shell.request_redraw();
                }
                if state.scrollbar_drag {
                    // Drag tracks `y` anywhere (the cursor may leave the band horizontally).
                    if let (Some((thumb_h, max_scroll)), Some(position)) =
                        (self.scrollbar_metrics(state, bounds), cursor.position())
                    {
                        let offset_px =
                            self.scroll_offset_for(thumb_h, max_scroll, bounds, position.y);
                        shell.publish((self.on_event)(EditorEvent::ScrollTo { offset_px }));
                    }
                } else if state.dragging {
                    if let (Some(cell), Some(position)) = (state.cell, cursor.position()) {
                        let (row, dcol) = self.cell_at(position, bounds, cell);
                        // Only re-publish when the cell changed — drags otherwise flood the
                        // server with one cursor/set per pixel.
                        if state.last_drag != Some((row, dcol)) {
                            state.last_drag = Some((row, dcol));
                            shell.publish((self.on_event)(EditorEvent::Dragged { row, dcol }));
                        }
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.scrollbar_drag {
                    state.scrollbar_drag = false;
                } else if state.dragging {
                    state.dragging = false;
                    state.last_drag = None;
                    shell.publish((self.on_event)(EditorEvent::Released));
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        // Over the scrollbar band (or actively dragging it) reads as a control, not text.
        if state.scrollbar_drag {
            return mouse::Interaction::Idle;
        }
        if let Some(p) = cursor.position_over(bounds) {
            if self.over_scrollbar(bounds, p.x) && self.scrollbar_metrics(state, bounds).is_some() {
                return mouse::Interaction::Idle;
            }
            return mouse::Interaction::Text;
        }
        mouse::Interaction::None
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        fill(renderer, bounds, theme::NORD0);

        let (Some(cell), Some(window)) = (state.cell, self.content.window) else {
            return;
        };
        let scroll = self.content.scroll_px;
        let cursor_pos = self.content.cursor.position;
        let (sel_min, sel_max) = selection_endpoints(&self.content.cursor);
        let draw_selection = !self.content.cursor.is_point();
        let scroll_x = self.content.scroll_x_px;
        // Code text shapes with ligatures on (`Advanced`) or off (`Basic`); markers/glyphs always
        // use `draw_run` (Advanced). Same Fira Code metrics either way, so the grid is unaffected.
        let text_shaping = if self.content.ligatures {
            text::Shaping::Advanced
        } else {
            text::Shaping::Basic
        };
        let text_x = |dcol: u32| bounds.x + (GUTTER_COLS + dcol) as f32 * cell.width - scroll_x;
        // Under horizontal scroll, content quads/text must not bleed over the gutter column.
        let content_left = bounds.x + GUTTER_COLS as f32 * cell.width;
        let content_clip = Rectangle {
            x: content_left,
            y: bounds.y,
            width: (bounds.width - GUTTER_COLS as f32 * cell.width).max(0.0),
            height: bounds.height,
        };
        let fill_content = |renderer: &mut Renderer, r: Rectangle, c: Color| {
            if let Some(r) = clamp_left(r, content_left) {
                fill(renderer, r, c);
            }
        };

        let mut abs_row = window.first_visual_row;
        for line in &window.lines {
            // Phantom deleted rows (inline diff view): the baseline content a hunk removed,
            // rendered above the line, never holding the cursor.
            for v in &line.virtual_rows_above {
                let y = bounds.y + PAD + abs_row as f32 * cell.height - scroll;
                abs_row += 1;
                if y + cell.height < bounds.y || y > bounds.y + bounds.height {
                    continue;
                }
                let staged = v.stage == DiffStage::Staged;
                let (bg, fg, bar) = if staged {
                    (
                        theme::GIT_STAGED_DELETED_BG,
                        theme::GIT_STAGED_DELETED,
                        theme::GIT_STAGED_DELETED,
                    )
                } else {
                    (theme::GIT_DELETED_BG, theme::NORD11, theme::NORD11)
                };
                fill(
                    renderer,
                    Rectangle {
                        x: bounds.x,
                        y,
                        width: bounds.width,
                        height: cell.height,
                    },
                    bg,
                );
                fill(
                    renderer,
                    Rectangle {
                        x: bounds.x,
                        y,
                        width: cell.width * 0.5,
                        height: cell.height,
                    },
                    bar,
                );
                let text = v
                    .text
                    .replace('\t', &" ".repeat(self.content.tab_width as usize));
                draw_text_run(
                    renderer,
                    text,
                    Point::new(text_x(0), y),
                    cell,
                    fg,
                    content_clip,
                    text_shaping,
                );
            }

            let n_rows = line.visual_rows.len();
            for (row_idx, row) in line.visual_rows.iter().enumerate() {
                let y = bounds.y + PAD + abs_row as f32 * cell.height - scroll;
                abs_row += 1;
                if y + cell.height < bounds.y || y > bounds.y + bounds.height {
                    continue;
                }
                let row_bounds = Rectangle {
                    x: bounds.x,
                    y,
                    width: bounds.width,
                    height: cell.height,
                };

                let cells = grid::row_cells(row, self.content.tab_width);

                // Line background, under everything else: the diff-view change tint, the
                // cursor-line tint, or — on the cursor's changed line — the variant that keeps
                // the change colour visible (web's `.row.cursor-line.added-bg` precedence).
                let on_cursor_line = line.logical_line == cursor_pos.line;
                let staged = line.diff_stage == DiffStage::Staged;
                let diff_bg = if self.content.diff_view {
                    match (line.diff_marker, staged) {
                        (Some(DiffMarker::Added), false) => Some(theme::GIT_ADDED_BG),
                        (Some(DiffMarker::Modified), false) => Some(theme::GIT_MODIFIED_BG),
                        (Some(DiffMarker::Added), true) => Some(theme::GIT_STAGED_ADDED_BG),
                        (Some(DiffMarker::Modified), true) => Some(theme::GIT_STAGED_MODIFIED_BG),
                        _ => None,
                    }
                } else {
                    None
                };
                let row_bg = match (on_cursor_line, diff_bg, line.diff_marker, staged) {
                    (false, bg, ..) => bg,
                    (true, None, ..) => Some(theme::CURSOR_LINE_BG),
                    (true, Some(_), Some(DiffMarker::Added), false) => {
                        Some(theme::CURSOR_LINE_ADDED_BG)
                    }
                    (true, Some(_), Some(DiffMarker::Modified), false) => {
                        Some(theme::CURSOR_LINE_MODIFIED_BG)
                    }
                    (true, Some(_), Some(DiffMarker::Added), true) => {
                        Some(theme::CURSOR_LINE_STAGED_ADDED_BG)
                    }
                    (true, Some(_), Some(DiffMarker::Modified), true) => {
                        Some(theme::CURSOR_LINE_STAGED_MODIFIED_BG)
                    }
                    (true, Some(_), ..) => Some(theme::CURSOR_LINE_BG),
                };
                if let Some(bg) = row_bg {
                    fill(renderer, row_bounds, bg);
                }

                // Search-match fills: a quiet NORD3, under selection and cursor (matching the
                // web's search-hit < selection < cursor stacking). The spans are kept — the
                // text pass below lifts NORD3-coloured glyphs inside them.
                let hit_spans: Vec<(u32, u32)> = line
                    .search_matches
                    .iter()
                    .filter_map(|m| grid::byte_range_span(&cells, m.start, m.end))
                    .collect();
                for &(start, end) in &hit_spans {
                    fill_content(
                        renderer,
                        Rectangle {
                            x: text_x(start),
                            y,
                            width: (end - start) as f32 * cell.width,
                            height: cell.height,
                        },
                        theme::NORD3,
                    );
                }

                // Gutter change-bar. Under the diff view, removed lines render as phantom
                // rows above, so no Deleted marker is needed on the anchor line.
                let gutter_marker = line
                    .diff_marker
                    .filter(|m| !(self.content.diff_view && *m == DiffMarker::Deleted));
                if let Some(marker) = gutter_marker {
                    let color = gutter_color(marker, line.diff_stage);
                    if marker == DiffMarker::Deleted {
                        // A pure deletion sits *between* this surviving line and the one above,
                        // so mark it with a small triangle straddling this line's top boundary
                        // rather than a full-height bar — a bar reads as "this line changed",
                        // which it didn't. Mirrors the web's `.gutter.deleted::before`.
                        fill_triangle_right(renderer, bounds.x, y, color);
                    } else {
                        fill(
                            renderer,
                            Rectangle {
                                x: bounds.x,
                                y,
                                width: cell.width * 0.5,
                                height: cell.height,
                            },
                            color,
                        );
                    }
                }

                // Selection: the saturated NORD10 blue, over the NORD3 search-hit fill
                // (terminal/web parity — hit < selection < cursor).
                if draw_selection {
                    if let Some((start, end)) = grid::row_selection_span(
                        line.logical_line,
                        row,
                        row_idx + 1 == n_rows,
                        sel_min,
                        sel_max,
                        self.content.tab_width,
                    ) {
                        fill_content(
                            renderer,
                            Rectangle {
                                x: text_x(start),
                                y,
                                width: (end - start) as f32 * cell.width,
                                height: cell.height,
                            },
                            theme::NORD10,
                        );
                    }
                }

                // Match-bracket highlight cells.
                if let Some((open, close)) = self.content.cursor.match_bracket {
                    for pos in [open, close] {
                        if pos.line == line.logical_line {
                            if let Some((r, dcol, width)) =
                                grid::position_cell(window, pos, self.content.tab_width)
                            {
                                if r + 1 == abs_row {
                                    fill_content(
                                        renderer,
                                        Rectangle {
                                            x: text_x(dcol),
                                            y,
                                            width: width as f32 * cell.width,
                                            height: cell.height,
                                        },
                                        theme::NORD3,
                                    );
                                }
                            }
                        }
                    }
                }

                // Continuation marker.
                if row.byte_offset > 0 {
                    draw_run(
                        renderer,
                        CONTINUATION_MARKER.to_string(),
                        Point::new(text_x(0), y),
                        cell,
                        theme::NORD3,
                        content_clip,
                    );
                }

                // Text, as runs of identical highlight kind. "Inside a search hit" is part
                // of the run key: comments are themed NORD3 — the same shade as the hit
                // fill — so a match inside one would be invisible. Lift just that text to
                // the normal foreground (the web's `.search-hit.hl-comment` rule; every
                // other syntax colour reads fine on NORD3).
                let in_hit = |dcol: u32| hit_spans.iter().any(|&(s, e)| dcol >= s && dcol < e);
                let mut run = String::new();
                let mut run_start: u32 = 0;
                let mut run_kind: Option<&str> = None;
                let mut run_hit = false;
                let flush = |run: &mut String,
                             start: u32,
                             kind: Option<&str>,
                             hit: bool,
                             renderer: &mut Renderer| {
                    if run.is_empty() {
                        return;
                    }
                    let mut color = kind
                        .and_then(theme::highlight_color)
                        .unwrap_or(theme::NORD4);
                    if hit && color == theme::NORD3 {
                        color = theme::NORD4;
                    }
                    draw_text_run(
                        renderer,
                        std::mem::take(run),
                        Point::new(text_x(start), y),
                        cell,
                        color,
                        content_clip,
                        text_shaping,
                    );
                };
                // Byte offset where the row's trailing whitespace run begins (row end if none) —
                // only spaces from here on get the `·` glyph, matching the terminal client.
                let trailing_ws_start = {
                    let mut start = grid::row_end_byte(row);
                    for c in cells.iter().rev() {
                        if c.ch == ' ' || c.ch == '\t' {
                            start = c.byte;
                        } else {
                            break;
                        }
                    }
                    start
                };
                for c in &cells {
                    // Selected whitespace gets a muted NORD3 indicator glyph over the selection
                    // fill (terminal parity): `→` for tabs, `·` for trailing spaces. Drawn as its
                    // own run so the next text run repositions itself past the tab's full width.
                    let selected = draw_selection
                        && pos_in_selection(line.logical_line, c.byte, sel_min, sel_max);
                    let glyph = if selected && c.ch == '\t' {
                        Some("→")
                    } else if selected && c.ch == ' ' && c.byte >= trailing_ws_start {
                        Some("·")
                    } else {
                        None
                    };
                    if let Some(g) = glyph {
                        flush(&mut run, run_start, run_kind, run_hit, renderer);
                        draw_run(
                            renderer,
                            g.to_string(),
                            Point::new(text_x(c.dcol), y),
                            cell,
                            theme::NORD3,
                            content_clip,
                        );
                        continue;
                    }
                    let hit = in_hit(c.dcol);
                    if run.is_empty() {
                        run_start = c.dcol;
                        run_kind = c.kind;
                        run_hit = hit;
                    } else if c.kind != run_kind || hit != run_hit {
                        flush(&mut run, run_start, run_kind, run_hit, renderer);
                        run_start = c.dcol;
                        run_kind = c.kind;
                        run_hit = hit;
                    }
                    if c.ch == '\t' {
                        for _ in 0..c.width {
                            run.push(' ');
                        }
                    } else {
                        run.push(c.ch);
                    }
                }
                flush(&mut run, run_start, run_kind, run_hit, renderer);

                // Selected newline: a muted `↵` at the line's end on its last visual row, when the
                // consumed `\n` falls in the selection (terminal parity). The fill ensures the
                // selection blue sits under the glyph even when the selection ends exactly on it
                // (where `row_selection_span` stops at the last char).
                let nl_selected = draw_selection
                    && row_idx + 1 == n_rows
                    && line.logical_line >= sel_min.line
                    && (line.logical_line < sel_max.line
                        || (line.logical_line == sel_max.line
                            && sel_max.col >= grid::row_end_byte(row)));
                if nl_selected {
                    let end_dcol = cells
                        .last()
                        .map(|c| c.dcol + c.width)
                        .unwrap_or_else(|| grid::row_prefix_cols(row));
                    fill_content(
                        renderer,
                        Rectangle {
                            x: text_x(end_dcol),
                            y,
                            width: cell.width,
                            height: cell.height,
                        },
                        theme::NORD10,
                    );
                    draw_run(
                        renderer,
                        "↵".to_string(),
                        Point::new(text_x(end_dcol), y),
                        cell,
                        theme::NORD3,
                        content_clip,
                    );
                }

                // Cursor-line blame: dim virtual text after the line's last row.
                if let Some((bline, btext)) = self.content.blame {
                    if line.logical_line == bline && row_idx + 1 == n_rows {
                        let end = cells.last().map(|c| c.dcol + c.width).unwrap_or(0);
                        draw_run(
                            renderer,
                            btext.to_string(),
                            Point::new(text_x(end + 3), y),
                            cell,
                            theme::NORD3_BRIGHT,
                            content_clip,
                        );
                    }
                }

                // Diagnostics underline: 2px line under the span (zero-width ones widened to
                // one cell so they're visible). A diagnostic clamped to the line end (e.g.
                // "expected ;") has no real char to mark, so underline the virtual EOL cell —
                // where the newline glyph sits — on the line's last visual row.
                let row_end = grid::row_end_byte(row);
                let eol_dcol = cells
                    .last()
                    .map(|c| c.dcol + c.width)
                    .unwrap_or_else(|| grid::row_prefix_cols(row));
                for diag in &line.diagnostics {
                    let span = if row_idx + 1 == n_rows && diag.start >= row_end {
                        Some((eol_dcol, eol_dcol + 1))
                    } else {
                        grid::byte_range_span(&cells, diag.start, diag.end.max(diag.start + 1))
                    };
                    if let Some((start, end)) = span {
                        fill_wavy(
                            renderer,
                            text_x(start),
                            y + cell.height - 2.0,
                            (end - start).max(1) as f32 * cell.width,
                            theme::diagnostic_color(diag.severity),
                            content_left,
                        );
                    }
                }
            }
        }

        // Cursor, on top.
        if let Some((row, dcol, width)) =
            grid::position_cell(window, cursor_pos, self.content.tab_width)
        {
            let y = bounds.y + PAD + row as f32 * cell.height - scroll;
            if y + cell.height >= bounds.y && y <= bounds.y + bounds.height {
                let x = text_x(dcol);
                // Underscore while a capture is armed takes precedence over insert/normal.
                if self.content.awaiting_key {
                    fill_content(
                        renderer,
                        Rectangle {
                            x,
                            y: y + cell.height - 2.0,
                            width: width as f32 * cell.width,
                            height: 2.0,
                        },
                        theme::NORD4,
                    );
                } else if self.content.insert_mode {
                    fill_content(
                        renderer,
                        Rectangle {
                            x,
                            y,
                            width: 2.0,
                            height: cell.height,
                        },
                        theme::NORD8,
                    );
                } else {
                    fill_content(
                        renderer,
                        Rectangle {
                            x,
                            y,
                            width: width as f32 * cell.width,
                            height: cell.height,
                        },
                        theme::NORD4,
                    );
                    // Re-draw the char — or, on selected whitespace, its `→`/`·`/`↵` glyph — under
                    // the block in the background colour, so the cursor doesn't blank the indicator.
                    let ws = draw_selection
                        .then(|| {
                            cursor_ws_glyph(
                                window,
                                cursor_pos,
                                sel_min,
                                sel_max,
                                self.content.tab_width,
                            )
                        })
                        .flatten();
                    if let Some(g) = ws {
                        draw_run(
                            renderer,
                            g.to_string(),
                            Point::new(x, y),
                            cell,
                            theme::NORD0,
                            content_clip,
                        );
                    } else if let Some(ch) = char_at(window, cursor_pos) {
                        if ch != '\t' {
                            draw_run(
                                renderer,
                                ch.to_string(),
                                Point::new(x, y),
                                cell,
                                theme::NORD0,
                                content_clip,
                            );
                        }
                    }
                    // The block covered the row pass's diagnostic underline; redraw it on top so a
                    // diagnostic on the cursor's char stays visible (the web draws the wavy line
                    // over the cursor background).
                    if let Some(sev) = window
                        .lines
                        .iter()
                        .find(|l| l.logical_line == cursor_pos.line)
                        .and_then(|l| diagnostic_at(l, cursor_pos.col))
                    {
                        fill_wavy(
                            renderer,
                            x,
                            y + cell.height - 2.0,
                            width as f32 * cell.width,
                            theme::diagnostic_color(sev),
                            content_left,
                        );
                    }
                }
            }
        }

        // Editor scrollbar: a thin overlay at the right edge, shown only when the document is
        // taller than the viewport. Geometry from the shared `scrollbar::thumb` (same as the TUI
        // and picker); appearance pulled from the theme's scrollable catalog — the exact style
        // the picker/popover scrollbars use, so they match including hover/drag highlighting.
        let content_h = PAD * 2.0 + window.total_visual_rows as f32 * cell.height;
        if let Some((thumb_y, thumb_h)) = crate::core::scrollbar::thumb(
            bounds.height as f64,
            content_h as f64,
            bounds.height as f64,
            scroll as f64,
            SCROLLBAR_MIN_THUMB as f64,
        ) {
            let rail_x = bounds.x + bounds.width - SCROLLBAR_W;
            // `scrollbar_hover` is tracked in `update` (which also requests the repaint); fall back
            // to the live cursor so the very first frame after layout is correct too.
            let hovered = state.scrollbar_hover
                || cursor
                    .position_over(bounds)
                    .is_some_and(|p| self.over_scrollbar(bounds, p.x));
            let rail = scrollbar_rail(theme, state.scrollbar_drag, hovered);

            // Rail background (drawn only when the theme gives the track a fill).
            if let Some(bg) = rail.background {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: rail_x,
                            y: bounds.y,
                            width: SCROLLBAR_W,
                            height: bounds.height,
                        },
                        border: rail.border,
                        ..renderer::Quad::default()
                    },
                    bg,
                );
            }
            // Thumb.
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: rail_x,
                        y: bounds.y + thumb_y as f32,
                        width: SCROLLBAR_W,
                        height: thumb_h as f32,
                    },
                    border: rail.scroller.border,
                    ..renderer::Quad::default()
                },
                rail.scroller.background,
            );
        }
    }
}

/// The vertical scrollbar [`Rail`](iced::widget::scrollable::Rail) style for the given
/// interaction state, read from the theme's scrollable catalog — the same default style the
/// picker's `scrollable` uses, so the editor's hand-drawn bar matches it (track/thumb colours,
/// border radius, and the hover/drag accent).
fn scrollbar_rail<Theme: iced::widget::scrollable::Catalog>(
    theme: &Theme,
    dragged: bool,
    hovered: bool,
) -> iced::widget::scrollable::Rail {
    use iced::widget::scrollable::{Catalog, Status};
    let status = if dragged {
        Status::Dragged {
            is_horizontal_scrollbar_dragged: false,
            is_vertical_scrollbar_dragged: true,
            is_horizontal_scrollbar_disabled: true,
            is_vertical_scrollbar_disabled: false,
        }
    } else if hovered {
        Status::Hovered {
            is_horizontal_scrollbar_hovered: false,
            is_vertical_scrollbar_hovered: true,
            is_horizontal_scrollbar_disabled: true,
            is_vertical_scrollbar_disabled: false,
        }
    } else {
        Status::Active {
            is_horizontal_scrollbar_disabled: true,
            is_vertical_scrollbar_disabled: false,
        }
    };
    let class = <Theme as Catalog>::default();
    <Theme as Catalog>::style(theme, &class, status).vertical_rail
}

impl<'a, Message> EditorView<'a, Message> {
    /// Pixel position → (absolute visual row, display col).
    fn cell_at(&self, position: Point, bounds: Rectangle, cell: Size) -> (i64, u32) {
        let row =
            ((self.content.scroll_px + (position.y - bounds.y) - PAD) / cell.height).floor() as i64;
        let col = ((position.x - bounds.x + self.content.scroll_x_px) / cell.width).floor() as i64
            - GUTTER_COLS as i64;
        (row, col.max(0) as u32)
    }

    /// Scrollbar thumb height and max scroll offset (px), or `None` when the document fits and
    /// no bar is shown. The thumb geometry uses the shared [`scrollbar::thumb`] so it tracks the
    /// TUI and picker; this returns just the pieces the drag math needs.
    fn scrollbar_metrics(&self, state: &State, bounds: Rectangle) -> Option<(f32, f32)> {
        let (cell, window) = (state.cell?, self.content.window?);
        let content_h = PAD * 2.0 + window.total_visual_rows as f32 * cell.height;
        let (_, thumb_h) = crate::core::scrollbar::thumb(
            bounds.height as f64,
            content_h as f64,
            bounds.height as f64,
            self.content.scroll_px as f64,
            SCROLLBAR_MIN_THUMB as f64,
        )?;
        Some((thumb_h as f32, (content_h - bounds.height).max(0.0)))
    }

    /// Whether `x` falls in the right-edge scrollbar band (only meaningful when a bar shows).
    fn over_scrollbar(&self, bounds: Rectangle, x: f32) -> bool {
        x >= bounds.x + bounds.width - SCROLLBAR_W
    }

    /// Content offset (px) that centres the thumb under cursor `y` while dragging.
    fn scroll_offset_for(&self, thumb_h: f32, max_scroll: f32, bounds: Rectangle, y: f32) -> f32 {
        let travel = bounds.height - thumb_h;
        if travel <= 0.0 {
            return 0.0;
        }
        let frac = ((y - bounds.y - thumb_h / 2.0) / travel).clamp(0.0, 1.0);
        frac * max_scroll
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<EditorView<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer<Font = Font> + 'a,
    Theme: iced::widget::scrollable::Catalog,
{
    fn from(editor: EditorView<'a, Message>) -> Self {
        Element::new(editor)
    }
}

fn measure_cell<Renderer: text::Renderer<Font = Font>>(renderer: &Renderer) -> Size {
    use iced::advanced::text::Paragraph as _;
    let paragraph = Renderer::Paragraph::with_text(text::Text {
        content: "M",
        bounds: Size::INFINITE,
        size: renderer.default_size(),
        line_height: EDITOR_LINE_HEIGHT,
        font: EDITOR_FONT,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
    });
    paragraph.min_bounds()
}

/// Clamp a rect's left edge to `left`, shrinking its width; `None` when fully clipped.
fn clamp_left(mut r: Rectangle, left: f32) -> Option<Rectangle> {
    if r.x < left {
        let d = left - r.x;
        if d >= r.width {
            return None;
        }
        r.x = left;
        r.width -= d;
    }
    Some(r)
}

fn fill<Renderer: renderer::Renderer>(renderer: &mut Renderer, bounds: Rectangle, color: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            ..renderer::Quad::default()
        },
        color,
    );
}

/// A wavy underline — the browser's `text-decoration: underline wavy`, which the web client uses
/// for diagnostics. The renderer only fills axis-aligned rects, so trace a sine with a row of
/// 1px-wide quads. `mid_y` is the wave's vertical centre; segments left of `clip_left` (the
/// gutter) are dropped, mirroring `fill_content`.
fn fill_wavy<Renderer: renderer::Renderer>(
    renderer: &mut Renderer,
    x: f32,
    mid_y: f32,
    width: f32,
    color: Color,
    clip_left: f32,
) {
    const PERIOD: f32 = 4.0; // px per full cycle
    const AMP: f32 = 1.1; // peak offset from the centre line
    const TH: f32 = 1.4; // stroke thickness
    let mut dx = 0.0;
    while dx < width {
        let off = AMP * (std::f32::consts::TAU * dx / PERIOD).sin();
        let seg = (width - dx).min(1.0);
        if let Some(r) = clamp_left(
            Rectangle {
                x: x + dx,
                y: mid_y + off - TH / 2.0,
                width: seg,
                height: TH,
            },
            clip_left,
        ) {
            fill(renderer, r, color);
        }
        dx += 1.0;
    }
}

/// A small right-pointing triangle, its vertical base flush against the gutter's left edge and its
/// tip pointing toward the text — the web client's `.gutter.deleted::before`. Marks a pure deletion
/// when the diff view is off: the removed lines sit "between" this line and the one above, so the
/// marker centres on the line's top boundary (`mid_y`) instead of filling the surviving line. The
/// renderer only fills axis-aligned rects, so build the taper from a stack of 1px rows.
fn fill_triangle_right<Renderer: renderer::Renderer>(
    renderer: &mut Renderer,
    x: f32,
    mid_y: f32,
    color: Color,
) {
    const W: f32 = 6.0; // tip distance from the base (web's `border-left` width)
    const H: f32 = 8.0; // base height (web's `border-top` + `border-bottom`)
    let half = H / 2.0;
    let mut dy = -half;
    while dy < half {
        // Width tapers linearly from W at the centre to 0 at the top/bottom tips; sample at the
        // row's midpoint so the taper stays symmetric about `mid_y`.
        let w = W * (1.0 - ((dy + 0.5).abs() / half)).max(0.0);
        if w > 0.0 {
            fill(
                renderer,
                Rectangle {
                    x,
                    y: mid_y + dy,
                    width: w,
                    height: 1.0,
                },
                color,
            );
        }
        dy += 1.0;
    }
}

fn severity_rank(s: DiagnosticSeverity) -> u8 {
    use DiagnosticSeverity as S;
    match s {
        S::Error => 3,
        S::Warning => 2,
        S::Information => 1,
        S::Hint => 0,
    }
}

/// The worst-severity diagnostic covering byte `col` on `line` (zero-width ones widened one cell,
/// so a diagnostic clamped to the line end still matches the cursor parked there). Drives the
/// over-the-cursor underline redraw.
fn diagnostic_at(line: &LogicalLineRender, col: u32) -> Option<DiagnosticSeverity> {
    line.diagnostics
        .iter()
        .filter(|d| d.start <= col && col < d.end.max(d.start + 1))
        .map(|d| d.severity)
        .max_by_key(|s| severity_rank(*s))
}

/// Draw a run of editor text with an explicit shaping mode. Code-text runs pass the ligature-driven
/// shaping (`Advanced` forms ligatures, `Basic` doesn't); single-glyph markers use [`draw_run`].
fn draw_text_run<Renderer: text::Renderer<Font = Font>>(
    renderer: &mut Renderer,
    content: String,
    position: Point,
    cell: Size,
    color: Color,
    clip: Rectangle,
    shaping: text::Shaping,
) {
    renderer.fill_text(
        text::Text {
            content,
            bounds: Size::new(f32::INFINITY, cell.height),
            size: renderer.default_size(),
            line_height: EDITOR_LINE_HEIGHT,
            font: EDITOR_FONT,
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping,
            wrapping: text::Wrapping::None,
        },
        position,
        color,
        clip,
    );
}

/// Draw a run that doesn't need ligature control (continuation markers, whitespace glyphs, blame) —
/// always fully shaped.
fn draw_run<Renderer: text::Renderer<Font = Font>>(
    renderer: &mut Renderer,
    content: String,
    position: Point,
    cell: Size,
    color: Color,
    clip: Rectangle,
) {
    draw_text_run(
        renderer,
        content,
        position,
        cell,
        color,
        clip,
        text::Shaping::Advanced,
    );
}

fn selection_endpoints(cursor: &CursorState) -> (LogicalPosition, LogicalPosition) {
    let a = cursor.anchor;
    let b = cursor.position;
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Whether a char at `(line, byte)` falls inside the inclusive selection `[min, max]` — the
/// per-cell test behind the selected-whitespace glyphs.
fn pos_in_selection(line: u32, byte: u32, min: LogicalPosition, max: LogicalPosition) -> bool {
    let after_min = line > min.line || (line == min.line && byte >= min.col);
    let before_max = line < max.line || (line == max.line && byte <= max.col);
    after_min && before_max
}

/// The whitespace glyph to redraw (inverted) under the block cursor when it sits on selected
/// whitespace — the cursor is painted last, over the text, so without this it would blank the
/// `→`/`·`/`↵` the text pass drew. Mirrors the per-row glyph rules. `None` when the cursor cell
/// isn't selected whitespace.
fn cursor_ws_glyph(
    window: &Window,
    pos: LogicalPosition,
    min: LogicalPosition,
    max: LogicalPosition,
    tab_width: u32,
) -> Option<&'static str> {
    if !pos_in_selection(pos.line, pos.col, min, max) {
        return None;
    }
    let line = window.lines.iter().find(|l| l.logical_line == pos.line)?;
    let row_idx = line
        .visual_rows
        .iter()
        .rposition(|r| r.byte_offset <= pos.col)?;
    let row = &line.visual_rows[row_idx];
    let cells = grid::row_cells(row, tab_width);
    match cells.iter().find(|c| c.byte == pos.col) {
        Some(c) if c.ch == '\t' => Some("→"),
        Some(c) if c.ch == ' ' => {
            // Only a space in the row's trailing-whitespace run gets the dot.
            let mut start = grid::row_end_byte(row);
            for cc in cells.iter().rev() {
                if cc.ch == ' ' || cc.ch == '\t' {
                    start = cc.byte;
                } else {
                    break;
                }
            }
            (c.byte >= start).then_some("·")
        }
        Some(_) => None,
        // Past the row's last char: the consumed-newline cell, on the line's last row.
        None => (row_idx + 1 == line.visual_rows.len()).then_some("↵"),
    }
}

fn gutter_color(marker: DiffMarker, stage: DiffStage) -> Color {
    match (marker, stage) {
        (DiffMarker::Added, DiffStage::Unstaged) => theme::GIT_ADDED,
        (DiffMarker::Modified, DiffStage::Unstaged) => theme::GIT_MODIFIED,
        (DiffMarker::Deleted, DiffStage::Unstaged) => theme::GIT_DELETED,
        (DiffMarker::Added, DiffStage::Staged) => theme::GIT_STAGED_ADDED,
        (DiffMarker::Modified, DiffStage::Staged) => theme::GIT_STAGED_MODIFIED,
        (DiffMarker::Deleted, DiffStage::Staged) => theme::GIT_STAGED_DELETED,
    }
}

/// The char at a position, for re-drawing under the block cursor.
fn char_at(window: &Window, pos: LogicalPosition) -> Option<char> {
    let line = window.lines.iter().find(|l| l.logical_line == pos.line)?;
    let row = line
        .visual_rows
        .iter()
        .rev()
        .find(|r| r.byte_offset <= pos.col)?;
    let mut byte = row.byte_offset;
    for segment in &row.segments {
        for ch in segment.text.chars() {
            if byte == pos.col {
                return Some(ch);
            }
            byte += ch.len_utf8() as u32;
        }
    }
    None
}
