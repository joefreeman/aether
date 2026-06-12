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
use aether_protocol::viewport::{DiffMarker, DiffStage, Window};
use aether_protocol::LogicalPosition;
use iced::advanced::widget::{tree, Tree};
use iced::advanced::{layout, mouse, renderer, text, Clipboard, Layout, Shell, Widget};
use iced::keyboard;
use iced::{Color, Element, Event, Length, Point, Rectangle, Size};

/// Breathing room above the first line / below the last, in px (web client's `BUFFER_PAD`).
pub const PAD: f32 = 8.0;
/// Change-bar gutter width, in cells (TUI's `GUTTER_WIDTH`).
pub const GUTTER_COLS: u32 = 1;

const CONTINUATION_MARKER: &str = "↪ ";

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
    Layout { cell: Size, size: Size },
    /// Wheel/trackpad scroll; positive = content scrolls down / right.
    Wheel { delta_px: f32, delta_x_px: f32 },
    Pressed { row: i64, dcol: u32, kind: ClickKind, shift: bool },
    Dragged { row: i64, dcol: u32 },
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
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for EditorView<'a, Message>
where
    Renderer: text::Renderer,
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
                if let (Some(cell), Some(position)) = (state.cell, cursor.position_over(bounds)) {
                    let click =
                        mouse::Click::new(position, mouse::Button::Left, state.last_click);
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
                if state.dragging {
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
                if state.dragging {
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
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
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
        let text_x = |dcol: u32| {
            bounds.x + (GUTTER_COLS + dcol) as f32 * cell.width - scroll_x
        };
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
                let text = v.text.replace('\t', &" ".repeat(self.content.tab_width as usize));
                draw_run(renderer, text, Point::new(text_x(0), y), cell, fg, content_clip);
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
                    draw_run(
                        renderer,
                        std::mem::take(run),
                        Point::new(text_x(start), y),
                        cell,
                        color,
                        content_clip,
                    );
                };
                for c in &cells {
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
                // one cell so they're visible).
                for diag in &line.diagnostics {
                    let span = grid::byte_range_span(&cells, diag.start, diag.end.max(diag.start + 1));
                    if let Some((start, end)) = span {
                        fill_content(
                            renderer,
                            Rectangle {
                                x: text_x(start),
                                y: y + cell.height - 2.0,
                                width: (end - start).max(1) as f32 * cell.width,
                                height: 2.0,
                            },
                            theme::diagnostic_color(diag.severity),
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
                    // Re-draw the char under the block in the background colour.
                    if let Some(ch) = char_at(window, cursor_pos) {
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
                }
            }
        }
    }
}

impl<'a, Message> EditorView<'a, Message> {
    /// Pixel position → (absolute visual row, display col).
    fn cell_at(&self, position: Point, bounds: Rectangle, cell: Size) -> (i64, u32) {
        let row = ((self.content.scroll_px + (position.y - bounds.y) - PAD) / cell.height).floor()
            as i64;
        let col = ((position.x - bounds.x + self.content.scroll_x_px) / cell.width).floor()
            as i64
            - GUTTER_COLS as i64;
        (row, col.max(0) as u32)
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer> From<EditorView<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer + 'a,
{
    fn from(editor: EditorView<'a, Message>) -> Self {
        Element::new(editor)
    }
}

fn measure_cell<Renderer: text::Renderer>(renderer: &Renderer) -> Size {
    use iced::advanced::text::Paragraph as _;
    let paragraph = Renderer::Paragraph::with_text(text::Text {
        content: "M",
        bounds: Size::INFINITE,
        size: renderer.default_size(),
        line_height: EDITOR_LINE_HEIGHT,
        font: renderer.default_font(),
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

fn draw_run<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    content: String,
    position: Point,
    cell: Size,
    color: Color,
    clip: Rectangle,
) {
    renderer.fill_text(
        text::Text {
            content,
            bounds: Size::new(f32::INFINITY, cell.height),
            size: renderer.default_size(),
            line_height: EDITOR_LINE_HEIGHT,
            font: renderer.default_font(),
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
        },
        position,
        color,
        clip,
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
