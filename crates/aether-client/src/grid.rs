//! Pure mapping between protocol coordinates and the monospace cell grid.
//!
//! The server renders a `Window` of logical lines, each split into `WrappedRow`s by its soft-wrap
//! math; positions on the wire are `(logical line, byte col)`. Everything pixel-ish in the client
//! reduces to *(absolute visual row, display column)* cells, so this module owns that translation:
//! cursor → cell, mouse cell → position, selection → per-row display-column spans. Display-column
//! math mirrors the server's: tabs advance to the next `tab_width` stop, other chars take their
//! Unicode width. Continuation rows are prefixed by the wrap marker ("↪ ") plus the row's
//! continuation indent, same as the web client.

use aether_protocol::coords::{ElementRow, VisualRow};
use aether_protocol::ui::LayoutOwner;
use aether_protocol::viewport::{
    BaselineRow, Element, FieldId, LogicalLineRender, ScrollPosition, SliceRequest, Window,
    WrappedRow,
};
use aether_protocol::LogicalPosition;
use serde::{Deserialize, Serialize};
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

// ---- the view's vertical layout ----------------------------------------------------------------
//
// The tree carries every element's height and every loaded slice's row within its element, and
// the client lays the view out from that: a chrome node is one row, an editor is its `rows`, and
// a loaded slice sits `first_row` rows into its editor. Everything below is arithmetic over that
// one walk — the total height, where an element starts, which slices a viewport reaches, and where
// each painted row lands — so the three shells cannot disagree about any of it.
//
// One height in the walk is not the tree's to give: an element the **client** lays out. The
// server sends its lines unwrapped and counts one row per line, which is an estimate; how tall it
// really is, and where each line sits within it, is whatever the shell measured — and the shell
// says so through [`Measured`], which every function here takes. The tree's numbers are the answer
// wherever the shell has none.

/// What a shell that laid an element out **itself** tells the grid about it — the one input to the
/// view's vertical layout that does not come from the tree.
///
/// An editor the server laid out has a height the server computed: its wrapped rows and phantoms.
/// An element the client lays out — prose it wraps and renders from source — has no height the
/// server could know. So the server sends such an element's lines unwrapped, one row per line on
/// the wire, and the shell measures it: how tall it is, and the row each loaded line starts on.
/// Absent from here, the element is taken at one row per line, which is what the server counted
/// and a serviceable estimate until the shell has measured.
///
/// Rows are the unit throughout, in every shell. A pixel shell reports a prose element's height
/// rounded up to whole rows and positions its blocks at pixel precision inside that; the slack is
/// at most a row at the element's foot, and the scroll model stays one arithmetic in one unit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measured {
    pub elements: std::collections::HashMap<FieldId, MeasuredElement>,
}

/// One client-laid-out element as measured: where each loaded line starts within it, and where
/// the loaded slice ends. Lines outside the loaded slice — which the shell has not seen — count one
/// row each, exactly as the tree counts them, so the rows above the slice are `first_row` rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredElement {
    /// The wire row of the first loaded line: the editor's `first_row`.
    pub first_row: ElementRow,
    /// For each loaded line, the row within the element it starts on, ascending. The first is
    /// `first_row` — the unloaded lines above it are one row each.
    pub starts: Vec<u32>,
    /// The row after the last loaded line's last row.
    pub end: u32,
}

impl MeasuredElement {
    /// The element's height in rows, given its line count (`rows` as the server sent it): the
    /// unloaded lines at one row each, the loaded ones as measured.
    pub fn rows(&self, lines: u32) -> u32 {
        let loaded = self.starts.len() as u32;
        let before = self.first_row.get();
        let measured = self
            .end
            .saturating_sub(self.starts.first().copied().unwrap_or(before));
        before + measured + lines.saturating_sub(before + loaded)
    }

    /// The wire row sitting `offset` rows into the element.
    fn row_at(&self, offset: u32) -> ElementRow {
        let first = self.first_row.get();
        let Some(&start) = self.starts.first() else {
            return ElementRow(offset);
        };
        if offset < start {
            return ElementRow(offset);
        }
        if offset >= self.end {
            return ElementRow(first + self.starts.len() as u32 + (offset - self.end));
        }
        let idx = self
            .starts
            .partition_point(|s| *s <= offset)
            .saturating_sub(1);
        ElementRow(first + idx as u32)
    }

    /// The row within the element that wire row `row` starts on.
    fn offset_of(&self, row: ElementRow) -> u32 {
        let first = self.first_row.get();
        let Some(i) = row.get().checked_sub(first) else {
            return row.get();
        };
        match self.starts.get(i as usize) {
            Some(start) => *start,
            None => self.end + (i - self.starts.len() as u32),
        }
    }
}

impl Measured {
    /// The measurement for `node`, when it is an element the client lays out and the shell has
    /// measured.
    fn of(&self, node: &Element) -> Option<&MeasuredElement> {
        match node {
            Element::Editor {
                element,
                laid_out_by: LayoutOwner::Client,
                ..
            } => self.elements.get(element),
            _ => None,
        }
    }

    /// Rows `node` occupies: the tree's count, or the measured height for an element the client
    /// laid out.
    pub fn height(&self, node: &Element) -> u32 {
        match (node, self.of(node)) {
            (Element::Editor { rows, .. }, Some(m)) => m.rows(*rows),
            (Element::Editor { rows, .. }, None) => *rows,
            (Element::Stack { .. }, _) => 0,
            _ => 1,
        }
    }

    /// Which wire row of `node` sits `offset` rows into it — the row the server numbers, which is
    /// the row a fetch is asked by.
    pub fn row_at(&self, node: &Element, offset: u32) -> ElementRow {
        self.of(node)
            .map_or(ElementRow(offset), |m| m.row_at(offset))
    }

    /// The row within `node` that wire row `row` starts on.
    pub fn offset_of(&self, node: &Element, row: ElementRow) -> u32 {
        self.of(node).map_or(row.get(), |m| m.offset_of(row))
    }
}

/// Rows the whole view occupies: chrome, one row each, every server-laid-out editor's height, and
/// every client-laid-out element as measured.
///
/// What a scroller is sized to and what the scroll limit is taken from. The server used to send
/// this; it cannot once a view holds an element the client laid out, and it never needed to — the
/// tree says it, where the shell does not.
pub fn total_rows(root: &Element, measured: &Measured) -> u32 {
    let mut total = 0u32;
    walk_rows(root, measured, &mut |_, rows| {
        total = total.saturating_add(rows)
    });
    total
}

/// The absolute row an element's own content starts on — after its chrome.
///
/// Computable for **every** element, loaded or not: the tree carries every editor's height whether
/// or not its lines are in the window. That is what makes it the right target for revealing an
/// element you have just focused — the cursor's own line cannot be located, because the element it
/// moved to has no lines loaded yet.
pub fn element_start_row(
    window: &Window,
    element: FieldId,
    measured: &Measured,
) -> Option<VisualRow> {
    element_start_row_of(&window.root, element, measured)
}

/// [`element_start_row`] over a bare tree.
pub fn element_start_row_of(
    root: &Element,
    element: FieldId,
    measured: &Measured,
) -> Option<VisualRow> {
    let mut at = 0u32;
    let mut found = None;
    walk_rows(root, measured, &mut |node, rows| {
        if found.is_none() {
            if let Element::Editor { element: id, .. } = node {
                if *id == element {
                    found = Some(VisualRow(at));
                }
            }
            at = at.saturating_add(rows);
        }
    });
    found
}

/// The slices a viewport showing rows `top..top+rows` needs, `overscan` rows either side: one per
/// editor the span reaches, each named by row within that editor.
///
/// This is the request a client makes while scrolling. It knows where every editor starts from the
/// tree; what it cannot know is which *lines* a row range is, since that depends on how the lines
/// above wrapped — so it asks by row and the server answers with lines. An element the client lays
/// out is asked for whole whenever any of it is in reach: the client renders it from its source
/// entire — a block's shape depends on the lines around it — so a part of it is never something
/// it can paint, and the server loads such an element whole regardless.
pub fn slices_for(
    root: &Element,
    top: VisualRow,
    rows: u32,
    overscan: u32,
    measured: &Measured,
) -> Vec<SliceRequest> {
    let lo = top.get().saturating_sub(overscan);
    let hi = top.get().saturating_add(rows).saturating_add(overscan);
    let mut out = Vec::new();
    let mut at = 0u32;
    walk_rows(root, measured, &mut |node, height| {
        if let Element::Editor {
            element,
            rows: lines,
            laid_out_by,
            ..
        } = node
        {
            let (start, end) = (at, at.saturating_add(height));
            let (a, b) = (lo.max(start), hi.min(end));
            if a < b {
                let (from, rows) = match laid_out_by {
                    LayoutOwner::Client => (ElementRow::ZERO, (*lines).max(1)),
                    LayoutOwner::Server => {
                        let from = measured.row_at(node, a - start);
                        let to = measured.row_at(node, b - start);
                        (from, to.get().saturating_sub(from.get()).max(1))
                    }
                };
                out.push(SliceRequest {
                    element: *element,
                    from_row: from,
                    rows,
                });
            }
        }
        at = at.saturating_add(height);
    });
    out
}

/// Drop measurements of elements the tree no longer lays out client-side — a view re-presented as
/// the editor, or a switch to another view — so no stale height positions a window it was never
/// measured for. Called by a shell on every window adoption, before anything resolves through it.
pub fn prune_measured(measured: &mut Measured, root: &Element) {
    let client: Vec<FieldId> = root
        .editors()
        .into_iter()
        .filter_map(|node| match node {
            Element::Editor {
                element,
                laid_out_by: LayoutOwner::Client,
                ..
            } => Some(*element),
            _ => None,
        })
        .collect();
    measured.elements.retain(|id, _| client.contains(id));
}

/// Whether the tree has an element the client lays out that it has not measured yet: the shell
/// cannot place a window by rows it has not counted, so a placement into such a view waits for
/// the shell's layout — the reader's, once it has parsed what the window carries.
pub fn awaits_measure(root: &Element, measured: &Measured) -> bool {
    root.editors().into_iter().any(|node| {
        matches!(
            node,
            Element::Editor {
                element,
                laid_out_by: LayoutOwner::Client,
                ..
            } if !measured.elements.contains_key(element)
        )
    })
}

/// Whether every slice in `wanted` is already loaded: its rows lie inside the rows the editor's
/// loaded lines cover. What decides whether a scroll needs a fetch.
pub fn loaded_covers(root: &Element, wanted: &[SliceRequest]) -> bool {
    wanted.iter().all(|slice| {
        root.editors().iter().any(|node| match node {
            Element::Editor {
                element,
                first_row,
                lines,
                ..
            } if *element == slice.element => {
                let loaded_rows: u32 = lines.iter().map(line_rows).sum();
                let (lo, hi) = (first_row.get(), first_row.get().saturating_add(loaded_rows));
                lo <= slice.from_row.get() && slice.from_row.get().saturating_add(slice.rows) <= hi
            }
            _ => false,
        })
    })
}

/// One pass over the tree in painting order, telling `f` each node's row count: 1 for chrome and
/// for any inline element standing on its own, an editor's height, nothing for a container.
///
/// A horizontal group is **one** screen row: its children share it. An `Editor` nested inside one
/// is representable and not renderable — this row model is a flat top-to-bottom list with no way to
/// say "these two editors share these rows", so side-by-side diff needs a different row model, not
/// a deeper walk here. Worth asserting rather than dropping silently, because [`Element::walk`]
/// *does* descend into rows: `lines()` and `editors()` would count such an editor while this drew
/// it as one chrome row, and two traversals of one tree disagreeing is the kind of thing that shows
/// up as a cursor in the wrong place three layers away.
fn walk_rows<'a>(node: &'a Element, measured: &Measured, f: &mut impl FnMut(&'a Element, u32)) {
    match node {
        Element::Stack { children } => children.iter().for_each(|c| walk_rows(c, measured, f)),
        Element::Editor { .. } => f(node, measured.height(node)),
        Element::Row { children } | Element::Chrome { children, .. } => {
            debug_assert!(
                !children.iter().any(|c| !c.editors().is_empty()),
                "an Editor inside a Row/Chrome: representable, but the row model cannot give \
                 it rows of its own — see the module docs on `ui::Element`"
            );
            f(node, 1)
        }
        Element::Text { .. } | Element::Space { .. } | Element::Fill { .. } => f(node, 1),
    }
}

/// Every rendered line of the view, top to bottom, each paired with **where it is**.
///
/// Returns [`ElementLine`]s rather than bare lines deliberately: this is the flattening that made
/// every "wrong line" bug possible, because a `.find(|l| l.logical_line == n)` over its result
/// silently answers about whichever element came first.
pub fn window_lines(window: &Window) -> Vec<(ElementLine, &LogicalLineRender)> {
    window
        .root
        .editors()
        .into_iter()
        .filter_map(|node| match node {
            Element::Editor { element, lines, .. } => Some(
                lines
                    .iter()
                    .map(move |line| (ElementLine::new(*element, line.logical_line), line)),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The **buffer**-line range this window has loaded for `element`, as `first..=last`. `None` when
/// the element has no lines in the window.
///
/// For anything that needs to name lines of the focused element's file — scoping a sneak to what is
/// on screen, say.
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

/// Whether `logical_line` of `element` is among the lines this window actually carries.
///
/// Per element: a cursor's line is a line of its element's buffer, and in a patch two elements
/// both have a line 12. A client that asks about the number alone refetches around a line sitting
/// in plain sight.
pub fn line_is_loaded(window: &Window, element: FieldId, logical_line: u32) -> bool {
    window.root.editors().iter().any(|node| match node {
        Element::Editor {
            element: id, lines, ..
        } => *id == element && lines.iter().any(|l| l.logical_line == logical_line),
        _ => false,
    })
}

/// What a painter draws on one visual row.
///
/// Every loaded row of a view is exactly one of these, and the rows a view has are exactly the
/// sequence [`painted_rows`] produces. That is the point: three shells were each walking the tree
/// themselves, summing chrome and phantom rows into a running row counter, and getting *different*
/// answers — the cursor drawn rows above where it was painted, clicks landing on the wrong line, a
/// whole file dropped because the walk stopped at the first element. Those are one bug, made three
/// times.
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
    ///
    /// For an element the client lays out, one per line, on the row the shell measured the line
    /// as starting on: the line's whole unwrapped text, which a shell with no layout of its own for
    /// the element paints as it is, and a shell that laid it out paints from that layout instead.
    Text {
        element: FieldId,
        line: &'a LogicalLineRender,
        row: &'a WrappedRow,
        /// Which of the line's wrapped rows this is; `0` is the line's first.
        row_index: usize,
        /// Whether this is the last rendered line of the whole view — what a closing rule hangs
        /// off. Asking `logical_line + 1 == some line count` instead compares a buffer line to a
        /// count of the wrong space, and never matches once a view spans files.
        last_line: bool,
    },
}

/// Every loaded visual row of the view, top to bottom, paired with its **absolute** row.
///
/// The single source of truth for a view's vertical layout, shared by every shell so they cannot
/// drift. Chrome occupies one row each; an editor's loaded lines sit `first_row` rows into it and
/// the rest of its height is unloaded — those rows are simply absent here, so consecutive entries
/// are not necessarily consecutive rows. A painter draws what it finds at each row and blank
/// elsewhere.
pub fn painted_rows<'a>(
    window: &'a Window,
    measured: &Measured,
) -> Vec<(VisualRow, PaintedRow<'a>)> {
    painted_rows_of(&window.root, measured)
}

/// [`painted_rows`] over a bare tree, for shells that keep the root rather than the whole window.
pub fn painted_rows_of<'a>(
    root: &'a Element,
    measured: &Measured,
) -> Vec<(VisualRow, PaintedRow<'a>)> {
    let total_lines: usize = root.lines().len();
    let mut out = Vec::new();
    let mut at = 0u32;
    let mut seen = 0usize;
    walk_rows(root, measured, &mut |node, height| {
        match node {
            Element::Editor {
                element,
                first_row,
                lines,
                ..
            } => {
                let client_laid_out = measured.of(node).is_some()
                    || matches!(
                        node,
                        Element::Editor {
                            laid_out_by: LayoutOwner::Client,
                            ..
                        }
                    );
                let mut row = at.saturating_add(first_row.get());
                for (i, line) in lines.iter().enumerate() {
                    if client_laid_out {
                        // Where the shell put the line — or, unmeasured, one row per line.
                        row = at.saturating_add(
                            measured.offset_of(node, first_row.saturating_add(i as u32)),
                        );
                    }
                    for (index, baseline) in line.baseline_above.iter().enumerate() {
                        out.push((
                            VisualRow(row),
                            PaintedRow::Baseline {
                                element: *element,
                                line,
                                index,
                                row: baseline,
                            },
                        ));
                        row = row.saturating_add(1);
                    }
                    seen += 1;
                    for (row_index, wrapped) in line.visual_rows.iter().enumerate() {
                        out.push((
                            VisualRow(row),
                            PaintedRow::Text {
                                element: *element,
                                line,
                                row: wrapped,
                                row_index,
                                last_line: seen == total_lines,
                            },
                        ));
                        row = row.saturating_add(1);
                    }
                }
            }
            _ => out.push((VisualRow(at), PaintedRow::Chrome(node))),
        }
        at = at.saturating_add(height);
    });
    out
}

/// The first painted row of a line: its first phantom row if it has any, else its first text row.
/// `None` when the line isn't loaded.
pub fn line_top_row(
    window: &Window,
    element: FieldId,
    logical_line: u32,
    measured: &Measured,
) -> Option<VisualRow> {
    painted_rows(window, measured)
        .into_iter()
        .find_map(|(at, item)| match item {
            PaintedRow::Baseline {
                element: e, line, ..
            }
            | PaintedRow::Text {
                element: e, line, ..
            } if e == element && line.logical_line == logical_line => Some(at),
            _ => None,
        })
}

/// The first row of a line's **block** — the chrome standing above its element included, when the
/// line is the element's first row.
///
/// What "scroll to this line" means, and the inverse of the row→line direction: a chrome row
/// belongs to the line it introduces (see [`line_at_row`]), so the row a line starts at is the
/// first row of that run, not the line's own text row. Positioning a viewport with
/// [`line_top_row`] instead puts the text at the top and the heading above the fold — which is
/// why a patch opened at its first line hid the very file heading that introduces it.
pub fn line_block_start(
    window: &Window,
    element: FieldId,
    logical_line: u32,
    measured: &Measured,
) -> Option<VisualRow> {
    let rows = painted_rows(window, measured);
    let idx = rows.iter().position(|(_, item)| match item {
        PaintedRow::Baseline {
            element: e, line, ..
        }
        | PaintedRow::Text {
            element: e, line, ..
        } => *e == element && line.logical_line == logical_line,
        PaintedRow::Chrome(_) => false,
    })?;
    // Walk back over chrome immediately above, but only if the line is the first row of its element
    // — chrome above an element belongs to the element, not to a line in its middle.
    let starts_element = element_start_row(window, element, measured) == Some(rows[idx].0);
    let mut start = idx;
    if starts_element {
        while start > 0
            && matches!(rows[start - 1].1, PaintedRow::Chrome(_))
            && rows[start - 1].0.saturating_add(1) == rows[start].0
        {
            start -= 1;
        }
    }
    Some(rows[start].0)
}

/// The line owning absolute row `abs_row`, and how many of that line's rows sit above it (the
/// sub-row offset into the line — phantom rows included). A chrome row resolves to the line below
/// it; a row nothing is loaded at resolves to the nearest loaded line at or before it, else the
/// first loaded line after it.
///
/// Resolved from [`painted_rows`], so it cannot disagree with what a shell draws.
pub fn line_at_row(
    window: &Window,
    abs_row: VisualRow,
    measured: &Measured,
) -> (FieldId, u32, u32) {
    let rows = painted_rows(window, measured);
    let content = |item: &PaintedRow<'_>| -> Option<(FieldId, u32, u32)> {
        match item {
            PaintedRow::Chrome(_) => None,
            PaintedRow::Baseline {
                element,
                line,
                index,
                ..
            } => Some((*element, line.logical_line, *index as u32)),
            PaintedRow::Text {
                element,
                line,
                row_index,
                ..
            } => Some((
                *element,
                line.logical_line,
                (line.baseline_above.len() + row_index) as u32,
            )),
        }
    };
    let mut before: Option<(FieldId, u32, u32)> = None;
    let mut after: Option<(FieldId, u32, u32)> = None;
    let mut on_chrome = false;
    for (at, item) in &rows {
        let Some(here) = content(item) else {
            on_chrome |= *at == abs_row;
            continue;
        };
        if *at == abs_row {
            return here;
        }
        if *at < abs_row {
            before = Some(here);
        } else if after.is_none() {
            after = Some(here);
        }
    }
    // A chrome row belongs to the line it introduces — the one below it; a row nothing is loaded
    // at belongs to whatever is nearest above.
    let nearest = if on_chrome {
        after.or(before)
    } else {
        before.or(after)
    };
    nearest.unwrap_or_else(|| {
        // Nothing loaded. The answer is a *buffer* line, and the first editor's own start is the
        // only honest one.
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

/// Where a viewport's top row is, as content — what a window request reports so the server can
/// restore the view there. The line at the row and how far into its rows the top sits.
pub fn anchor_at(window: &Window, top: VisualRow, measured: &Measured) -> ScrollPosition {
    let (element, line, sub_row) = line_at_row(window, top, measured);
    ScrollPosition {
        element,
        line,
        sub_row: sub_row as f32,
    }
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
    /// The place this anchor references — what a re-subscribe must load a window around so
    /// [`resolve_scroll_anchor`] can place it. The cursor's own place for a cursor anchor.
    pub fn reference(&self, cursor_element: FieldId, cursor: LogicalPosition) -> ScrollPosition {
        match self {
            ScrollAnchor::Cursor { .. } => ScrollPosition {
                element: cursor_element,
                line: cursor.line,
                sub_row: 0.0,
            },
            ScrollAnchor::Line {
                element,
                logical_line,
                sub_row,
            } => ScrollPosition {
                element: *element,
                line: *logical_line,
                sub_row: *sub_row as f32,
            },
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
    measured: &Measured,
) -> ScrollAnchor {
    if let Some((cursor_row, _, _)) = position_cell(window, element, cursor, tab_width, measured) {
        if cursor_row >= top_row && cursor_row < top_row.saturating_add(viewport_rows) {
            return ScrollAnchor::Cursor {
                screen_row_offset: top_row.distance_to(cursor_row),
            };
        }
    }
    let (element, logical_line, sub_row) = line_at_row(window, top_row, measured);
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
    measured: &Measured,
) -> VisualRow {
    match anchor {
        ScrollAnchor::Cursor { screen_row_offset } => {
            let cursor_row = position_cell(window, element, cursor, tab_width, measured)
                .map(|(row, _, _)| row)
                .unwrap_or_else(|| first_loaded_row(window, measured));
            cursor_row.saturating_sub(screen_row_offset)
        }
        ScrollAnchor::Line {
            element: anchored,
            logical_line,
            sub_row,
        } => {
            let Some(top) = line_top_row(window, anchored, logical_line, measured) else {
                return first_loaded_row(window, measured);
            };
            // Wrap may have shrunk the line; clamp the sub-row into its new height.
            let height = window_lines(window)
                .into_iter()
                .find(|(at, _)| *at == ElementLine::new(anchored, logical_line))
                .map(|(_, line)| line_rows(line))
                .unwrap_or(1);
            top.saturating_add(sub_row.min(height.saturating_sub(1)))
        }
    }
}

/// The first row anything is loaded at, or row 0 with nothing loaded — the fallback a placement
/// takes when the content it was pinned to is gone.
fn first_loaded_row(window: &Window, measured: &Measured) -> VisualRow {
    painted_rows(window, measured)
        .into_iter()
        .find_map(|(at, item)| match item {
            PaintedRow::Chrome(_) => None,
            _ => Some(at),
        })
        .unwrap_or(VisualRow::ZERO)
}

/// Locate a position's grid cell: `(absolute visual row, display col, width)`. The width covers
/// the char under a block cursor; a position past the line's last char (Insert mode at EOL, or
/// the empty line) gets a 1-col cell just past the text. `None` when the line isn't loaded.
pub fn position_cell(
    window: &Window,
    element: FieldId,
    pos: LogicalPosition,
    tab_width: u32,
    measured: &Measured,
) -> Option<(VisualRow, u32, u32)> {
    // The line's text rows, with their absolute rows — the cursor never lands on phantom rows.
    let rows: Vec<(VisualRow, &WrappedRow)> = painted_rows(window, measured)
        .into_iter()
        .filter_map(|(at, item)| match item {
            PaintedRow::Text {
                element: e,
                line,
                row,
                ..
            } if e == element && line.logical_line == pos.line => Some((at, row)),
            _ => None,
        })
        .collect();
    // The row owning the position: the last one starting at or before it. A position exactly at
    // a wrap boundary belongs to the *following* row (the boundary byte is its first char).
    let (at, row) = rows
        .iter()
        .rev()
        .find(|(_, r)| r.byte_offset <= pos.col)
        .or_else(|| rows.first())
        .copied()?;
    for cell in row_cells(row, tab_width) {
        if cell.byte == pos.col {
            return Some((at, cell.dcol, cell.width));
        }
        if cell.byte > pos.col {
            // Position inside a multi-byte char; snap to that char's cell.
            return Some((at, cell.dcol, cell.width));
        }
    }
    // Past the row's text: the virtual cell after the last char.
    let dcol = row_cells(row, tab_width)
        .last()
        .map(|c| c.dcol + c.width)
        .unwrap_or_else(|| row_prefix_cols(row));
    Some((at, dcol, 1))
}

/// Map a grid cell back to a buffer position — the mouse path. A chrome row snaps to the line it
/// introduces, a phantom row to the line it belongs to, a row nothing is loaded at to the nearest
/// loaded line — all as [`line_at_row`] resolves them, so a click and a scroll cannot disagree. A
/// display col past the row's text maps to just past the last char (the server clamps to the line
/// end). `None` only when the window has no lines.
pub fn hit_test(
    window: &Window,
    abs_row: i64,
    dcol: u32,
    tab_width: u32,
    measured: &Measured,
) -> Option<(FieldId, LogicalPosition)> {
    let (element, logical_line, sub_row) =
        line_at_row(window, VisualRow(abs_row.max(0) as u32), measured);
    let rows = painted_rows(window, measured);
    let text_rows: Vec<&WrappedRow> = rows
        .iter()
        .filter_map(|(_, item)| match item {
            PaintedRow::Text {
                element: e,
                line,
                row,
                ..
            } if *e == element && line.logical_line == logical_line => Some(*row),
            _ => None,
        })
        .collect();
    let phantoms = window_lines(window)
        .into_iter()
        .find(|(at, _)| *at == ElementLine::new(element, logical_line))
        .map_or(0, |(_, line)| line.baseline_above.len() as u32);
    // A phantom row (sub-row below the line's own rows) snaps to the line's first text row.
    let row = text_rows
        .get(sub_row.saturating_sub(phantoms) as usize)
        .or(text_rows.first())?;
    for cell in row_cells(row, tab_width) {
        if dcol < cell.dcol + cell.width {
            return Some((
                element,
                LogicalPosition {
                    line: logical_line,
                    col: cell.byte,
                },
            ));
        }
    }
    Some((
        element,
        LogicalPosition {
            line: logical_line,
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

    /// A one-editor view whose loaded slice starts at `first_logical` and sits `first_row` rows
    /// into the element — with no chrome, that is also its absolute row.
    fn window(first_logical: u32, first_row: u32, lines: Vec<LogicalLineRender>) -> Window {
        let loaded: u32 = lines.iter().map(line_rows).sum();
        Window {
            other_elements_dirty: false,
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                element: 0,
                buffer: 0,
                rows: first_row + loaded,
                first_row: ElementRow(first_row),
                laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                first_buffer_line: first_logical,
                lines,
            },
        }
    }

    fn editor(element: u32, first_row: u32, rows: u32, lines: Vec<LogicalLineRender>) -> Element {
        let first_buffer_line = lines.first().map_or(0, |l| l.logical_line);
        Element::Editor {
            element,
            buffer: element as u64 + 1,
            rows,
            first_row: ElementRow(first_row),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            first_buffer_line,
            lines,
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
        let (abs, dcol, width) = position_cell(
            &w,
            0,
            LogicalPosition { line: 10, col: 12 },
            4,
            &Measured::default(),
        )
        .unwrap();
        assert_eq!(abs, VisualRow(21));
        assert_eq!(dcol, CONTINUATION_MARKER_COLS + 2);
        assert_eq!(width, 1);
        // Line 11 starts after line 10's two rows.
        let (abs, dcol, _) = position_cell(
            &w,
            0,
            LogicalPosition { line: 11, col: 0 },
            4,
            &Measured::default(),
        )
        .unwrap();
        assert_eq!((abs, dcol), (VisualRow(22), 0));
        // Past EOL → virtual cell after the text.
        let (_, dcol, width) = position_cell(
            &w,
            0,
            LogicalPosition { line: 11, col: 5 },
            4,
            &Measured::default(),
        )
        .unwrap();
        assert_eq!((dcol, width), (5, 1));
        // Outside the window → None.
        assert!(position_cell(
            &w,
            0,
            LogicalPosition { line: 9, col: 0 },
            4,
            &Measured::default()
        )
        .is_none());
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
            hit_test(&w, 21, 4, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 10, col: 12 }))
        );
        assert_eq!(
            hit_test(&w, 21, 0, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 10, col: 10 }))
        );
        // Click past a row's end → just past its last char.
        assert_eq!(
            hit_test(&w, 22, 40, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 11, col: 5 }))
        );
        // Above the loaded rows snaps to their first; far below to their last.
        assert_eq!(
            hit_test(&w, 3, 0, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 10, col: 0 }))
        );
        assert_eq!(
            hit_test(&w, 999, 0, 4, &Measured::default()),
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
        let deleted = |text: &str| BaselineRow {
            text: text.into(),
            stage: Default::default(),
            emphasis: vec![],
        };
        let mut l10 = line(10, vec![row(0, 0, "content")]);
        l10.baseline_above = vec![deleted("removed 1"), deleted("removed 2")];
        let w = window(10, 20, vec![l10, line(11, vec![row(0, 0, "next")])]);
        // Line 11 starts after line 10's block: 2 phantoms + 1 content row.
        assert_eq!(
            line_top_row(&w, 0, 11, &Measured::default()),
            Some(VisualRow(23))
        );
        // And line 10's own top is its first phantom.
        assert_eq!(
            line_top_row(&w, 0, 10, &Measured::default()),
            Some(VisualRow(20))
        );
        // The cursor's cell skips the phantoms: line 10 col 0 sits at abs row 22.
        let (abs, dcol, _) = position_cell(
            &w,
            0,
            LogicalPosition { line: 10, col: 0 },
            4,
            &Measured::default(),
        )
        .unwrap();
        assert_eq!((abs, dcol), (VisualRow(22), 0));
        // Clicking a phantom row snaps to the line's first content row, keeping the column.
        assert_eq!(
            hit_test(&w, 20, 3, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 10, col: 3 }))
        );
        assert_eq!(
            hit_test(&w, 22, 2, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 10, col: 2 }))
        );
        assert_eq!(
            hit_test(&w, 23, 0, 4, &Measured::default()),
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
        // line 5: 2 rows, line 6: 1 row, line 7: 3 rows. The slice sits at row 100.
        let w = window(
            5,
            100,
            vec![
                line(5, vec![row(0, 0, "aa"), row(2, 0, "bb")]),
                line(6, vec![row(0, 0, "c")]),
                line(7, vec![row(0, 0, "d"), row(1, 0, "e"), row(2, 0, "f")]),
            ],
        );
        assert_eq!(
            line_at_row(&w, VisualRow(100), &Measured::default()),
            (0, 5, 0)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(101), &Measured::default()),
            (0, 5, 1)
        ); // 2nd row of line 5
        assert_eq!(
            line_at_row(&w, VisualRow(102), &Measured::default()),
            (0, 6, 0)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(104), &Measured::default()),
            (0, 7, 1)
        ); // 2nd row of line 7
           // Past the loaded rows resolves to the nearest loaded row above; before them, to the
           // first loaded row.
        assert_eq!(
            line_at_row(&w, VisualRow(999), &Measured::default()),
            (0, 7, 2)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(3), &Measured::default()),
            (0, 5, 0)
        );
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
            capture_scroll_anchor(&w, VisualRow(100), 5, 0, cursor, 4, &Measured::default()),
            ScrollAnchor::Cursor {
                screen_row_offset: 2
            }
        );
        // Cursor off-screen (viewport [100, 101)) → pin the top line + sub-row.
        assert_eq!(
            capture_scroll_anchor(&w, VisualRow(101), 1, 0, cursor, 4, &Measured::default()),
            ScrollAnchor::Line {
                element: 0,
                logical_line: 5,
                sub_row: 1
            }
        );
        // Either kind names a place a re-subscribe can load a window around.
        assert_eq!(
            ScrollAnchor::Cursor {
                screen_row_offset: 2
            }
            .reference(3, cursor),
            ScrollPosition {
                element: 3,
                line: 6,
                sub_row: 0.0
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
            &Measured::default(),
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
                &Measured::default()
            ),
            VisualRow(103)
        );
    }

    /// A row resolves to a *position* in the loaded rows, not to a logical line arithmetic.
    ///
    /// Regression: a patch's elements window real files, so its lines start at whatever line the
    /// hunk begins on — 16, say — while rows count from 0. A painter converting a row to a logical
    /// line and indexing by `line - something` read past the end and drew nothing: the working
    /// changes view was blank for every patch whose first hunk was not at the top of its file.
    #[test]
    fn a_row_resolves_to_a_position_not_a_logical_line() {
        let lines: Vec<LogicalLineRender> =
            (16..20).map(|i| line(i, vec![row(0, 0, "x")])).collect();
        let w = window(16, 0, lines);
        assert_eq!(
            line_at_row(&w, VisualRow(0), &Measured::default()),
            (0, 16, 0)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(2), &Measured::default()),
            (0, 18, 0)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(9), &Measured::default()),
            (0, 19, 0),
            "past the end is the nearest loaded row, not a wrapped-around index"
        );
    }

    /// Chrome rows occupy screen rows and belong to no line, so every row calculation has to walk
    /// the tree rather than sum per-line heights.
    ///
    /// This is a regression test with a scar: when chrome moved out of the lines and into the tree,
    /// three separate row calculations kept summing `baseline_above + visual_rows` and silently
    /// stopped counting it — which draws the cursor N rows high in a patch and lands clicks N rows
    /// off, N being the chrome between the scroll top and the target. Nothing caught it.
    #[test]
    fn chrome_rows_count_toward_row_positions() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("─"),
                editor(0, 0, 1, vec![line(0, vec![row(0, 0, "x")])]),
                chrome("─"),
                editor(1, 0, 1, vec![line(1, vec![row(0, 0, "x")])]),
            ],
        };

        assert_eq!(
            line_top_row(&w, 0, 0, &Measured::default()),
            Some(VisualRow(1)),
            "after the chrome above line 0"
        );
        assert_eq!(
            line_top_row(&w, 1, 1, &Measured::default()),
            Some(VisualRow(3)),
            "chrome, line 0, chrome — summing line heights alone would say 1"
        );
        // And the inverse agrees, reporting which element the row landed in.
        assert_eq!(
            line_at_row(&w, VisualRow(1), &Measured::default()),
            (0, 0, 0)
        );
        assert_eq!(
            line_at_row(&w, VisualRow(3), &Measured::default()),
            (1, 1, 0)
        );
        assert_eq!(
            total_rows(&w.root, &Measured::default()),
            4,
            "two chrome rows and two content rows"
        );
    }

    /// Two elements over two different files whose line numbers collide — the shape that made
    /// every lookup-by-number wrong. Element 0 shows lines 10..12 of one file, element 1 the same
    /// numbered lines of another, so "line 11" names two different rows.
    fn colliding_elements() -> Window {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                editor(
                    0,
                    0,
                    2,
                    vec![
                        line(10, vec![row(0, 0, "a")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                ),
                editor(
                    1,
                    0,
                    2,
                    vec![
                        line(10, vec![row(0, 0, "a")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                ),
            ],
        };
        w
    }

    /// A buffer line has its own row in each element that windows it; naming a row has to go
    /// through the element that owns the line.
    #[test]
    fn a_buffer_line_has_its_own_row_in_each_element() {
        let w = colliding_elements();
        assert_eq!(
            line_top_row(&w, 0, 11, &Measured::default()),
            Some(VisualRow(1))
        );
        assert_eq!(
            line_top_row(&w, 1, 11, &Measured::default()),
            Some(VisualRow(3)),
            "the same buffer line, two rows down"
        );
        // A line no element carries has no row — and a caller must fetch rather than guess.
        assert_eq!(line_top_row(&w, 0, 99, &Measured::default()), None);
        assert_eq!(
            line_top_row(&w, 9, 11, &Measured::default()),
            None,
            "no such element"
        );
    }

    /// "Scroll to this line" means the top of its **block**: a heading comes with the line it
    /// introduces, rather than being left above the fold.
    ///
    /// The two directions have to be inverses. `line_at_row` maps a chrome row to the line below it,
    /// so the row that line starts at is where its chrome starts — asking for the line's own row
    /// instead scrolled a patch just past its own first file heading every time it opened.
    #[test]
    fn a_lines_block_starts_at_its_elements_chrome() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                chrome("@@ hunk"),
                editor(
                    0,
                    0,
                    3,
                    vec![
                        line(10, vec![row(0, 0, "a"), row(1, 0, "wrapped")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                ),
                chrome("b.rs"),
                editor(1, 0, 1, vec![line(10, vec![row(0, 0, "c")])]),
            ],
        };
        // Rows: 0 "a.rs", 1 "@@ hunk", 2 line 10, 3 its wrap, 4 line 11, 5 "b.rs", 6 line 10 of
        // the second element.
        assert_eq!(
            line_block_start(&w, 0, 10, &Measured::default()),
            Some(VisualRow(0)),
            "the first line's block starts at its headings, not below them"
        );
        assert_eq!(
            line_top_row(&w, 0, 10, &Measured::default()),
            Some(VisualRow(2)),
            "…which is exactly what asking for the line's own row answers differently"
        );
        assert_eq!(
            line_block_start(&w, 0, 11, &Measured::default()),
            Some(VisualRow(4)),
            "a line in the middle of its element has no chrome of its own"
        );
        assert_eq!(
            line_block_start(&w, 1, 10, &Measured::default()),
            Some(VisualRow(5)),
            "the second element's block starts at its own heading"
        );
        assert_eq!(line_block_start(&w, 0, 99, &Measured::default()), None);
        // And it inverts the row→line direction: a chrome row resolves to the line it introduces,
        // whose block starts back at that chrome; a content row resolves to its line, whose own
        // rows the sub-row counts from.
        for r in [0u32, 1, 5] {
            let (e, l, sub) = line_at_row(&w, VisualRow(r), &Measured::default());
            assert_eq!(sub, 0, "row {r}");
            assert!(
                line_block_start(&w, e, l, &Measured::default()).unwrap() <= VisualRow(r),
                "row {r}"
            );
        }
        for r in [2u32, 3, 4, 6] {
            let (e, l, sub) = line_at_row(&w, VisualRow(r), &Measured::default());
            assert_eq!(
                line_top_row(&w, e, l, &Measured::default())
                    .unwrap()
                    .saturating_add(sub),
                VisualRow(r),
                "row {r} → line {e}/{l} + {sub}"
            );
        }
    }

    /// Scoping anything to "what is on screen" needs the focused element's own line numbers.
    #[test]
    fn the_loaded_range_is_reported_in_the_elements_own_lines() {
        let w = colliding_elements();
        assert_eq!(loaded_line_range(&w, 0), Some((10, 11)));
        assert_eq!(loaded_line_range(&w, 1), Some((10, 11)));
        assert_eq!(loaded_line_range(&w, 7), None);
    }

    /// A compact reading of a view's row layout: one string per loaded visual row, tagged by kind,
    /// so a test can state the whole expected column at once.
    fn painted(w: &Window) -> Vec<String> {
        painted_rows(w, &Measured::default())
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

    /// An `Editor` nested in a `Row` is loud, not silently drawn as one chrome row.
    ///
    /// The shape nothing produces — and the one that would quietly lose an editor's lines if the
    /// row walk kept its old catch-all. `debug_assertions`-gated because the guard is a
    /// `debug_assert`: it is a producer bug, caught where producers are written, not a condition to
    /// pay for in a release render loop.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "an Editor inside a Row")]
    fn an_editor_nested_in_a_row_is_refused_rather_than_dropped() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Row {
            children: vec![editor(0, 0, 1, vec![line(0, vec![row(0, 0, "a")])])],
        };
        let _ = painted_rows(&w, &Measured::default());
    }

    /// The row layout and the tree walk must agree about how many lines a view has.
    ///
    /// `Element::walk` — and so `lines()` and `editors()` — descends into `Row` and `Chrome`, while
    /// the row layout treats each as a single screen row. That is the right answer for everything
    /// produced today (a `Row` is chrome, and its children are text), and it silently stops being
    /// the right answer the moment anything nests an `Editor` in one: the tree would count its
    /// lines and the layout would draw one chrome row instead. Nothing produces that shape, which
    /// is exactly why it needs a test rather than a reader noticing.
    #[test]
    fn the_row_layout_accounts_for_every_line_the_tree_walk_finds() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                editor(
                    0,
                    0,
                    2,
                    vec![line(0, vec![row(0, 0, "a")]), line(1, vec![row(0, 0, "b")])],
                ),
                // A row of pure chrome: one screen row, no lines — the shape that exists today.
                Element::Row {
                    children: vec![
                        Element::text("left", Vec::new()),
                        Element::Fill { glyph: '─' },
                    ],
                },
                chrome("closing"),
            ],
        };

        let painted = painted_rows(&w, &Measured::default())
            .iter()
            .filter(|(_, i)| matches!(i, PaintedRow::Text { row_index: 0, .. }))
            .count();
        assert_eq!(
            painted,
            w.root.lines().len(),
            "the layout paints {painted} lines while the tree walk finds {} — the two traversals \
             disagree about what the view contains",
            w.root.lines().len()
        );
        assert_eq!(painted, 2, "and the fixture must actually contain lines");
        assert_eq!(
            total_rows(&w.root, &Measured::default()),
            5,
            "chrome, two lines, a row, chrome"
        );
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
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                editor(0, 0, 3, vec![l16]),
                chrome("b.rs"),
                editor(1, 0, 1, vec![line(0, vec![row(0, 0, "b")])]),
                chrome("closing"),
            ],
        };
        assert_eq!(
            painted(&w),
            vec![
                "0 chrome a.rs",
                "1 baseline was",
                "2 text e0 L16#0",
                "3 text e0 L16#1",
                "4 chrome b.rs",
                "5 text e1 L0#0 last",
                "6 chrome closing",
            ],
            "chrome and phantoms occupy one row each"
        );
    }

    /// A loaded slice sits `first_row` rows into its element, and an element with nothing loaded
    /// still takes its rows: what comes after it is placed by the tree's heights, not by what
    /// happens to be loaded.
    #[test]
    fn a_slice_sits_at_its_row_within_its_element() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                // 10 rows tall, with lines 40..42 loaded 7 rows in.
                editor(
                    0,
                    7,
                    10,
                    vec![
                        line(40, vec![row(0, 0, "x")]),
                        line(41, vec![row(0, 0, "y")]),
                    ],
                ),
                chrome("b.rs"),
                // 5 rows tall, nothing loaded.
                editor(1, 0, 5, vec![]),
                chrome("c.rs"),
                editor(2, 0, 1, vec![line(0, vec![row(0, 0, "z")])]),
            ],
        };
        assert_eq!(
            painted(&w),
            vec![
                "0 chrome a.rs",
                "8 text e0 L40#0",
                "9 text e0 L41#0",
                "11 chrome b.rs",
                "17 chrome c.rs",
                "18 text e2 L0#0 last",
            ],
            "rows 1..8 and 10 of the first element and all of the second are unloaded"
        );
        assert_eq!(total_rows(&w.root, &Measured::default()), 19);
        assert_eq!(
            element_start_row(&w, 1, &Measured::default()),
            Some(VisualRow(12))
        );
        assert_eq!(
            element_start_row(&w, 2, &Measured::default()),
            Some(VisualRow(18))
        );
        assert_eq!(
            line_block_start(&w, 0, 40, &Measured::default()),
            Some(VisualRow(8)),
            "not the element's first row, so no chrome"
        );
        assert_eq!(
            line_block_start(&w, 2, 0, &Measured::default()),
            Some(VisualRow(17)),
            "the element's first row: its heading comes too"
        );
        // A row nothing is loaded at resolves to the nearest loaded row above it.
        assert_eq!(
            line_at_row(&w, VisualRow(14), &Measured::default()),
            (0, 41, 0)
        );
        assert_eq!(
            hit_test(&w, 14, 0, 4, &Measured::default()),
            Some((0, LogicalPosition { line: 41, col: 0 }))
        );
    }

    /// The request a scrolling client makes: one slice per editor its viewport reaches, each by row
    /// within that editor, with the overscan either side.
    #[test]
    fn slices_name_each_editor_the_span_reaches_by_row_within_it() {
        let root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                editor(0, 0, 5, vec![]),
                chrome("b.rs"),
                editor(1, 0, 10, vec![]),
            ],
        };
        // Rows: 0 chrome, 1..6 e0, 6 chrome, 7..17 e1.
        assert_eq!(
            slices_for(&root, VisualRow(4), 6, 0, &Measured::default()),
            vec![
                SliceRequest {
                    element: 0,
                    from_row: ElementRow(3),
                    rows: 2
                },
                SliceRequest {
                    element: 1,
                    from_row: ElementRow(0),
                    rows: 3
                },
            ]
        );
        assert_eq!(
            slices_for(&root, VisualRow(4), 6, 2, &Measured::default()),
            vec![
                SliceRequest {
                    element: 0,
                    from_row: ElementRow(1),
                    rows: 4
                },
                SliceRequest {
                    element: 1,
                    from_row: ElementRow(0),
                    rows: 5
                },
            ],
            "overscan reaches two rows further each way"
        );
        assert_eq!(
            slices_for(&root, VisualRow(0), 1, 0, &Measured::default()),
            vec![],
            "a viewport showing only chrome needs no lines"
        );
        assert_eq!(
            slices_for(&root, VisualRow(40), 5, 0, &Measured::default()),
            vec![],
            "past the end there is nothing to ask for"
        );
    }

    /// Whether a scroll needs a fetch: every wanted slice must lie inside the rows its editor
    /// already has loaded.
    #[test]
    fn loaded_covers_reads_each_editors_loaded_rows() {
        let root = Element::Stack {
            children: vec![editor(
                0,
                3,
                10,
                vec![line(5, vec![row(0, 0, "a")]), line(6, vec![row(0, 0, "b")])],
            )],
        };
        let want = |from_row: u32, rows: u32| {
            vec![SliceRequest {
                element: 0,
                from_row: ElementRow(from_row),
                rows,
            }]
        };
        assert!(loaded_covers(&root, &want(3, 2)), "exactly the loaded rows");
        assert!(loaded_covers(&root, &want(4, 1)));
        assert!(!loaded_covers(&root, &want(2, 2)), "starts above the slice");
        assert!(!loaded_covers(&root, &want(4, 2)), "runs past it");
        assert!(
            !loaded_covers(
                &root,
                &want(3, 0)
                    .into_iter()
                    .map(|s| SliceRequest { element: 9, ..s })
                    .collect::<Vec<_>>()
            ),
            "no such element"
        );
        assert!(loaded_covers(&root, &[]), "nothing wanted is covered");
    }

    /// The anchor a window request reports: the content under the top row.
    #[test]
    fn the_anchor_names_the_content_under_the_top_row() {
        let w = window(
            5,
            100,
            vec![
                line(5, vec![row(0, 0, "aa"), row(2, 0, "bb")]),
                line(6, vec![row(0, 0, "c")]),
            ],
        );
        assert_eq!(
            anchor_at(&w, VisualRow(101), &Measured::default()),
            ScrollPosition {
                element: 0,
                line: 5,
                sub_row: 1.0
            }
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
                editor(0, 0, 1, vec![line(10, vec![row(0, 0, "a")])]),
                chrome("b.rs"),
                editor(1, 0, 1, vec![line(10, vec![row(0, 0, "b")])]),
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
    /// of step with the window. The row→line lookup summed only each line's own rows, so every
    /// chrome row above the target cost it one line — the error grew with the number of file
    /// headings scrolled past, which is why it looked like drift rather than a constant offset.
    #[test]
    fn a_screen_row_resolves_to_the_line_painted_on_it_across_chrome() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.rs"),
                editor(
                    0,
                    0,
                    2,
                    vec![
                        line(10, vec![row(0, 0, "a")]),
                        line(11, vec![row(0, 0, "b")]),
                    ],
                ),
                chrome("b.rs"),
                editor(
                    1,
                    0,
                    2,
                    vec![
                        line(10, vec![row(0, 0, "c")]),
                        line(11, vec![row(0, 0, "d")]),
                    ],
                ),
            ],
        };
        // Rows: 0 chrome, 1 "a", 2 "b", 3 chrome, 4 "c", 5 "d".
        let painted_at = |r: u32| {
            painted_rows(&w, &Measured::default())
                .into_iter()
                .find(|(at, _)| *at == VisualRow(r))
                .map(|(_, item)| match item {
                    PaintedRow::Text { line, .. } => format!("line {}", line.logical_line),
                    _ => "chrome".into(),
                })
                .unwrap()
        };
        assert_eq!(painted_at(4), "line 10", "the second file's first line");
        assert_eq!(
            line_at_row(&w, VisualRow(4), &Measured::default()),
            (1, 10, 0),
            "row 4 is the second element's first line"
        );
        // The chrome row belongs to the line it introduces.
        assert_eq!(
            line_at_row(&w, VisualRow(3), &Measured::default()),
            (1, 10, 0)
        );
        // Without counting chrome, row 5 would have resolved past the end.
        assert_eq!(
            line_at_row(&w, VisualRow(5), &Measured::default()),
            (1, 11, 0)
        );
    }

    /// `last_line` is what a closing rule hangs off, and it is positional. The second file's lines
    /// are numbered *below* the first's, so any test of "is this the end?" that reads a line number
    /// answers about the wrong row.
    #[test]
    fn the_last_row_is_positional_not_a_line_number() {
        let w = colliding_elements();
        let last = painted_rows(&w, &Measured::default())
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

    /// The view's height and its drawn rows must never disagree when everything is loaded: the
    /// height sizes the scrollbar and clamps the scroll, the drawn rows are what the shells paint.
    /// A view whose height and whose drawn rows differ is exactly the short-scrollbar bug.
    #[test]
    fn the_row_count_matches_the_rows_actually_painted() {
        for w in [
            colliding_elements(),
            window(0, 0, vec![line(0, vec![row(0, 0, "x")])]),
        ] {
            assert_eq!(
                total_rows(&w.root, &Measured::default()) as usize,
                painted_rows(&w, &Measured::default()).len()
            );
        }
    }

    /// With nothing loaded, `line_at_row` answers in buffer lines — the first editor's own start.
    #[test]
    fn an_empty_window_falls_back_to_a_buffer_line() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Editor {
            element: 0,
            buffer: 0,
            rows: 0,
            first_row: ElementRow::ZERO,
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
            first_buffer_line: 7,
            lines: vec![],
        };
        assert_eq!(
            line_at_row(&w, VisualRow(0), &Measured::default()),
            (0, 7, 0)
        );
    }

    // ---- elements the client lays out ----------------------------------------------------------

    /// An element the client lays out: unwrapped lines on the wire, one row per line, with the
    /// shell's measurements attached — every third loaded line laid out three rows tall.
    fn prose(element: u32, first_row: u32, lines: u32, loaded: std::ops::Range<u32>) -> Element {
        Element::Editor {
            element,
            buffer: element as u64 + 1,
            rows: lines,
            first_row: ElementRow(first_row),
            laid_out_by: aether_protocol::ui::LayoutOwner::Client,
            first_buffer_line: loaded.start,
            lines: loaded.map(|n| line(n, vec![row(0, 0, "prose")])).collect(),
        }
    }

    fn measured(element: u32, first_row: u32, heights: &[u32]) -> Measured {
        let mut starts = Vec::new();
        let mut at = first_row;
        for h in heights {
            starts.push(at);
            at += h;
        }
        let mut m = Measured::default();
        m.elements.insert(
            element,
            MeasuredElement {
                first_row: ElementRow(first_row),
                starts,
                end: at,
            },
        );
        m
    }

    /// Unmeasured, a client-laid-out element is what the server counted: one row per line. The
    /// tree's estimate stands until the shell has laid the element out.
    #[test]
    fn an_unmeasured_client_element_is_one_row_per_line() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.md"),
                prose(0, 2, 10, 2..5),
                chrome("b.rs"),
                editor(1, 0, 3, vec![]),
            ],
        };
        let none = Measured::default();
        assert_eq!(total_rows(&w.root, &none), 1 + 10 + 1 + 3);
        assert_eq!(
            painted(&w),
            vec![
                "0 chrome a.md",
                "3 text e0 L2#0",
                "4 text e0 L3#0",
                "5 text e0 L4#0 last",
                "11 chrome b.rs"
            ]
        );
        assert_eq!(element_start_row(&w, 1, &none), Some(VisualRow(12)));
    }

    /// Measured, the shell's numbers are the element's: its height is the loaded lines as laid
    /// out plus one row per unloaded line, everything below moves by the difference, and each
    /// loaded line paints on the row the shell put it on.
    #[test]
    fn a_measured_client_element_is_as_tall_as_the_shell_says() {
        let mut w = window(0, 0, vec![]);
        // Ten lines, lines 2..5 loaded two rows in; the shell laid them out 1, 3 and 2 rows tall.
        w.root = Element::Stack {
            children: vec![
                chrome("a.md"),
                prose(0, 2, 10, 2..5),
                chrome("b.rs"),
                editor(1, 0, 3, vec![]),
            ],
        };
        let m = measured(0, 2, &[1, 3, 2]);
        // 2 rows above the slice, 6 measured, 5 unloaded lines below at one each.
        assert_eq!(total_rows(&w.root, &m), 1 + (2 + 6 + 5) + 1 + 3);
        assert_eq!(element_start_row(&w, 1, &m), Some(VisualRow(1 + 13 + 1)));
        let rows: Vec<String> = painted_rows(&w, &m)
            .into_iter()
            .map(|(at, item)| match item {
                PaintedRow::Chrome(n) => format!("{at} chrome {}", chrome_text(n)),
                PaintedRow::Text { line, .. } => format!("{at} L{}", line.logical_line),
                PaintedRow::Baseline { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            rows,
            vec!["0 chrome a.md", "3 L2", "4 L3", "7 L4", "14 chrome b.rs"],
            "line 3 starts at row 4 and takes three rows, so line 4 starts at row 7"
        );
    }

    /// A row inside a tall line belongs to that line: the scroll anchor, a click and the cursor
    /// all resolve through the shell's layout.
    #[test]
    fn rows_inside_a_measured_line_resolve_to_it() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![chrome("a.md"), prose(0, 2, 10, 2..5)],
        };
        let m = measured(0, 2, &[1, 3, 2]);
        // Absolute rows 4, 5 and 6 are all line 3's.
        for r in 4..7 {
            let (element, line, _) = line_at_row(&w, VisualRow(r), &m);
            assert_eq!((element, line), (0, 3), "row {r}");
        }
        assert_eq!(anchor_at(&w, VisualRow(6), &m).line, 3);
        assert_eq!(
            hit_test(&w, 6, 0, 4, &m).map(|(e, p)| (e, p.line)),
            Some((0, 3))
        );
        assert_eq!(
            position_cell(&w, 0, LogicalPosition { line: 4, col: 0 }, 4, &m).map(|(r, _, _)| r),
            Some(VisualRow(7)),
            "the cursor on line 4 is drawn where the shell put line 4"
        );
        assert_eq!(line_top_row(&w, 0, 4, &m), Some(VisualRow(7)));
    }

    /// A fetch for a client-laid-out element asks by the rows the *server* numbers — lines —
    /// which the shell's layout translates from the rows on screen.
    #[test]
    fn slices_through_a_measured_element_name_its_lines() {
        let mut w = window(0, 0, vec![]);
        w.root = Element::Stack {
            children: vec![
                chrome("a.md"),
                prose(0, 2, 10, 2..5),
                chrome("b.rs"),
                editor(1, 0, 3, vec![]),
            ],
        };
        let m = measured(0, 2, &[1, 3, 2]);
        // A screen from absolute row 5 (inside line 3) to row 16 reaches the prose element and
        // then the editor below. The prose is asked for whole — every one of its ten lines from
        // wire row 0 — however little of it is on screen; the editor for the rows reached.
        let slices = slices_for(&w.root, VisualRow(5), 11, 0, &m);
        assert_eq!(
            slices
                .iter()
                .map(|s| (s.element, s.from_row.get(), s.rows))
                .collect::<Vec<_>>(),
            vec![(0, 0, 10), (1, 0, 1)],
            "the whole element, then the editor's reached row"
        );
        // Unmeasured, the prose is still whole; the editor's request is one row per line.
        let plain = slices_for(&w.root, VisualRow(5), 11, 0, &Measured::default());
        assert_eq!(
            plain
                .iter()
                .map(|s| (s.element, s.from_row.get(), s.rows))
                .collect::<Vec<_>>(),
            vec![(0, 0, 10), (1, 0, 3)]
        );
        // A screen entirely below it — the prose is 13 rows under its chrome — asks nothing of it.
        let below = slices_for(&w.root, VisualRow(15), 3, 0, &m);
        assert_eq!(below.iter().map(|s| s.element).collect::<Vec<_>>(), vec![1]);
    }
}
