//! Pure mapping between protocol coordinates and the monospace cell grid.
//!
//! The server renders a `Window` of logical lines, each split into `VisualRow`s by its soft-wrap
//! math; positions on the wire are `(logical line, byte col)`. Everything pixel-ish in the client
//! reduces to *(absolute visual row, display column)* cells, so this module owns that translation:
//! cursor → cell, mouse cell → position, selection → per-row display-column spans. Display-column
//! math mirrors the server's: tabs advance to the next `tab_width` stop, other chars take their
//! Unicode width. Continuation rows are prefixed by the wrap marker ("↪ ") plus the row's
//! continuation indent, same as the web client.

use aether_protocol::viewport::{LogicalLineRender, VisualRow, Window};
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
pub fn row_prefix_cols(row: &VisualRow) -> u32 {
    if row.byte_offset == 0 {
        0
    } else {
        CONTINUATION_MARKER_COLS + row.continuation_indent
    }
}

/// Walk a visual row's chars as grid cells.
pub fn row_cells(row: &VisualRow, tab_width: u32) -> Vec<Cell<'_>> {
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
pub fn row_end_byte(row: &VisualRow) -> u32 {
    row.byte_offset
        + row
            .segments
            .iter()
            .map(|s| s.text.len() as u32)
            .sum::<u32>()
}

/// Visual rows a line occupies: its phantom deleted rows (inline diff view) plus its (possibly
/// wrapped) content rows.
pub fn line_rows(line: &LogicalLineRender) -> u32 {
    (line.virtual_rows_above.len() + line.visual_rows.len()) as u32
}

/// The window-relative index of the line's first visual row — phantom rows included, so this
/// points at the top of the line's whole block. `None` when the line isn't loaded. Absolute
/// visual row = `window.first_visual_row + this`.
pub fn rows_before_line(window: &Window, logical_line: u32) -> Option<u32> {
    if logical_line < window.first_logical_line
        || logical_line >= window.last_logical_line_exclusive
    {
        return None;
    }
    let mut rows = 0u32;
    for line in &window.lines {
        if line.logical_line == logical_line {
            return Some(rows);
        }
        rows += line_rows(line);
    }
    None
}

/// The logical line owning absolute visual `abs_row`, and how many of that line's visual rows sit
/// above it (the sub-row offset into the line — phantom diff rows included). Clamps to the loaded
/// window's last row when `abs_row` is past it. The inverse of [`rows_before_line`] + first row.
pub fn line_at_row(window: &Window, abs_row: u32) -> (u32, u32) {
    let mut rel = abs_row.saturating_sub(window.first_visual_row);
    for line in &window.lines {
        let h = line_rows(line);
        if rel < h {
            return (line.logical_line, rel);
        }
        rel -= h;
    }
    match window.lines.last() {
        Some(l) => (l.logical_line, line_rows(l).saturating_sub(1)),
        None => (window.first_logical_line, 0),
    }
}

/// A scroll position pinned to *content* rather than an absolute visual row, so it survives a
/// re-layout (wrap toggle, diff toggle) that changes how many visual rows lines occupy. Captured
/// from the current window before the toggle, resolved against the new window after it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollAnchor {
    /// The cursor was visible: keep it at this row offset below the top of the viewport.
    Cursor { screen_row_offset: u32 },
    /// The cursor was off-screen: keep this logical line's `sub_row`-th visual row at the top.
    Line { logical_line: u32, sub_row: u32 },
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
    top_row: u32,
    viewport_rows: u32,
    cursor: LogicalPosition,
    tab_width: u32,
) -> ScrollAnchor {
    if let Some((cursor_row, _, _)) = position_cell(window, cursor, tab_width) {
        if cursor_row >= top_row && cursor_row < top_row + viewport_rows {
            return ScrollAnchor::Cursor {
                screen_row_offset: cursor_row - top_row,
            };
        }
    }
    let (logical_line, sub_row) = line_at_row(window, top_row);
    ScrollAnchor::Line {
        logical_line,
        sub_row,
    }
}

/// Resolve a captured anchor against the (post-toggle) window into a new absolute top visual row.
/// `cursor` is re-read here because the cursor's visual row moves under the new layout.
pub fn resolve_scroll_anchor(
    window: &Window,
    anchor: ScrollAnchor,
    cursor: LogicalPosition,
    tab_width: u32,
) -> u32 {
    match anchor {
        ScrollAnchor::Cursor { screen_row_offset } => {
            let cursor_row = position_cell(window, cursor, tab_width)
                .map(|(row, _, _)| row)
                .unwrap_or(window.first_visual_row);
            cursor_row.saturating_sub(screen_row_offset)
        }
        ScrollAnchor::Line {
            logical_line,
            sub_row,
        } => {
            let Some(rel) = rows_before_line(window, logical_line) else {
                return window.first_visual_row;
            };
            // Wrap may have shrunk the line; clamp the sub-row into its new height.
            let height = window
                .lines
                .iter()
                .find(|l| l.logical_line == logical_line)
                .map(line_rows)
                .unwrap_or(1);
            window.first_visual_row + rel + sub_row.min(height.saturating_sub(1))
        }
    }
}

/// Locate a position's grid cell: `(absolute visual row, display col, width)`. The width covers
/// the char under a block cursor; a position past the line's last char (Insert mode at EOL, or
/// the empty line) gets a 1-col cell just past the text. `None` when the line isn't loaded.
pub fn position_cell(
    window: &Window,
    pos: LogicalPosition,
    tab_width: u32,
) -> Option<(u32, u32, u32)> {
    let line = window.lines.iter().find(|l| l.logical_line == pos.line)?;
    // The cursor never lands on phantom rows; content starts below them.
    let line_start = window.first_visual_row
        + rows_before_line(window, pos.line)?
        + line.virtual_rows_above.len() as u32;
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
            return Some((line_start + row_idx as u32, cell.dcol, cell.width));
        }
        if cell.byte > pos.col {
            // Position inside a multi-byte char; snap to that char's cell.
            return Some((line_start + row_idx as u32, cell.dcol, cell.width));
        }
    }
    // Past the row's text: the virtual cell after the last char.
    let dcol = row_cells(row, tab_width)
        .last()
        .map(|c| c.dcol + c.width)
        .unwrap_or_else(|| row_prefix_cols(row));
    Some((line_start + row_idx as u32, dcol, 1))
}

/// Map a grid cell back to a buffer position — the mouse path. Rows above/below the loaded
/// window clamp to its first/last row; a display col past the row's text maps to just past the
/// last char (the server clamps to the line end). `None` only when the window has no lines.
pub fn hit_test(
    window: &Window,
    abs_row: i64,
    dcol: u32,
    tab_width: u32,
) -> Option<LogicalPosition> {
    let rel = (abs_row - window.first_visual_row as i64).max(0) as u32;
    let mut remaining = rel;
    let mut target: Option<(&LogicalLineRender, &VisualRow)> = None;
    'outer: for line in &window.lines {
        // Phantom deleted rows have no cursor position — a click on one snaps to the first
        // content row of the line they render above.
        let virtuals = line.virtual_rows_above.len() as u32;
        if remaining < virtuals {
            if let Some(row) = line.visual_rows.first() {
                target = Some((line, row));
                break 'outer;
            }
        }
        remaining -= virtuals.min(remaining);
        for row in &line.visual_rows {
            if remaining == 0 {
                target = Some((line, row));
                break 'outer;
            }
            remaining -= 1;
        }
    }
    // Past the loaded window: clamp to its last row.
    let (line, row) = match target {
        Some(t) => t,
        None => {
            let line = window.lines.last()?;
            (line, line.visual_rows.last()?)
        }
    };
    for cell in row_cells(row, tab_width) {
        if dcol < cell.dcol + cell.width {
            return Some(LogicalPosition {
                line: line.logical_line,
                col: cell.byte,
            });
        }
    }
    Some(LogicalPosition {
        line: line.logical_line,
        col: row_end_byte(row),
    })
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
/// On lines that end *inside* the selection, the span extends one col past the last char so the
/// consumed newline is visible.
pub fn row_selection_span(
    line_no: u32,
    row: &VisualRow,
    is_last_row_of_line: bool,
    min: LogicalPosition,
    max: LogicalPosition,
    tab_width: u32,
) -> Option<(u32, u32)> {
    if line_no < min.line || line_no > max.line {
        return None;
    }
    let cells = row_cells(row, tab_width);
    let Some(last) = cells.last() else {
        // Empty line strictly inside the selection: show its consumed newline as one cell.
        let p = row_prefix_cols(row);
        return Some((p, p + 1));
    };
    let row_start = row.byte_offset;
    let row_end = row_end_byte(row);
    // The selection's inclusive byte range on this line; u32::MAX = "through the newline".
    let sel_start = if line_no == min.line { min.col } else { 0 };
    let sel_end = if line_no == max.line {
        max.col
    } else {
        u32::MAX
    };
    if sel_end < row_start {
        return None;
    }
    if sel_start >= row_end {
        // Starts past this row's text. The one visible case: the selection begins exactly on
        // this line's newline and continues to a later line — show the newline cell.
        return (line_no < max.line && is_last_row_of_line && sel_start == row_end)
            .then(|| (last.dcol + last.width, last.dcol + last.width + 1));
    }
    let start_dcol = cells
        .iter()
        .find(|c| c.byte >= sel_start)
        .map(|c| c.dcol)
        .unwrap_or(last.dcol);
    let mut end_dcol = cells
        .iter()
        .rev()
        .find(|c| c.byte <= sel_end)
        .map(|c| c.dcol + c.width)?;
    // The newline consumed by a selection continuing past this line: one extra col, drawn on
    // the line's last visual row.
    if line_no < max.line && is_last_row_of_line {
        end_dcol += 1;
    }
    (end_dcol > start_dcol).then_some((start_dcol, end_dcol))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::viewport::{Highlight, Segment};

    fn row(byte_offset: u32, indent: u32, text: &str) -> VisualRow {
        VisualRow {
            byte_offset,
            continuation_indent: indent,
            segments: vec![Segment {
                text: text.into(),
                highlights: vec![],
            }],
        }
    }

    fn line(logical_line: u32, rows: Vec<VisualRow>) -> LogicalLineRender {
        LogicalLineRender {
            logical_line,
            visual_rows: rows,
            search_matches: vec![],
            virtual_rows_above: vec![],
            diff_marker: None,
            diff_stage: Default::default(),
            diagnostics: vec![],
            sneak_targets: vec![],
        }
    }

    fn window(first_logical: u32, first_visual: u32, lines: Vec<LogicalLineRender>) -> Window {
        let last = first_logical + lines.len() as u32;
        Window {
            first_logical_line: first_logical,
            last_logical_line_exclusive: last,
            line_count: 100,
            max_scroll_logical_line: 99,
            total_visual_rows: 120,
            first_visual_row: first_visual,
            max_line_width: 0,
            git_status: None,
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
        let r = VisualRow {
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
            position_cell(&w, LogicalPosition { line: 10, col: 12 }, 4).unwrap();
        assert_eq!(abs, 21);
        assert_eq!(dcol, CONTINUATION_MARKER_COLS + 2);
        assert_eq!(width, 1);
        // Line 11 starts after line 10's two rows.
        let (abs, dcol, _) = position_cell(&w, LogicalPosition { line: 11, col: 0 }, 4).unwrap();
        assert_eq!((abs, dcol), (22, 0));
        // Past EOL → virtual cell after the text.
        let (_, dcol, width) = position_cell(&w, LogicalPosition { line: 11, col: 5 }, 4).unwrap();
        assert_eq!((dcol, width), (5, 1));
        // Outside the window → None.
        assert!(position_cell(&w, LogicalPosition { line: 9, col: 0 }, 4).is_none());
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
            Some(LogicalPosition { line: 10, col: 12 })
        );
        assert_eq!(
            hit_test(&w, 21, 0, 4),
            Some(LogicalPosition { line: 10, col: 10 })
        );
        // Click past a row's end → just past its last char.
        assert_eq!(
            hit_test(&w, 22, 40, 4),
            Some(LogicalPosition { line: 11, col: 5 })
        );
        // Above the window clamps to its first row; far below to its last.
        assert_eq!(
            hit_test(&w, 3, 0, 4),
            Some(LogicalPosition { line: 10, col: 0 })
        );
        assert_eq!(
            hit_test(&w, 999, 0, 4),
            Some(LogicalPosition { line: 11, col: 0 })
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
    fn virtual_rows_count_but_hold_no_cursor() {
        use aether_protocol::viewport::{VirtualRow, VirtualRowKind};
        let mut l10 = line(10, vec![row(0, 0, "content")]);
        l10.virtual_rows_above = vec![
            VirtualRow {
                text: "removed 1".into(),
                kind: VirtualRowKind::Deleted,
                stage: Default::default(),
            },
            VirtualRow {
                text: "removed 2".into(),
                kind: VirtualRowKind::Deleted,
                stage: Default::default(),
            },
        ];
        let w = window(10, 20, vec![l10, line(11, vec![row(0, 0, "next")])]);
        // Line 11 starts after line 10's block: 2 phantoms + 1 content row.
        assert_eq!(rows_before_line(&w, 11), Some(3));
        // The cursor's cell skips the phantoms: line 10 col 0 sits at abs row 22.
        let (abs, dcol, _) = position_cell(&w, LogicalPosition { line: 10, col: 0 }, 4).unwrap();
        assert_eq!((abs, dcol), (22, 0));
        // Clicking a phantom row snaps to the line's first content row, keeping the column.
        assert_eq!(
            hit_test(&w, 20, 3, 4),
            Some(LogicalPosition { line: 10, col: 3 })
        );
        assert_eq!(
            hit_test(&w, 22, 2, 4),
            Some(LogicalPosition { line: 10, col: 2 })
        );
        assert_eq!(
            hit_test(&w, 23, 0, 4),
            Some(LogicalPosition { line: 11, col: 0 })
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
        assert_eq!(line_at_row(&w, 100), (5, 0));
        assert_eq!(line_at_row(&w, 101), (5, 1)); // 2nd row of line 5
        assert_eq!(line_at_row(&w, 102), (6, 0));
        assert_eq!(line_at_row(&w, 104), (7, 1)); // 2nd row of line 7
                                                  // Past the loaded window clamps to the last line's last row.
        assert_eq!(line_at_row(&w, 999), (7, 2));
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
            capture_scroll_anchor(&w, 100, 5, cursor, 4),
            ScrollAnchor::Cursor {
                screen_row_offset: 2
            }
        );
        // Cursor off-screen (viewport [100, 101)) → pin the top line + sub-row.
        assert_eq!(
            capture_scroll_anchor(&w, 101, 1, cursor, 4),
            ScrollAnchor::Line {
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
            cursor,
            4,
        );
        assert_eq!(row, 101);
        // Line anchor for line 6 → its first row (103), sub-row clamped into the line.
        assert_eq!(
            resolve_scroll_anchor(
                &after,
                ScrollAnchor::Line {
                    logical_line: 6,
                    sub_row: 0
                },
                cursor,
                4,
            ),
            103
        );
    }
}
