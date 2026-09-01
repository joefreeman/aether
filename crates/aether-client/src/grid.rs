//! Pure mapping between protocol coordinates and the monospace cell grid.
//!
//! The server renders a `Window` of logical lines, each split into `WrappedRow`s by its soft-wrap
//! math; positions on the wire are `(logical line, byte col)`. Everything pixel-ish in the client
//! reduces to *(absolute visual row, display column)* cells, so this module owns that translation:
//! cursor → cell, mouse cell → position, selection → per-row display-column spans. Display-column
//! math mirrors the server's: tabs advance to the next `tab_width` stop, other chars take their
//! Unicode width. Continuation rows are prefixed by the wrap marker ("↪ ") plus the row's
//! continuation indent, same as the web client.

use std::collections::HashMap;

use aether_protocol::coords::{ViewLine, VisualRow};
use aether_protocol::viewport::{
    BaselineRow, Element, FieldId, LogicalLineRender, Window, WrappedRow,
};
use aether_protocol::LogicalPosition;
use unicode_width::UnicodeWidthChar;

/// Display cols the "↪ " wrap marker occupies on continuation rows (mirrors the web client's
/// `CONTINUATION_MARKER_WIDTH`).
pub const CONTINUATION_MARKER_COLS: u32 = 2;

/// One renderable char of a visual row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell<'a> {
    /// Byte offset within the *logical line*.
    pub byte: u32,
    /// Display column within the visual row (continuation prefix included).
    pub dcol: u32,
    /// Display width in columns (tabs expand to their stop; never 0 so every cell is hittable).
    pub width: u32,
    pub ch: char,
    /// Tree-sitter highlight kind covering this char, if any.
    pub kind: Option<&'a str>,
}

/// Display cols a row's text starts at: 0 for a line's first row, marker + indent for
/// continuation rows.
pub fn row_prefix_cols(row: &WrappedRow) -> u32 {
    if row.byte_offset == 0 {
        0
    } else {
        CONTINUATION_MARKER_COLS + row.continuation_indent
    }
}

/// Walk a visual row's chars as grid cells.
pub fn row_cells(row: &WrappedRow, tab_width: u32) -> Vec<Cell<'_>> {
    let mut cells = Vec::new();
    let mut dcol = row_prefix_cols(row);
    let mut byte = row.byte_offset;
    for segment in &row.segments {
        for (seg_off, ch) in segment.text.char_indices() {
            let seg_off = seg_off as u32;
            let kind = segment
                .highlights
                .iter()
                .find(|h| h.start <= seg_off && seg_off < h.end)
                .map(|h| h.kind.as_str());
            let width = char_width(ch, dcol, tab_width);
            cells.push(Cell {
                byte,
                dcol,
                width,
                ch,
                kind,
            });
            dcol += width;
            byte += ch.len_utf8() as u32;
        }
    }
    cells
}

fn char_width(ch: char, dcol: u32, tab_width: u32) -> u32 {
    if ch == '\t' {
        let tw = tab_width.max(1);
        tw - (dcol % tw)
    } else {
        (ch.width().unwrap_or(1) as u32).max(1)
    }
}

/// Byte offset (within the logical line) just past the row's last char.
pub fn row_end_byte(row: &WrappedRow) -> u32 {
    row.byte_offset
        + row
            .segments
            .iter()
            .map(|s| s.text.len() as u32)
            .sum::<u32>()
}

/// Visual rows a line occupies: its phantom baseline rows (inline diff view) plus its (possibly
/// wrapped) content rows.
pub fn line_rows(line: &LogicalLineRender) -> u32 {
    (line.baseline_above.len() + line.visual_rows.len()) as u32
}

/// One row-producing item of the view, in order.
///
/// The unit the scroll arithmetic counts in. A chrome row occupies a screen row exactly as a
/// phantom one does — leaving it out makes the scrollbar short, puts the last lines out of reach,
/// and lets the cursor sit below the scrollable area — but it belongs to no logical line, which is
/// why the two cannot simply be summed per line any more.
pub enum RowItem<'a> {
    /// A chrome row: occupies a row, holds no cursor position, has no logical line.
    Chrome,
    /// A rendered line, tagged with the editor element it belongs to.
    ///
    /// The element is what disambiguates it. Once elements window *different* buffers, a logical
    /// line number is unique only within its own element — two hunks from two files both have a
    /// line 12 — so every lookup that used to match on the number alone has to match on the pair.
    Line {
        element: FieldId,
        line: &'a LogicalLineRender,
    },
}

impl RowItem<'_> {
    pub fn rows(&self) -> u32 {
        match self {
            RowItem::Chrome => 1,
            RowItem::Line { line, .. } => line_rows(line),
        }
    }
}

/// The view's row-producing items, top to bottom, flattened across its tree.
pub fn row_items(window: &Window) -> Vec<RowItem<'_>> {
    row_items_of(&window.root)
}

/// [`row_items`] over a bare tree, for shells that keep the root rather than the whole window.
pub fn row_items_of(root: &Element) -> Vec<RowItem<'_>> {
    fn walk<'a>(node: &'a Element, out: &mut Vec<RowItem<'a>>) {
        match node {
            Element::Stack { children } => children.iter().for_each(|c| walk(c, out)),
            Element::Editor { element, lines, .. } => {
                out.extend(lines.iter().map(|line| RowItem::Line {
                    element: *element,
                    line,
                }))
            }
            // A row of generated presentation: chrome, or — now that one vocabulary describes both
            // axes — any inline element standing on its own. One screen row, no cursor position.
            _ => out.push(RowItem::Chrome),
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Rows above the given **view** line, within the loaded window. `None` when that line isn't
/// loaded. Absolute visual row = `window.first_visual_row + this`.
///
/// The view-space counterpart of [`rows_before_line`], for the one caller that legitimately holds a
/// view line: a scroll position. Counting is structural — each rendered line is one view line, in
/// order, and chrome is none — so it needs no knowledge of which buffer any element windows.
pub fn rows_before_view_line(window: &Window, line: ViewLine) -> Option<u32> {
    let mut rows = 0u32;
    let mut at = window.first_view_line;
    for item in row_items(window) {
        if matches!(item, RowItem::Line { .. }) {
            if at == line {
                return Some(rows);
            }
            at = at.saturating_add(1);
        }
        rows += item.rows();
    }
    None
}

/// The first row of the **block** a view line belongs to — the chrome standing above it included.
///
/// What "scroll to this line" means, and the inverse of the row→line direction: a chrome row belongs
/// to the line it introduces (see [`line_at_row`]), so the row a line starts at is the first row of
/// that run, not the line's own text row. Positioning a viewport with [`rows_before_view_line`]
/// instead puts the text at the top and the heading above the fold — which is why a patch opened at
/// its first line hid the very file heading that introduces it.
///
/// `None` when the line isn't in the loaded window.
pub fn block_start_of_view_line(window: &Window, line: ViewLine) -> Option<VisualRow> {
    let mut at = window.first_view_line;
    for (start, _) in line_blocks(window) {
        if at == line {
            return Some(start);
        }
        at = at.saturating_add(1);
    }
    None
}

/// Where a line is **in a view**: which element, and which of that element's buffer lines.
///
/// The pair, always, because half of it means nothing on its own. A logical line number identifies
/// a line only within its element — two files' hunks both have a line 10 — so `l.logical_line ==
/// cursor.line` is a question with two right answers and no way to tell which was meant. Comparing
/// `ElementLine`s asks the whole question.
///
/// This exists because that mistake was made **eight times** in three shells: the cursor-line tint,
/// the block cursor, the blame label, hit-testing, scroll anchoring, reveal. Each was fixed where it
/// was found and the next one was written the same week. The flattening helpers below hand out
/// these rather than bare lines so the incomplete comparison cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementLine {
    pub element: FieldId,
    pub line: u32,
}

impl ElementLine {
    pub fn new(element: FieldId, line: u32) -> Self {
        Self { element, line }
    }
}

/// The element each rendered line belongs to, indexed by its position in the flattened line list.
///
/// The list a painter walks is flat, but a logical line number names a line only *within* its
/// element — two files' hunks both have a line 10. Anything the painter decides by comparing a
/// line's number to the cursor's (the cursor-line tint, the block cursor, the blame label) has to
/// ask this too, or it paints that decision on **both** lines. Two cursors moving in sync is what
/// that looks like.
pub fn elements_by_line_index(root: &Element) -> Vec<FieldId> {
    row_items_of(root)
        .into_iter()
        .filter_map(|i| match i {
            RowItem::Line { element, .. } => Some(element),
            RowItem::Chrome => None,
        })
        .collect()
}

/// The absolute visual row an element's first row sits on, chrome above it included.
///
/// Computable for **every** element, loaded or not: the tree is the whole view, and each editor
/// reports its total height whether or not its lines are in the window. That is what makes it the
/// right target for revealing an element you have just focused — the cursor's own line cannot be
/// located, because the element it moved to has no lines loaded yet.
pub fn element_start_row(window: &Window, element: FieldId) -> Option<VisualRow> {
    fn walk(node: &Element, want: FieldId, at: &mut u32) -> bool {
        match node {
            Element::Stack { children } => children.iter().any(|c| walk(c, want, at)),
            Element::Editor {
                element: id, rows, ..
            } => {
                if *id == want {
                    return true;
                }
                *at += rows;
                false
            }
            // Chrome, and any inline element standing on its own, occupy one row each.
            _ => {
                *at += 1;
                false
            }
        }
    }
    let mut at = 0u32;
    walk(&window.root, element, &mut at).then_some(VisualRow(at))
}

/// The **view** line at which `element`'s buffer line `line` sits, if the window has it loaded.
///
/// The bridge a shell needs when it holds a cursor position — a buffer line — and must name it to
/// something that speaks view lines, like a scroll request. Structural, like
/// [`rows_before_view_line`]: rendered lines are view lines, in order.
///
/// `None` when the line isn't loaded, and callers must not fall back to the number itself: the two
/// spaces coincide only for a single whole-buffer element. A caller that cannot answer should be
/// fetching around the line rather than guessing where it is.
pub fn view_line_of(window: &Window, element: FieldId, line: u32) -> Option<ViewLine> {
    let mut at = window.first_view_line;
    for item in row_items(window) {
        if let RowItem::Line {
            element: e,
            line: l,
        } = item
        {
            if e == element && l.logical_line == line {
                return Some(at);
            }
            at = at.saturating_add(1);
        }
    }
    None
}

/// The **buffer**-line range this window has loaded for `element`, as `first..=last`. `None` when
/// the element has no lines in the window.
///
/// For anything that needs to name lines of the focused element's file — scoping a sneak to what is
/// on screen, say. The window's own `first_view_line`/`last_view_line_exclusive` cannot serve: those
/// are view coordinates, and in a patch they are unrelated to any file's line numbers.
pub fn loaded_line_range(window: &Window, element: FieldId) -> Option<(u32, u32)> {
    let lines: Vec<u32> = window
        .root
        .editors()
        .iter()
        .filter_map(|n| match n {
            Element::Editor {
                element: id, lines, ..
            } if *id == element => Some(lines.iter().map(|l| l.logical_line)),
            _ => None,
        })
        .flatten()
        .collect();
    Some((*lines.iter().min()?, *lines.iter().max()?))
}

/// The view's rendered lines, top to bottom — for the paths that want every line and no structure.
/// Whether `logical_line` of `element` is among the lines this window actually carries.
///
/// The question "is the cursor's line loaded?" cannot be answered by comparing it against
/// `Window::first_view_line ..last_view_line_exclusive`: those are *view* coordinates — an
/// index into the view's concatenated lines — while a cursor's line is a line of its element's own
/// buffer. In a patch the two are unrelated, so the range test says "not loaded" for a line sitting
/// in plain sight, and a client that then refetches around it fetches nothing and goes blank.
pub fn line_is_loaded(window: &Window, element: FieldId, logical_line: u32) -> bool {
    window.root.editors().iter().any(|node| match node {
        Element::Editor {
            element: id, lines, ..
        } => *id == element && lines.iter().any(|l| l.logical_line == logical_line),
        _ => false,
    })
}

/// Where a viewport row lands in the window's **flattened line list**: its index, and how many of
/// that line's rows sit above the viewport.
///
/// An index, deliberately, not a logical line. A logical line identifies a line only *within its
/// element* — two elements windowing different files both have a line 12 — so a painter that
/// converts a row to a logical line and then indexes by `line - window.first_view_line` reads
/// off the end of the list and draws nothing. That is not hypothetical: it blanked the working
/// changes view for every patch whose first hunk was not at the top of its file.
pub fn line_index_at_row(window: &Window, row: VisualRow) -> Option<(usize, u32)> {
    line_blocks(window)
        .into_iter()
        .enumerate()
        .find_map(|(idx, (start, height))| {
            (row >= start && start.distance_to(row) < height).then(|| (idx, start.distance_to(row)))
        })
}

/// Each rendered line's **block**: where it starts and how tall it is, counting the chrome standing
/// above it and its phantom rows as part of it.
///
/// The unit the terminal scrolls in — its `(scroll_line_index, scroll_skip_rows)` pair names a
/// position inside one of these. Summing only the lines' own heights, which is what this used to do,
/// loses one row per chrome row: the mapping from screen row to line drifts further out the further
/// you scroll, which is exactly what it looked like.
fn line_blocks(window: &Window) -> Vec<(VisualRow, u32)> {
    line_blocks_of(&window.root)
        .into_iter()
        .map(|(rel, height)| (window.first_visual_row.saturating_add(rel), height))
        .collect()
}

/// [`line_blocks`] over a bare tree, in rows counted from the top of the window.
fn line_blocks_of(root: &Element) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    // Rows drawn above the next line — chrome, then its phantoms — and where that run began.
    let mut above = 0u32;
    let mut start: Option<u32> = None;
    for (at, item) in painted_rows_of(root, VisualRow::ZERO) {
        match item {
            PaintedRow::Chrome(_) | PaintedRow::Baseline { .. } => {
                start.get_or_insert(at.get());
                above += 1;
            }
            PaintedRow::Text { row_index, .. } => {
                if row_index == 0 {
                    out.push((start.take().unwrap_or(at.get()), above + 1));
                    above = 0;
                } else if let Some(last) = out.last_mut() {
                    last.1 += 1;
                }
            }
        }
    }
    out
}

/// Resolve the terminal's scroll pair — an index into the flattened line list, plus how many rows
/// of that line's block are hidden above the viewport — into the row painted at the top of the
/// viewport (counted from the top of the window) and that hidden count, clamped into the block.
///
/// Both halves come out of **one** clamp deliberately. Two of them is what "two cursors" was: the
/// painter clamped the hidden count to the top line's whole block — the chrome above it included,
/// since the scroll moves through those rows — while the cursor's own walk clamped to the line's
/// own rows, so a scroll resting inside that chrome had it clamped away and reported the cursor
/// exactly that many rows below where it was painted.
pub fn scroll_top(root: &Element, line_index: usize, skip_rows: u32) -> (u32, u32) {
    let Some((start, height)) = line_blocks_of(root).into_iter().nth(line_index) else {
        return (0, 0);
    };
    let skip = skip_rows.min(height.saturating_sub(1));
    (start + skip, skip)
}

/// Every rendered line of the view, top to bottom, each paired with **where it is**.
///
/// Returns [`ElementLine`]s rather than bare lines deliberately: this is the flattening that made
/// every "wrong line" bug possible, because a `.find(|l| l.logical_line == n)` over its result
/// silently answers about whichever element came first.
pub fn window_lines(window: &Window) -> Vec<(ElementLine, &LogicalLineRender)> {
    row_items(window)
        .into_iter()
        .filter_map(|i| match i {
            RowItem::Line { element, line } => {
                Some((ElementLine::new(element, line.logical_line), line))
            }
            RowItem::Chrome => None,
        })
        .collect()
}

/// What a painter draws on one visual row.
///
/// Every row of a view is exactly one of these, and the rows a view has are exactly the sequence
/// [`painted_rows`] produces. That is the point: three shells were each walking the tree themselves,
/// summing chrome and phantom rows into a running row counter, and getting *different* answers — the
/// cursor drawn rows above where it was painted, clicks landing on the wrong line, a whole file
/// dropped because the walk stopped at the first element. Those are one bug, made three times.
pub enum PaintedRow<'a> {
    /// Generated presentation — a file heading, a rule, a spacer. Holds no cursor position.
    Chrome(&'a Element),
    /// A phantom baseline row: text the working buffer removed, drawn above the line that replaced
    /// it. Holds no cursor position either, but it belongs to a line — which is what a click on one
    /// snaps to.
    Baseline {
        element: FieldId,
        line: &'a LogicalLineRender,
        /// Which of the line's phantom rows this is.
        index: usize,
        row: &'a BaselineRow,
    },
    /// One (possibly wrapped) row of real buffer text — the only kind the cursor can land on.
    Text {
        element: FieldId,
        line: &'a LogicalLineRender,
        row: &'a WrappedRow,
        /// Which of the line's wrapped rows this is; `0` is the line's first.
        row_index: usize,
        /// Whether this is the last rendered line of the whole view — what a closing rule hangs
        /// off. Asking `logical_line + 1 == view_line_count` instead compares a buffer line to a
        /// view line, and never matches once a view spans files.
        last_line: bool,
    },
}

/// Every visual row of the loaded window, top to bottom, paired with its **absolute** visual row.
///
/// The single source of truth for a view's vertical layout, shared by every shell so they cannot
/// drift. Rows run from `window.first_visual_row`; chrome and phantom rows occupy one each, exactly
/// as [`RowItem::rows`] counts them.
pub fn painted_rows(window: &Window) -> Vec<(VisualRow, PaintedRow<'_>)> {
    painted_rows_of(&window.root, window.first_visual_row)
}

/// [`painted_rows`] over a bare tree, for shells that keep the root rather than the whole window.
pub fn painted_rows_of(
    root: &Element,
    first_visual_row: VisualRow,
) -> Vec<(VisualRow, PaintedRow<'_>)> {
    // Chrome belongs to the **element** it introduces, not to the line beneath it. Keying it by
    // that line collapses two files' headings onto one entry the moment their elements start at the
    // same line number, and one of them is simply lost. Two hunks at line 10 of two files is not an
    // edge case; it is what a patch looks like.
    struct Region<'a> {
        element: FieldId,
        lines: &'a [LogicalLineRender],
        chrome: Vec<&'a Element>,
    }
    fn walk<'a>(node: &'a Element, pending: &mut Vec<&'a Element>, out: &mut Vec<Region<'a>>) {
        match node {
            Element::Stack { children } => children.iter().for_each(|c| walk(c, pending, out)),
            Element::Editor { element, lines, .. } => out.push(Region {
                element: *element,
                lines,
                chrome: std::mem::take(pending),
            }),
            // Chrome, or any inline element standing on its own: presentation introducing whatever
            // editor comes next, and trailing the last one if none does.
            _ => pending.push(node),
        }
    }
    let mut regions: Vec<Region<'_>> = Vec::new();
    let mut pending: Vec<&Element> = Vec::new();
    walk(root, &mut pending, &mut regions);

    let total: usize = regions.iter().map(|r| r.lines.len()).sum();
    let mut out = Vec::new();
    let mut at = first_visual_row;
    let mut seen = 0usize;
    let push = |out: &mut Vec<_>, at: &mut VisualRow, item| {
        out.push((*at, item));
        *at = at.saturating_add(1);
    };
    for region in &regions {
        for node in &region.chrome {
            push(&mut out, &mut at, PaintedRow::Chrome(node));
        }
        for line in region.lines {
            for (index, row) in line.baseline_above.iter().enumerate() {
                push(
                    &mut out,
                    &mut at,
                    PaintedRow::Baseline {
                        element: region.element,
                        line,
                        index,
                        row,
                    },
                );
            }
            seen += 1;
            for (row_index, row) in line.visual_rows.iter().enumerate() {
                push(
                    &mut out,
                    &mut at,
                    PaintedRow::Text {
                        element: region.element,
                        line,
                        row,
                        row_index,
                        last_line: seen == total,
                    },
                );
            }
        }
    }
    // Chrome with no editor below it is the patch's closing rule.
    for node in &pending {
        push(&mut out, &mut at, PaintedRow::Chrome(node));
    }
    out
}

/// The chrome standing above each line of the flattened line list, keyed by that line's **index**,
/// plus whatever trails the last one.
///
/// Keyed by index, not by logical line: two elements windowing different files both start at, say,
/// line 10, and keying by the number collapsed their two file headings onto one entry — so one file
/// lost its heading and the other's was drawn in its place. A patch of two hunks is exactly that
/// shape, which is why this is not an edge case.
///
/// A projection of [`painted_rows_of`], so the two cannot drift.
pub fn chrome_by_line_index(root: &Element) -> (HashMap<usize, Vec<&Element>>, Vec<&Element>) {
    let rows = painted_rows_of(root, VisualRow::ZERO);
    let mut above: HashMap<usize, Vec<&Element>> = HashMap::new();
    let mut trailing: Vec<&Element> = Vec::new();
    let mut pending: Vec<&Element> = Vec::new();
    let mut index = 0usize;
    let mut last_line: Option<u32> = None;
    for (_, item) in rows {
        match item {
            PaintedRow::Chrome(node) => pending.push(node),
            PaintedRow::Baseline { .. } => {}
            PaintedRow::Text {
                line, row_index, ..
            } => {
                // One line may span several rows; it enters the list once, on its first.
                if row_index == 0 {
                    if last_line.is_some() {
                        index += 1;
                    }
                    last_line = Some(line.logical_line);
                    if !pending.is_empty() {
                        above.entry(index).or_default().append(&mut pending);
                    }
                }
            }
        }
    }
    // Chrome with no line below it is the patch's closing rule.
    trailing.append(&mut pending);
    (above, trailing)
}

/// The window-relative index of the line's first visual row — phantom rows included, so this
/// points at the top of the line's whole block. `None` when the line isn't loaded. Absolute
/// visual row = `window.first_visual_row + this`.
pub fn rows_before_line(window: &Window, element: FieldId, logical_line: u32) -> Option<u32> {
    let mut rows = 0u32;
    for item in row_items(window) {
        if let RowItem::Line { element: e, line } = item {
            if e == element && line.logical_line == logical_line {
                return Some(rows);
            }
        }
        rows += item.rows();
    }
    None
}

/// The logical line owning absolute visual `abs_row`, and how many of that line's visual rows sit
/// above it (the sub-row offset into the line — phantom diff rows included). Clamps to the loaded
/// window's last row when `abs_row` is past it. The inverse of [`rows_before_line`] + first row.
///
/// Resolved from [`painted_rows`], so it cannot disagree with what a shell draws — the two used to
/// be separate walks, and a chrome row counted by one and not the other is what drew the cursor
/// rows above where it was painted.
pub fn line_at_row(window: &Window, abs_row: VisualRow) -> (FieldId, u32, u32) {
    let rows = painted_rows(window);
    let mut last: Option<(FieldId, u32, u32)> = None;
    for (at, item) in &rows {
        let here = match item {
            // A chrome row holds no cursor position, so the line below it owns the landing.
            PaintedRow::Chrome(_) => continue,
            PaintedRow::Baseline {
                element,
                line,
                index,
                ..
            } => (*element, line.logical_line, *index as u32),
            PaintedRow::Text {
                element,
                line,
                row_index,
                ..
            } => (
                *element,
                line.logical_line,
                (line.baseline_above.len() + row_index) as u32,
            ),
        };
        if *at == abs_row {
            return here;
        }
        last = Some(here);
    }
    last.unwrap_or_else(|| {
        // Nothing loaded. The answer is a *buffer* line, so it cannot be the window's first view
        // line — in a patch those are unrelated numbers, and returning one put the cursor on a line
        // of a different file. The first editor's own start is the only honest answer here.
        (
            0,
            window
                .root
                .editors()
                .first()
                .and_then(|n| match n {
                    Element::Editor {
                        first_buffer_line, ..
                    } => Some(*first_buffer_line),
                    _ => None,
                })
                .unwrap_or(0),
            0,
        )
    })
}

/// A scroll position pinned to *content* rather than an absolute visual row, so it survives a
/// re-layout (wrap toggle, diff toggle) that changes how many visual rows lines occupy. Captured
/// from the current window before the toggle, resolved against the new window after it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollAnchor {
    /// The cursor was visible: keep it at this row offset below the top of the viewport.
    Cursor { screen_row_offset: u32 },
    /// The cursor was off-screen: keep this element's logical line's `sub_row`-th visual row at
    /// the top. The element is part of the pin: a line number alone stops identifying a row once
    /// elements window different buffers.
    Line {
        element: FieldId,
        logical_line: u32,
        sub_row: u32,
    },
}

impl ScrollAnchor {
    /// The logical line this anchor references — the line a re-subscribe must load a window around
    /// so [`resolve_scroll_anchor`] can place it.
    pub fn reference_line(&self, cursor: LogicalPosition) -> u32 {
        match self {
            ScrollAnchor::Cursor { .. } => cursor.line,
            ScrollAnchor::Line { logical_line, .. } => *logical_line,
        }
    }
}

/// Capture a [`ScrollAnchor`] for the current view: pin the cursor if it's visible (so the user's
/// focus stays put), else pin the top visible line (so the content stays put). `top_row` is the
/// absolute visual row at the top of the viewport; `viewport_rows` its height.
pub fn capture_scroll_anchor(
    window: &Window,
    top_row: VisualRow,
    viewport_rows: u32,
    element: FieldId,
    cursor: LogicalPosition,
    tab_width: u32,
) -> ScrollAnchor {
    if let Some((cursor_row, _, _)) = position_cell(window, element, cursor, tab_width) {
        if cursor_row >= top_row && cursor_row < top_row.saturating_add(viewport_rows) {
            return ScrollAnchor::Cursor {
                screen_row_offset: top_row.distance_to(cursor_row),
            };
        }
    }
    let (element, logical_line, sub_row) = line_at_row(window, top_row);
    ScrollAnchor::Line {
        element,
        logical_line,
        sub_row,
    }
}

/// Resolve a captured anchor against the (post-toggle) window into a new absolute top visual row.
/// `cursor` is re-read here because the cursor's visual row moves under the new layout.
pub fn resolve_scroll_anchor(
    window: &Window,
    anchor: ScrollAnchor,
    element: FieldId,
    cursor: LogicalPosition,
    tab_width: u32,
) -> VisualRow {
    match anchor {
        ScrollAnchor::Cursor { screen_row_offset } => {
            let cursor_row = position_cell(window, element, cursor, tab_width)
                .map(|(row, _, _)| row)
                .unwrap_or(window.first_visual_row);
            cursor_row.saturating_sub(screen_row_offset)
        }
        ScrollAnchor::Line {
            element: anchored,
            logical_line,
            sub_row,
        } => {
            let Some(rel) = rows_before_line(window, anchored, logical_line) else {
                return window.first_visual_row;
            };
            // Wrap may have shrunk the line; clamp the sub-row into its new height.
            let height = row_items(window)
                .into_iter()
                .find_map(|i| match i {
                    RowItem::Line { element: e, line }
                        if e == anchored && line.logical_line == logical_line =>
                    {
                        Some(line_rows(line))
                    }
                    _ => None,
                })
                .unwrap_or(1);
            window
                .first_visual_row
                .saturating_add(rel)
                .saturating_add(sub_row.min(height.saturating_sub(1)))
        }
    }
}

/// Locate a position's grid cell: `(absolute visual row, display col, width)`. The width covers
/// the char under a block cursor; a position past the line's last char (Insert mode at EOL, or
/// the empty line) gets a 1-col cell just past the text. `None` when the line isn't loaded.
pub fn position_cell(
    window: &Window,
    element: FieldId,
    pos: LogicalPosition,
    tab_width: u32,
) -> Option<(VisualRow, u32, u32)> {
    let items = row_items(window);
    let line = items.iter().find_map(|i| match i {
        RowItem::Line { element: e, line } if *e == element && line.logical_line == pos.line => {
            Some(*line)
        }
        _ => None,
    })?;
    // The cursor never lands on phantom rows; content starts below them.
    let line_start = window
        .first_visual_row
        .saturating_add(rows_before_line(window, element, pos.line)?)
        .saturating_add(line.baseline_above.len() as u32);
    // The row owning the position: the last one starting at or before it. A position exactly at
    // a wrap boundary belongs to the *following* row (the boundary byte is its first char).
    let row_idx = line
        .visual_rows
        .iter()
        .rposition(|r| r.byte_offset <= pos.col)
        .unwrap_or(0);
    let row = &line.visual_rows[row_idx];
    for cell in row_cells(row, tab_width) {
        if cell.byte == pos.col {
            return Some((
                line_start.saturating_add(row_idx as u32),
                cell.dcol,
                cell.width,
            ));
        }
        if cell.byte > pos.col {
            // Position inside a multi-byte char; snap to that char's cell.
            return Some((
                line_start.saturating_add(row_idx as u32),
                cell.dcol,
                cell.width,
            ));
        }
    }
    // Past the row's text: the virtual cell after the last char.
    let dcol = row_cells(row, tab_width)
        .last()
        .map(|c| c.dcol + c.width)
        .unwrap_or_else(|| row_prefix_cols(row));
    Some((line_start.saturating_add(row_idx as u32), dcol, 1))
}

/// Map a grid cell back to a buffer position — the mouse path. Rows above/below the loaded
/// window clamp to its first/last row; a display col past the row's text maps to just past the
/// last char (the server clamps to the line end). `None` only when the window has no lines.
pub fn hit_test(
    window: &Window,
    abs_row: i64,
    dcol: u32,
    tab_width: u32,
) -> Option<(FieldId, LogicalPosition)> {
    let rel = (abs_row - window.first_visual_row.get() as i64).max(0) as u32;
    let mut remaining = rel;
    let mut target: Option<(FieldId, &LogicalLineRender, &WrappedRow)> = None;
    'outer: for item in row_items(window) {
        // Neither a chrome row nor a phantom baseline row holds a cursor position — a click on
        // either snaps to the first content row at or below it.
        let RowItem::Line { element, line } = item else {
            remaining = remaining.saturating_sub(1);
            continue;
        };
        let virtuals = line.baseline_above.len() as u32;
        if remaining < virtuals {
            if let Some(row) = line.visual_rows.first() {
                target = Some((element, line, row));
                break 'outer;
            }
        }
        remaining -= virtuals.min(remaining);
        for row in &line.visual_rows {
            if remaining == 0 {
                target = Some((element, line, row));
                break 'outer;
            }
            remaining -= 1;
        }
    }
    // Past the loaded window: clamp to its last row.
    let (element, line, row) = match target {
        Some(t) => t,
        None => {
            let items = row_items(window);
            let (e, line) = items.iter().rev().find_map(|i| match i {
                RowItem::Line { element, line } => Some((*element, *line)),
                RowItem::Chrome => None,
            })?;
            (e, line, line.visual_rows.last()?)
        }
    };
    for cell in row_cells(row, tab_width) {
        if dcol < cell.dcol + cell.width {
            return Some((
                element,
                LogicalPosition {
                    line: line.logical_line,
                    col: cell.byte,
                },
            ));
        }
    }
    Some((
        element,
        LogicalPosition {
            line: line.logical_line,
            col: row_end_byte(row),
        },
    ))
}

/// Display-col span of a byte range `[start, end)` on one visual row's cells, or `None` when
/// they don't overlap. Used for search-match and diagnostic spans (both are line-relative byte
/// ranges; the cells only carry this row's bytes, so clipping is implicit).
pub fn byte_range_span(cells: &[Cell<'_>], start: u32, end: u32) -> Option<(u32, u32)> {
    let s = cells
        .iter()
        .find(|c| c.byte >= start || c.byte + c.ch.len_utf8() as u32 > start)?;
    let e = cells
        .iter()
        .rev()
        .find(|c| c.byte < end)
        .map(|c| c.dcol + c.width)?;
    (e > s.dcol).then_some((s.dcol, e))
}

/// The selection's display-col span on one visual row, or `None` when the selection doesn't
/// touch it. `min`/`max` are the selection's inclusive endpoints in normal form (`min ≤ max`).
///
/// The line's implicit `\n` lives at byte `row_end` of its last visual row and gets one display
/// cell whenever the inclusive selection covers that byte — uniformly: a range continuing past
/// the line, one ending on the newline, or a 1-char point selection parked there. Callers decide
/// whether a point renders at all (block-cursor modes treat it as the 1-char selection it is).
pub fn row_selection_span(
    line_no: u32,
    row: &WrappedRow,
    is_last_row_of_line: bool,
    min: LogicalPosition,
    max: LogicalPosition,
    tab_width: u32,
) -> Option<(u32, u32)> {
    if line_no < min.line || line_no > max.line {
        return None;
    }
    let cells = row_cells(row, tab_width);
    let row_start = row.byte_offset;
    let row_end = row_end_byte(row);
    // The selection's inclusive byte range on this line; u32::MAX = "through the newline".
    let sel_start = if line_no == min.line { min.col } else { 0 };
    let sel_end = if line_no == max.line {
        max.col
    } else {
        u32::MAX
    };
    let newline_cell = is_last_row_of_line && sel_start <= row_end && sel_end >= row_end;
    // The span over the row's text cells (the newline cell rides on after it).
    let text_span = cells.last().and_then(|last| {
        if sel_end < row_start || sel_start >= row_end {
            return None;
        }
        let start = cells
            .iter()
            .find(|c| c.byte >= sel_start)
            .map(|c| c.dcol)
            .unwrap_or(last.dcol);
        let end = cells
            .iter()
            .rev()
            .find(|c| c.byte <= sel_end)
            .map(|c| c.dcol + c.width)?;
        (end > start).then_some((start, end))
    });
    match (text_span, newline_cell) {
        // A covered newline always adjoins the text span's end (`sel_end ≥ row_end` means the
        // span already runs through the last char).
        (Some((start, end)), true) => Some((start, end + 1)),
        (span, false) => span,
        (None, true) => {
            // Only the newline is covered (a selection edge or point on the `\n`, or an empty
            // line): its cell sits just past the last char, or at the prefix on an empty row.
            let p = cells
                .last()
                .map(|c| c.dcol + c.width)
                .unwrap_or_else(|| row_prefix_cols(row));
            Some((p, p + 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::viewport::{Highlight, Segment};

    fn row(byte_offset: u32, indent: u32, text: &str) -> WrappedRow {
        WrappedRow {
            byte_offset,
            continuation_indent: indent,
            segments: vec![Segment {
                text: text.into(),
                highlights: vec![],
            }],
        }
    }

    fn line(logical_line: u32, rows: Vec<WrappedRow>) -> LogicalLineRender {
        LogicalLineRender {
            change: Default::default(),
            logical_line,
            visual_rows: rows,
            search_matches: vec![],
            baseline_above: vec![],
            diagnostics: vec![],
            sneak_targets: vec![],
        }
    }

    fn window(first_logical: u32, first_visual: u32, lines: Vec<LogicalLineRender>) -> Window {
        let last = first_logical + lines.len() as u32;
        Window {
            first_view_line: ViewLine(first_logical),
            last_view_line_exclusive: ViewLine(last),
            view_line_count: 100,
            max_scroll_view_line: ViewLine(99),
            total_visual_rows: 120,
            first_visual_row: VisualRow(first_visual),
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                element: 0,
                buffer: 0,
                rows: 0,
                first_buffer_line: first_logical,
                lines,
            },
        }
    }

    #[test]
    fn cells_expand_tabs_to_stops() {
        let r = row(0, 0, "\ta\tb");
        let cells = row_cells(&r, 4);
        // Tab at col 0 → 4 wide; 'a' at 4; tab at 5 → 3 wide (next stop 8); 'b' at 8.
        assert_eq!(
            cells.iter().map(|c| (c.dcol, c.width)).collect::<Vec<_>>(),
            vec![(0, 4), (4, 1), (5, 3), (8, 1)]
        );
        assert_eq!(cells[3].byte, 3);
    }

    #[test]
    fn continuation_rows_carry_prefix() {
        let r = row(40, 4, "wrapped");
        assert_eq!(row_prefix_cols(&r), CONTINUATION_MARKER_COLS + 4);
        let cells = row_cells(&r, 4);
        assert_eq!(cells[0].dcol, 6);
        assert_eq!(cells[0].byte, 40);
    }

    #[test]
    fn highlight_kind_attaches_to_cells() {
        let r = WrappedRow {
            byte_offset: 0,
            continuation_indent: 0,
            segments: vec![Segment {
                text: "let x".into(),
                highlights: vec![Highlight {
                    start: 0,
                    end: 3,
                    kind: "keyword".into(),
                }],
            }],
        };
        let cells = row_cells(&r, 4);
        assert_eq!(cells[0].kind, Some("keyword"));
        assert_eq!(cells[2].kind, Some("keyword"));
        assert_eq!(cells[3].kind, None);
    }

    #[test]
    fn position_cell_finds_wrapped_rows() {
        let w = window(
            10,
            20,
            vec![
                line(10, vec![row(0, 0, "0123456789"), row(10, 0, "abcdef")]),
                line(11, vec![row(0, 0, "short")]),
            ],
        );
        // Col 12 lives on line 10's continuation row (byte 10 + 2), abs row 21.
        let (abs, dcol, width) =
            position_cell(&w, 0, LogicalPosition { line: 10, col: 12 }, 4).unwrap();
        assert_eq!(abs, VisualRow(21));
        assert_eq!(dcol, CONTINUATION_MARKER_COLS + 2);
        assert_eq!(width, 1);
        // Line 11 starts after line 10's two rows.
        let (abs, dcol, _) = position_cell(&w, 0, LogicalPosition { line: 11, col: 0 }, 4).unwrap();
        assert_eq!((abs, dcol), (VisualRow(22), 0));
        // Past EOL → virtual cell after the text.
        let (_, dcol, width) =
            position_cell(&w, 0, LogicalPosition { line: 11, col: 5 }, 4).unwrap();
        assert_eq!((dcol, width), (5, 1));
        // Outside the window → None.
        assert!(position_cell(&w, 0, LogicalPosition { line: 9, col: 0 }, 4).is_none());
    }

    #[test]
    fn hit_test_round_trips_and_clamps() {
        let w = window(
            10,
            20,
            vec![
                line(10, vec![row(0, 0, "0123456789"), row(10, 0, "abcdef")]),
                line(11, vec![row(0, 0, "short")]),
            ],
        );
        // Display col 4 on the continuation row = marker(2) + 2 chars in → byte 12. Round-trips
        // with position_cell. A click on the marker itself (col < prefix) lands on the row's
        // first char.
        assert_eq!(
            hit_test(&w, 21, 4, 4),
            Some((0, LogicalPosition { line: 10, col: 12 }))
        );
        assert_eq!(
            hit_test(&w, 21, 0, 4),
            Some((0, LogicalPosition { line: 10, col: 10 }))
        );
        // Click past a row's end → just past its last char.
        assert_eq!(
            hit_test(&w, 22, 40, 4),
            Some((0, LogicalPosition { line: 11, col: 5 }))
        );
        // Above the window clamps to its first row; far below to its last.
        assert_eq!(
            hit_test(&w, 3, 0, 4),
            Some((0, LogicalPosition { line: 10, col: 0 }))
        );
        assert_eq!(
            hit_test(&w, 999, 0, 4),
            Some((0, LogicalPosition { line: 11, col: 0 }))
        );
    }

    #[test]
    fn byte_range_spans_clip_to_row() {
        // byte_offset 10 makes this a continuation row: cells start after the 2-col wrap
        // marker, so byte 10 sits at display col 2.
        let r = row(10, 0, "abcdef"); // bytes 10..16
        let cells = row_cells(&r, 4);
        // Fully inside.
        assert_eq!(byte_range_span(&cells, 12, 14), Some((4, 6)));
        // Overlapping the row start clips; before/after the row → None.
        assert_eq!(byte_range_span(&cells, 0, 12), Some((2, 4)));
        assert_eq!(byte_range_span(&cells, 16, 20), None);
        assert_eq!(byte_range_span(&cells, 0, 10), None);
    }

    #[test]
    fn phantom_rows_count_but_hold_no_cursor() {
        use aether_protocol::viewport::BaselineRow;
        let deleted = |text: &str| BaselineRow {
            text: text.into(),
            stage: Default::default(),
            emphasis: vec![],
        };
        let mut l10 = line(10, vec![row(0, 0, "content")]);
        l10.baseline_above = vec![deleted("removed 1"), deleted("removed 2")];
        let w = window(10, 20, vec![l10, line(11, vec![row(0, 0, "next")])]);
        // Line 11 starts after line 10's block: 2 phantoms + 1 content row.
        assert_eq!(rows_before_line(&w, 0, 11), Some(3));
        // The cursor's cell skips the phantoms: line 10 col 0 sits at abs row 22.
        let (abs, dcol, _) = position_cell(&w, 0, LogicalPosition { line: 10, col: 0 }, 4).unwrap();
        assert_eq!((abs, dcol), (VisualRow(22), 0));
        // Clicking a phantom row snaps to the line's first content row, keeping the column.
        assert_eq!(
            hit_test(&w, 20, 3, 4),
            Some((0, LogicalPosition { line: 10, col: 3 }))
        );
        assert_eq!(
            hit_test(&w, 22, 2, 4),
            Some((0, LogicalPosition { line: 10, col: 2 }))
        );
        assert_eq!(
            hit_test(&w, 23, 0, 4),
            Some((0, LogicalPosition { line: 11, col: 0 }))
        );
    }

    #[test]
    fn selection_spans_per_row() {
        let r1 = row(0, 0, "0123456789");
        let r2 = row(10, 0, "abcdef");
        let min = LogicalPosition { line: 10, col: 8 };
        let max = LogicalPosition { line: 11, col: 1 };
        // First row of line 10: bytes 8..9 selected → cols 8..10.
        assert_eq!(
            row_selection_span(10, &r1, false, min, max, 4),
            Some((8, 10))
        );
        // Continuation row: fully selected, plus the newline col (last row of the line).
        assert_eq!(
            row_selection_span(10, &r2, true, min, max, 4),
            Some((CONTINUATION_MARKER_COLS, CONTINUATION_MARKER_COLS + 6 + 1))
        );
        // End line: cols 0..=1 inclusive → 0..2.
        let r3 = row(0, 0, "short");
        assert_eq!(row_selection_span(11, &r3, true, min, max, 4), Some((0, 2)));
        // Untouched line.
        assert_eq!(row_selection_span(12, &r3, true, min, max, 4), None);
    }

    #[test]
    fn selection_span_skips_rows_before_start() {
        let r1 = row(0, 0, "0123456789");
        let min = LogicalPosition { line: 10, col: 12 };
        let max = LogicalPosition { line: 10, col: 14 };
        // Selection starts on the continuation row; the first row shows none of it.
        assert_eq!(row_selection_span(10, &r1, false, min, max, 4), None);
    }

    #[test]
    fn empty_line_inside_selection_shows_newline_cell() {
        let empty = row(0, 0, "");
        let min = LogicalPosition { line: 9, col: 0 };
        let max = LogicalPosition { line: 11, col: 0 };
        assert_eq!(
            row_selection_span(10, &empty, true, min, max, 4),
            Some((0, 1))
        );
    }

    #[test]
    fn selection_ending_on_newline_includes_its_cell() {
        // `max` sits *on* the newline byte (col == text len): the cell past the last char joins
        // the span — the same rule as a range continuing past the line.
        let r = row(0, 0, "abc");
        let min = LogicalPosition { line: 10, col: 1 };
        let max = LogicalPosition { line: 10, col: 3 };
        assert_eq!(row_selection_span(10, &r, true, min, max, 4), Some((1, 4)));
        // Ending on the last *char* doesn't reach the newline.
        let max = LogicalPosition { line: 10, col: 2 };
        assert_eq!(row_selection_span(10, &r, true, min, max, 4), Some((1, 3)));
    }

    #[test]
    fn point_selection_spans_one_cell() {
        // A point (min == max) is the 1-char selection of the char under it.
        let r = row(0, 0, "abc");
        let p = LogicalPosition { line: 10, col: 1 };
        assert_eq!(row_selection_span(10, &r, true, p, p, 4), Some((1, 2)));
        // Parked on the newline: just the newline cell.
        let p = LogicalPosition { line: 10, col: 3 };
        assert_eq!(row_selection_span(10, &r, true, p, p, 4), Some((3, 4)));
        // Parked on an empty line: its newline cell at col 0.
        let empty = row(0, 0, "");
        let p = LogicalPosition { line: 10, col: 0 };
        assert_eq!(row_selection_span(10, &empty, true, p, p, 4), Some((0, 1)));
    }

    #[test]
    fn line_at_row_maps_absolute_row_to_line_and_suboffset() {
        // line 5: 2 rows, line 6: 1 row, line 7: 3 rows. Window starts at visual row 100.
        let w = window(
            5,
            100,
            vec![
                line(5, vec![row(0, 0, "aa"), row(2, 0, "bb")]),
                line(6, vec![row(0, 0, "c")]),
                line(7, vec![row(0, 0, "d"), row(1, 0, "e"), row(2, 0, "f")]),
            ],
        );
        assert_eq!(line_at_row(&w, VisualRow(100)), (0, 5, 0));
        assert_eq!(line_at_row(&w, VisualRow(101)), (0, 5, 1)); // 2nd row of line 5
        assert_eq!(line_at_row(&w, VisualRow(102)), (0, 6, 0));
        assert_eq!(line_at_row(&w, VisualRow(104)), (0, 7, 1)); // 2nd row of line 7
                                                                // Past the loaded window clamps to the last line's last row.
        assert_eq!(line_at_row(&w, VisualRow(999)), (0, 7, 2));
    }

    #[test]
    fn scroll_anchor_pins_cursor_when_visible_else_top_line() {
        let w = window(
            5,
            100,
            vec![
                line(5, vec![row(0, 0, "aa"), row(2, 0, "bb")]),
                line(6, vec![row(0, 0, "cccc")]),
                line(7, vec![row(0, 0, "d")]),
            ],
        );
        // Cursor on line 6 (visual row 102), viewport [100, 105): visible → Cursor anchor at offset 2.
        let cursor = LogicalPosition { line: 6, col: 0 };
        assert_eq!(
            capture_scroll_anchor(&w, VisualRow(100), 5, 0, cursor, 4),
            ScrollAnchor::Cursor {
                screen_row_offset: 2
            }
        );
        // Cursor off-screen (viewport [100, 101)) → pin the top line + sub-row.
        assert_eq!(
            capture_scroll_anchor(&w, VisualRow(101), 1, 0, cursor, 4),
            ScrollAnchor::Line {
                element: 0,
                logical_line: 5,
                sub_row: 1
            }
        );
    }

    #[test]
    fn resolve_scroll_anchor_keeps_cursor_offset_after_relayout() {
        // After a wrap toggle the top line now wraps to 3 rows (was 2), shifting later lines down.
        let after = window(
            5,
            100,
            vec![
                line(5, vec![row(0, 0, "a"), row(1, 0, "a"), row(2, 0, "a")]),
                line(6, vec![row(0, 0, "cccc")]),
                line(7, vec![row(0, 0, "d")]),
            ],
        );
        let cursor = LogicalPosition { line: 6, col: 0 };
        // Cursor anchor (offset 2): line 6 now sits at visual row 103, so top = 103 - 2 = 101.
        let row = resolve_scroll_anchor(
            &after,
            ScrollAnchor::Cursor {
                screen_row_offset: 2,
            },
            0,
            cursor,
            4,
        );
        assert_eq!(row, VisualRow(101));
        // Line anchor for line 6 → its first row (103), sub-row clamped into the line.
        assert_eq!(
            resolve_scroll_anchor(
                &after,
                ScrollAnchor::Line {
                    element: 0,
                    logical_line: 6,
                    sub_row: 0,
                },
                0,
                cursor,
                4,
            ),
            VisualRow(103)
        );
    }
    /// Chrome rows occupy screen rows and belong to no line, so every row calculation has to walk
    /// the tree rather than sum per-line heights.
    ///
    /// This is a regression test with a scar: when chrome moved out of the lines and into the tree,
    /// three separate row calculations kept summing `baseline_above + visual_rows` and silently
    /// stopped counting it — which draws the cursor N rows high in a patch and lands clicks N rows
    /// off, N being the chrome between the scroll top and the target. Nothing caught it.
    /// A row resolves to a *position* in the line list, not to a logical line.
    ///
    /// Regression: a patch's elements window real files, so its lines start at whatever line the
    /// hunk begins on — 16, say — while the window's `first_view_line` is 0 in view coordinates.
    /// A painter converting a row to a logical line and indexing by `line - first_view_line`
    /// then read past the end and drew nothing: the working changes view was blank for every patch
    /// whose first hunk was not at the top of its file.
    #[test]
    fn a_row_resolves_to_a_position_not_a_logical_line() {
        let lines: Vec<LogicalLineRender> =
            (16..20).map(|i| line(i, vec![row(0, 0, "x")])).collect();
        let window = Window {
            first_view_line: ViewLine(0),
            last_view_line_exclusive: ViewLine(4),
            view_line_count: 4,
            max_scroll_view_line: ViewLine(0),
            total_visual_rows: 4,
            first_visual_row: VisualRow(0),
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                element: 0,
                buffer: 3,
                rows: 4,
                first_buffer_line: 16,
                lines,
            },
        };

        assert_eq!(line_index_at_row(&window, VisualRow(0)), Some((0, 0)));
        assert_eq!(line_index_at_row(&window, VisualRow(2)), Some((2, 0)));
        assert_eq!(
            line_index_at_row(&window, VisualRow(9)),
            None,
            "past the end is None, not a wrapped-around index"
        );
        // The line at that position keeps its own identity, which is a *file* line.
        let at = line_index_at_row(&window, VisualRow(2)).unwrap().0;
        assert_eq!(window_lines(&window)[at].0.line, 18);
    }

    #[test]
    fn chrome_rows_count_toward_row_positions() {
        use aether_protocol::ui::{Element, RailJoin};
        use aether_protocol::viewport::ChromeKind;

        let chrome = || Element::Chrome {
            kind: ChromeKind::Rule,
            rail: RailJoin::Tees,
            children: vec![Element::fill('─')],
        };
        let editor = |first: u32, n: u32| Element::Editor {
            element: first,
            buffer: 0,
            rows: n,
            first_buffer_line: first,
            lines: (first..first + n)
                .map(|i| line(i, vec![row(0, 0, "x")]))
                .collect(),
        };
        // chrome, line 0, chrome, line 1 — two single-row lines with a chrome row before each.
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![chrome(), editor(0, 1), chrome(), editor(1, 1)],
        };

        assert_eq!(
            rows_before_line(&w, 0, 0),
            Some(1),
            "the chrome above line 0"
        );
        assert_eq!(
            rows_before_line(&w, 1, 1),
            Some(3),
            "chrome, line 0, chrome — summing line heights alone would say 1"
        );
        // And the inverse agrees, reporting which element the row landed in.
        assert_eq!(line_at_row(&w, VisualRow(1)), (0, 0, 0));
        assert_eq!(line_at_row(&w, VisualRow(3)), (1, 1, 0));
        assert_eq!(
            row_items(&w).iter().map(RowItem::rows).sum::<u32>(),
            4,
            "two chrome rows and two content rows"
        );
    }

    /// Two elements over two different files whose line numbers collide — the shape that made
    /// every lookup-by-number wrong. Element 0 shows lines 10..12 of one file, element 1 the same
    /// numbered lines of another, so "line 11" names two different rows.
    fn colliding_elements() -> Window {
        let editor = |element: u32| Element::Editor {
            element,
            buffer: element as u64,
            rows: 2,
            first_buffer_line: 10,
            lines: vec![
                line(10, vec![row(0, 0, "a")]),
                line(11, vec![row(0, 0, "b")]),
            ],
        };
        let mut w = window(0, 0, vec![]);
        w.first_view_line = ViewLine(0);
        w.last_view_line_exclusive = ViewLine(4);
        w.root = Element::Stack {
            children: vec![editor(0), editor(1)],
        };
        w
    }

    /// The client's half of the crossing: a cursor line is a *buffer* line, and naming it as a
    /// view line has to go through the element that owns it.
    #[test]
    fn a_buffer_line_names_a_different_view_line_in_each_element() {
        let w = colliding_elements();
        assert_eq!(view_line_of(&w, 0, 11), Some(ViewLine(1)));
        assert_eq!(
            view_line_of(&w, 1, 11),
            Some(ViewLine(3)),
            "the same buffer line, two elements down"
        );
        // A line no element carries has no view line — and a caller must fetch rather than guess.
        assert_eq!(view_line_of(&w, 0, 99), None);
        assert_eq!(view_line_of(&w, 9, 11), None, "no such element");
    }

    /// "Scroll to this line" means the top of its **block**: a heading comes with the line it
    /// introduces, rather than being left above the fold.
    ///
    /// The two directions have to be inverses. `line_at_row` maps a chrome row to the line below it,
    /// so the row that line starts at is where its chrome starts — asking for "rows before the line"
    /// instead scrolled a patch just past its own first file heading every time it opened.
    #[test]
    fn a_view_lines_block_starts_at_its_chrome() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                Element::Editor {
                    element: 0,
                    buffer: 1,
                    rows: 2,
                    first_buffer_line: 10,
                    lines: vec![
                        line(10, vec![row(0, 0, "a")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                },
                chrome("b.rs"),
                Element::Editor {
                    element: 1,
                    buffer: 2,
                    rows: 1,
                    first_buffer_line: 10,
                    lines: vec![line(10, vec![row(0, 0, "c")])],
                },
            ],
        };
        // Rows: 0 "a.rs", 1 line 10, 2 line 11, 3 "b.rs", 4 line 10 of the second element.
        assert_eq!(
            block_start_of_view_line(&w, ViewLine(0)),
            Some(VisualRow(0)),
            "the first line's block starts at its heading, not below it"
        );
        assert_eq!(
            rows_before_view_line(&w, ViewLine(0)),
            Some(1),
            "…which is exactly what asking for the rows *before* the line answers differently"
        );
        assert_eq!(
            block_start_of_view_line(&w, ViewLine(1)),
            Some(VisualRow(2))
        );
        assert_eq!(
            block_start_of_view_line(&w, ViewLine(2)),
            Some(VisualRow(3)),
            "the second element's block starts at its own heading"
        );
        assert_eq!(block_start_of_view_line(&w, ViewLine(9)), None);
        // And it inverts the row→line direction: take any row to its line, then that line back to a
        // row, and you land on the start of the block the row was in.
        for row in 0..5u32 {
            let (idx, skip) = line_index_at_row(&w, VisualRow(row)).expect("row is in the window");
            let line = w.first_view_line.saturating_add(idx as u32);
            assert_eq!(
                block_start_of_view_line(&w, line),
                Some(VisualRow(row - skip)),
                "row {row} → line {line:?} → block start"
            );
        }
    }

    /// The inverse, in view space: rows accumulate across elements, chrome included.
    #[test]
    fn rows_before_a_view_line_span_the_elements_above_it() {
        let w = colliding_elements();
        assert_eq!(rows_before_view_line(&w, ViewLine(0)), Some(0));
        assert_eq!(
            rows_before_view_line(&w, ViewLine(3)),
            Some(3),
            "two rows of element 0 and one of element 1"
        );
        assert_eq!(rows_before_view_line(&w, ViewLine(9)), None);
    }

    /// Scoping anything to "what is on screen" needs the focused element's own line numbers, not
    /// the window's view-line range — which in a patch names no file's lines at all.
    #[test]
    fn the_loaded_range_is_reported_in_the_elements_own_lines() {
        let w = colliding_elements();
        assert_eq!(loaded_line_range(&w, 0), Some((10, 11)));
        assert_eq!(loaded_line_range(&w, 1), Some((10, 11)));
        assert_eq!(loaded_line_range(&w, 7), None);
        assert_ne!(
            loaded_line_range(&w, 1),
            Some((
                w.first_view_line.get(),
                w.last_view_line_exclusive.get() - 1
            )),
            "the view's own range would have been 0..3 — a different question entirely"
        );
    }

    /// A compact reading of a view's row layout: one string per visual row, tagged by kind, so a
    /// test can state the whole expected column at once.
    fn painted(w: &Window) -> Vec<String> {
        painted_rows(w)
            .into_iter()
            .map(|(at, item)| match item {
                PaintedRow::Chrome(n) => format!("{at} chrome {}", chrome_text(n)),
                PaintedRow::Baseline { row, .. } => format!("{at} baseline {}", row.text),
                PaintedRow::Text {
                    element,
                    line,
                    row_index,
                    last_line,
                    ..
                } => format!(
                    "{at} text e{element} L{}#{row_index}{}",
                    line.logical_line,
                    if last_line { " last" } else { "" }
                ),
            })
            .collect()
    }

    fn chrome_text(n: &Element) -> String {
        match n {
            Element::Chrome { children, .. } => {
                children.iter().map(Element::text_content).collect()
            }
            _ => String::new(),
        }
    }

    fn chrome(text: &str) -> Element {
        use aether_protocol::ui::{Element, RailJoin};
        use aether_protocol::viewport::ChromeKind;
        Element::Chrome {
            kind: ChromeKind::FileHeader,
            rail: RailJoin::Opens,
            children: vec![Element::text(text, Vec::new())],
        }
    }

    /// The whole point of the shared layout: chrome, phantoms and wrapped rows all occupy rows, and
    /// every shell must agree on *which*. Three painters each walked this themselves and disagreed
    /// — the cursor drawn above where it was painted, clicks landing short, a file dropped.
    #[test]
    fn every_kind_of_row_lands_where_the_shells_must_draw_it() {
        let mut l16 = line(16, vec![row(0, 0, "a"), row(1, 0, "wrapped")]);
        l16.baseline_above = vec![BaselineRow {
            text: "was".into(),
            stage: Default::default(),
            emphasis: vec![],
        }];
        let mut w = window(0, 0, vec![]);
        w.first_visual_row = VisualRow(100);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                Element::Editor {
                    element: 0,
                    buffer: 1,
                    rows: 3,
                    first_buffer_line: 16,
                    lines: vec![l16],
                },
                chrome("b.rs"),
                Element::Editor {
                    element: 1,
                    buffer: 2,
                    rows: 1,
                    first_buffer_line: 0,
                    lines: vec![line(0, vec![row(0, 0, "b")])],
                },
                chrome("closing"),
            ],
        };
        assert_eq!(
            painted(&w),
            vec![
                "100 chrome a.rs",
                "101 baseline was",
                "102 text e0 L16#0",
                "103 text e0 L16#1",
                "104 chrome b.rs",
                "105 text e1 L0#0 last",
                "106 chrome closing",
            ],
            "rows run from first_visual_row; chrome and phantoms occupy one each"
        );
    }

    /// Chrome belongs to the element it introduces, not to the line under it.
    ///
    /// Found by the browser shell's first-ever painter test: two hunks at line 10 of two files —
    /// which is simply what a patch looks like — keyed both headings to line 10, so one file lost
    /// its own and the other's appeared in its place. All three shells shared the mistake.
    #[test]
    fn two_files_starting_at_the_same_line_each_keep_their_own_heading() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                Element::Editor {
                    element: 0,
                    buffer: 1,
                    rows: 1,
                    first_buffer_line: 10,
                    lines: vec![line(10, vec![row(0, 0, "a")])],
                },
                chrome("b.rs"),
                Element::Editor {
                    element: 1,
                    buffer: 2,
                    rows: 1,
                    first_buffer_line: 10,
                    lines: vec![line(10, vec![row(0, 0, "b")])],
                },
            ],
        };
        assert_eq!(
            painted(&w),
            vec![
                "0 chrome a.rs",
                "1 text e0 L10#0",
                "2 chrome b.rs",
                "3 text e1 L10#0 last",
            ]
        );
    }

    /// Scrolling must land on the line that is actually painted there.
    ///
    /// Reported from the terminal: the further you scrolled, the further the viewport drifted out
    /// of step with the window. `line_index_at_row` summed only each line's own rows, so every
    /// chrome row above the target cost it one line — the error grew with the number of file
    /// headings scrolled past, which is why it looked like drift rather than a constant offset.
    #[test]
    fn a_screen_row_resolves_to_the_line_painted_on_it_across_chrome() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                Element::Editor {
                    element: 0,
                    buffer: 1,
                    rows: 2,
                    first_buffer_line: 10,
                    lines: vec![
                        line(10, vec![row(0, 0, "a")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                },
                chrome("b.rs"),
                Element::Editor {
                    element: 1,
                    buffer: 2,
                    rows: 2,
                    first_buffer_line: 10,
                    lines: vec![
                        line(10, vec![row(0, 0, "c")]),
                        line(11, vec![row(0, 0, "d")]),
                    ],
                },
            ],
        };
        // Rows: 0 chrome, 1 "a", 2 "b", 3 chrome, 4 "c", 5 "d".
        let painted_at = |r: u32| {
            painted_rows(&w)
                .into_iter()
                .find(|(at, _)| *at == VisualRow(r))
                .map(|(_, item)| match item {
                    PaintedRow::Text { line, .. } => format!("line {}", line.logical_line),
                    _ => "chrome".into(),
                })
                .unwrap()
        };
        assert_eq!(painted_at(4), "line 10", "the second file's first line");

        // The row→line lookup must agree with what is painted there. Line index 2 is the third
        // rendered line, which is that same row.
        assert_eq!(
            line_index_at_row(&w, VisualRow(4)),
            Some((2, 1)),
            "row 4 is the third line, one row into its block (the chrome above it)"
        );
        // And the chrome row itself belongs to the block of the line it introduces, exactly as the
        // painter treats it when skipping.
        assert_eq!(line_index_at_row(&w, VisualRow(3)), Some((2, 0)));
        // Without counting chrome, row 4 would have resolved to line index 4 — off the end.
        assert_eq!(line_index_at_row(&w, VisualRow(5)), Some((3, 0)));
        assert_eq!(line_index_at_row(&w, VisualRow(9)), None);
    }

    /// `scroll_top` is the inverse of `line_index_at_row`, including inside the chrome.
    ///
    /// Reported from the terminal as two cursors, one scroll row apart per chrome row hidden above
    /// the top line: the painter clamped the scroll's sub-line offset to the top line's whole block
    /// — chrome above it included, since scrolling moves through it — while the cursor's own walk
    /// clamped to the line's own rows, so the hidden chrome was clamped away and the terminal caret
    /// sat that many rows below the block it was painted in.
    #[test]
    fn a_scroll_resting_inside_the_chrome_resolves_to_the_row_painted_there() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                chrome("@@ hunk"),
                Element::Editor {
                    element: 0,
                    buffer: 1,
                    rows: 2,
                    first_buffer_line: 16,
                    lines: vec![
                        line(16, vec![row(0, 0, "a"), row(1, 0, "wrapped")]),
                        line(17, vec![row(0, 0, "b")]),
                    ],
                },
            ],
        };
        // Rows: 0 "a.rs", 1 "@@ hunk", 2 line 16, 3 its wrap, 4 line 17. The first line's block is
        // all of 0..=3, so a scroll can rest on either chrome row.
        for (skip, want) in [(0, 0), (1, 1), (2, 2), (3, 3)] {
            assert_eq!(scroll_top(&w.root, 0, skip), (want, skip), "skip {skip}");
        }
        // Past the block's last row the scroll clamps into it rather than bleeding onto the next
        // line — and clamps the *hidden count* to match, so the painter and the cursor agree.
        assert_eq!(scroll_top(&w.root, 0, 9), (3, 3));
        // Round-trip: whatever row the scroll names, that row resolves back to the same pair.
        for skip in 0..=3 {
            let (top, clamped) = scroll_top(&w.root, 0, skip);
            assert_eq!(line_index_at_row(&w, VisualRow(top)), Some((0, clamped)));
        }
        // The second line's block is its own row alone; its chrome belongs to the first.
        assert_eq!(scroll_top(&w.root, 1, 0), (4, 0));
        assert_eq!(scroll_top(&w.root, 1, 5), (4, 0));
    }

    /// `last_line` is what a closing rule hangs off, and it is positional. The second file's lines
    /// are numbered *below* the first's, so any test of "is this the end?" that reads a line number
    /// answers about the wrong row.
    #[test]
    fn the_last_row_is_positional_not_a_line_number() {
        let w = colliding_elements();
        let last = painted_rows(&w)
            .into_iter()
            .filter_map(|(at, item)| match item {
                PaintedRow::Text {
                    line, last_line, ..
                } => last_line.then_some((at, line.logical_line)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            last,
            vec![(VisualRow(3), 11)],
            "only the final rendered row is the last one"
        );
    }

    /// The layout and the row *counter* must never disagree: `RowItem::rows` is what sizes the
    /// scrollbar and clamps the scroll, while `painted_rows` is what the shells draw. A view whose
    /// height and whose drawn rows differ is exactly the short-scrollbar bug.
    #[test]
    fn the_row_count_matches_the_rows_actually_painted() {
        for w in [
            colliding_elements(),
            window(0, 0, vec![line(0, vec![row(0, 0, "x")])]),
        ] {
            let counted: u32 = row_items(&w).iter().map(RowItem::rows).sum();
            assert_eq!(counted as usize, painted_rows(&w).len());
        }
    }

    /// With nothing loaded, `line_at_row` must not fall back to the window's first *view* line:
    /// it answers in buffer lines, and in a patch the two are unrelated numbers.
    #[test]
    fn an_empty_window_falls_back_to_a_buffer_line_not_a_view_line() {
        let mut w = window(0, 0, vec![]);
        w.first_view_line = ViewLine(40);
        w.root = Element::Editor {
            element: 0,
            buffer: 0,
            rows: 0,
            first_buffer_line: 7,
            lines: vec![],
        };
        assert_eq!(line_at_row(&w, VisualRow(0)), (0, 7, 0));
    }
}
