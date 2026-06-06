//! Soft-wrap algorithm.
//!
//! Whitespace-aware, with continuation indent matching the original line's leading whitespace.
//! Columns are visual columns: tabs advance to the next tab stop (per `tab_width`) and other
//! chars use `UnicodeWidthChar` (so ASCII is 1, common wide chars are 2, control / unknowns 0).

use aether_protocol::viewport::{Highlight, LogicalLineRender, Segment, VisualRow, WrapMode};
use unicode_width::UnicodeWidthChar;

/// Visual cols a single char contributes when rendered at `current_col`. Tabs advance to the
/// next multiple of `tab_width` (so `\t` at col 5 with tab_width=4 takes 3 cols, landing the
/// cursor at col 8); everything else falls through to `UnicodeWidthChar`. Shared between wrap
/// math and cursor positioning so a buffer renders the same way it indexes.
pub(crate) fn char_display_width(c: char, current_col: u32, tab_width: u32) -> u32 {
    if c == '\t' {
        if tab_width == 0 {
            0
        } else {
            tab_width - (current_col % tab_width)
        }
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0) as u32
    }
}

pub fn render_line(
    line_text: &str,
    logical_line: u32,
    cols: u32,
    wrap: WrapMode,
    marker_width: u32,
    tab_width: u32,
    highlights: Vec<Highlight>,
) -> LogicalLineRender {
    let visual_rows = match wrap {
        WrapMode::None => vec![VisualRow {
            byte_offset: 0,
            continuation_indent: 0,
            segments: vec![Segment {
                text: line_text.to_string(),
                highlights,
            }],
        }],
        WrapMode::Soft => wrap_line(line_text, cols, marker_width, tab_width, &highlights),
    };
    LogicalLineRender {
        logical_line,
        visual_rows,
        search_matches: Vec::new(),
        virtual_rows_above: Vec::new(),
        diff_marker: None,
        diagnostics: Vec::new(),
    }
}

/// One physical row of wrapped output along with the byte range of the logical line it covers.
/// Separated from `VisualRow` so the wrap algorithm can be tested without depending on the wire
/// type, and so cursor-motion code can consume it directly.
pub(crate) struct RowInfo {
    /// Byte offset of `text` within the logical line.
    pub byte_offset: usize,
    pub text: String,
    pub continuation_indent: u32,
}

fn wrap_line(
    line: &str,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
    highlights: &[Highlight],
) -> Vec<VisualRow> {
    let row_infos = compute_rows(line, cols, marker_width, tab_width);
    row_infos
        .into_iter()
        .map(|info| {
            let row_highlights = slice_highlights(highlights, info.byte_offset, info.text.len());
            VisualRow {
                byte_offset: info.byte_offset as u32,
                continuation_indent: info.continuation_indent,
                segments: vec![Segment {
                    text: info.text,
                    highlights: row_highlights,
                }],
            }
        })
        .collect()
}

pub(crate) fn compute_rows(
    line: &str,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
) -> Vec<RowInfo> {
    if line.is_empty() {
        return vec![RowInfo {
            byte_offset: 0,
            text: String::new(),
            continuation_indent: 0,
        }];
    }
    if cols == 0 {
        return vec![RowInfo {
            byte_offset: 0,
            text: line.to_string(),
            continuation_indent: 0,
        }];
    }

    let lead: u32 = leading_whitespace_cols(line, tab_width);
    // Continuation rows have `cols - lead - marker_width` of usable width. If that's non-positive
    // there's no room for content on a continuation, so wrapping is meaningless — emit a single
    // row and let the client draw it (terminal will clip the overflow).
    if lead.saturating_add(marker_width) >= cols {
        return vec![RowInfo {
            byte_offset: 0,
            text: line.to_string(),
            continuation_indent: 0,
        }];
    }

    let mut rows: Vec<RowInfo> = Vec::new();
    let mut row_start_byte: usize = 0;
    // Visual cols accumulated in the current row (tabs advance by tab-stop math; wide chars
    // count as 2; ASCII as 1).
    let mut row_cols: u32 = 0;
    // Byte index *after* the last seen whitespace, candidate break point.
    let mut last_break_byte: Option<usize> = None;
    let mut iter = line.char_indices().peekable();

    while let Some(&(byte_idx, c)) = iter.peek() {
        let is_first = rows.is_empty();
        let max_cols = if is_first {
            cols
        } else {
            cols.saturating_sub(lead).saturating_sub(marker_width)
        };
        let max_cols = max_cols.max(1);

        let row_indent = if is_first { 0 } else { lead };
        // Visual cols this char would add at this point in the row.
        let step = char_display_width(c, row_indent + row_cols, tab_width);

        if row_cols + step > max_cols {
            match last_break_byte {
                Some(b) if b > row_start_byte => {
                    let slice = &line[row_start_byte..b];
                    rows.push(RowInfo {
                        byte_offset: row_start_byte,
                        text: slice.trim_end().to_string(),
                        continuation_indent: row_indent,
                    });
                    row_start_byte = b;
                }
                _ => {
                    // Hard break at current position (no whitespace seen in this row).
                    rows.push(RowInfo {
                        byte_offset: row_start_byte,
                        text: line[row_start_byte..byte_idx].to_string(),
                        continuation_indent: row_indent,
                    });
                    row_start_byte = byte_idx;
                }
            }
            row_cols = 0;
            last_break_byte = None;
            continue; // re-evaluate `c` against the new (empty) row
        }

        row_cols += step;
        iter.next();
        if c == ' ' || c == '\t' {
            last_break_byte = Some(byte_idx + c.len_utf8());
        }
    }

    if row_start_byte < line.len() {
        let row_indent = if rows.is_empty() { 0 } else { lead };
        rows.push(RowInfo {
            byte_offset: row_start_byte,
            text: line[row_start_byte..].to_string(),
            continuation_indent: row_indent,
        });
    } else if rows.is_empty() {
        rows.push(RowInfo {
            byte_offset: 0,
            text: String::new(),
            continuation_indent: 0,
        });
    }
    rows
}

/// Clip `highlights` to the byte range `[row_byte_offset, row_byte_offset + row_text_len)`,
/// re-basing offsets to be row-relative.
fn slice_highlights(
    highlights: &[Highlight],
    row_byte_offset: usize,
    row_text_len: usize,
) -> Vec<Highlight> {
    let row_end = row_byte_offset + row_text_len;
    let mut out = Vec::new();
    for h in highlights {
        let start = h.start as usize;
        let end = h.end as usize;
        if end <= row_byte_offset || start >= row_end {
            continue;
        }
        let clipped_start = start.max(row_byte_offset);
        let clipped_end = end.min(row_end);
        out.push(Highlight {
            start: (clipped_start - row_byte_offset) as u32,
            end: (clipped_end - row_byte_offset) as u32,
            kind: h.kind.clone(),
        });
    }
    out
}

/// Visual-column width of the leading-whitespace run on `line`. Tabs expand to the next tab
/// stop, so `\t\t` with `tab_width=4` reports 8, not 2 — continuation rows then align to where
/// the rendered text would actually start.
fn leading_whitespace_cols(line: &str, tab_width: u32) -> u32 {
    let mut col = 0;
    for c in line.chars().take_while(|c| matches!(c, ' ' | '\t')) {
        col += char_display_width(c, col, tab_width);
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(row: &VisualRow) -> String {
        row.segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn empty_line_one_empty_row() {
        let rows = wrap_line("", 10, 0, 4, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "");
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn short_line_no_wrap() {
        let rows = wrap_line("hello", 10, 0, 4, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "hello");
        assert_eq!(rows[0].continuation_indent, 0);
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn wraps_at_whitespace() {
        let rows = wrap_line("the quick brown fox", 10, 0, 4, &[]);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["the quick", "brown fox"]);
        assert_eq!(rows[0].continuation_indent, 0);
        assert_eq!(rows[1].continuation_indent, 0);
        assert_eq!(rows[0].byte_offset, 0);
        // Row 1 starts right after the break whitespace.
        assert_eq!(rows[1].byte_offset, 10);
    }

    #[test]
    fn continuation_indent_from_leading_whitespace() {
        // Leading 4 spaces, cols=20 — wrapping should happen at whitespace and second row
        // should carry continuation_indent=4.
        let rows = wrap_line("    println!(\"hello world\");", 20, 0, 4, &[]);
        assert!(rows.len() >= 2);
        // First row keeps the original leading whitespace as content.
        assert!(row_text(&rows[0]).starts_with("    "));
        assert_eq!(rows[0].continuation_indent, 0);
        // Subsequent rows are marked with the leading-whitespace count.
        assert_eq!(rows[1].continuation_indent, 4);
    }

    #[test]
    fn hard_breaks_when_no_whitespace() {
        let rows = wrap_line("abcdefghijklmnop", 5, 0, 4, &[]);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["abcde", "fghij", "klmno", "p"]);
        // Hard breaks are contiguous — no whitespace dropped.
        assert_eq!(rows[0].byte_offset, 0);
        assert_eq!(rows[1].byte_offset, 5);
        assert_eq!(rows[2].byte_offset, 10);
        assert_eq!(rows[3].byte_offset, 15);
    }

    #[test]
    fn drops_break_whitespace() {
        // The space at the wrap point isn't carried into the next row.
        let rows = wrap_line("aaa bbb", 4, 0, 4, &[]);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["aaa", "bbb"]);
        // Row 0 visible text is "aaa" (bytes 0..3), break whitespace is byte 3, row 1 starts at 4.
        assert_eq!(rows[0].byte_offset, 0);
        assert_eq!(rows[1].byte_offset, 4);
    }

    #[test]
    fn render_line_no_wrap_returns_single_row() {
        let r = render_line(
            "anything goes here at all",
            0,
            5,
            WrapMode::None,
            0,
            4,
            vec![],
        );
        assert_eq!(r.visual_rows.len(), 1);
        assert_eq!(row_text(&r.visual_rows[0]), "anything goes here at all");
        assert_eq!(r.visual_rows[0].byte_offset, 0);
    }

    #[test]
    fn very_long_indent_falls_back_to_unwrapped() {
        // Indent is wider than cols; we don't try to wrap.
        let line = "                            content";
        let rows = wrap_line(line, 10, 0, 4, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn highlights_clipped_to_visual_rows() {
        // "the quick brown fox" wraps at 10 → ["the quick", "brown fox"] with row[1] starting
        // at byte 10. A highlight on "brown" (bytes 10..15) should land entirely on row 1 with
        // row-relative offsets [0, 5).
        let highlights = vec![Highlight {
            start: 10,
            end: 15,
            kind: "keyword".into(),
        }];
        let rows = wrap_line("the quick brown fox", 10, 0, 4, &highlights);
        let row0_hl = &rows[0].segments[0].highlights;
        let row1_hl = &rows[1].segments[0].highlights;
        assert!(row0_hl.is_empty());
        assert_eq!(row1_hl.len(), 1);
        assert_eq!(row1_hl[0].start, 0);
        assert_eq!(row1_hl[0].end, 5);
        assert_eq!(row1_hl[0].kind, "keyword");
    }

    #[test]
    fn tab_counts_as_visual_width_for_wrap() {
        // `\thello world` with tab_width=4 starts at visual col 4 ("\t" = 4 cols), then 11
        // chars of text → 15 cols total. cols=10 should force a wrap, and the break point
        // should respect tab expansion (not raw char count).
        let rows = wrap_line("\thello world", 10, 0, 4, &[]);
        assert!(rows.len() >= 2, "expected wrap, got {} row(s)", rows.len());
        // First row begins with the tab and breaks at whitespace.
        let first = row_text(&rows[0]);
        assert!(first.starts_with('\t'));
        assert!(!first.contains("world"));
    }

    #[test]
    fn tab_only_indent_advances_continuation_correctly() {
        // Two tabs of leading whitespace = 8 visual cols. Continuation rows should report
        // continuation_indent=8, matching where the rendered text would actually start.
        let rows = wrap_line(
            "\t\tthis is a longer body that needs wrapping",
            30,
            0,
            4,
            &[],
        );
        assert!(rows.len() >= 2);
        assert_eq!(rows[1].continuation_indent, 8);
    }

    #[test]
    fn marker_width_reduces_continuation_row_width() {
        // With cols=10 and marker_width=2, continuation rows have effective width 8 (10 - 0
        // leading - 2 marker). "the quick brown fox" wraps into three rows.
        let rows = wrap_line("the quick brown fox", 10, 2, 4, &[]);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["the quick", "brown", "fox"]);
        assert_eq!(rows[0].byte_offset, 0);
        assert_eq!(rows[1].byte_offset, 10);
        assert_eq!(rows[2].byte_offset, 16);
    }

    #[test]
    fn highlights_split_across_rows() {
        // Highlight spans the wrap point: bytes 6..14 covers "uick" + " " + "bro".
        // Row 0 visible "the quick" (0..9), row 1 visible "brown fox" (starts at 10).
        // After clipping: row 0 highlight = [6, 9), row 1 highlight = [0, 4).
        let highlights = vec![Highlight {
            start: 6,
            end: 14,
            kind: "string".into(),
        }];
        let rows = wrap_line("the quick brown fox", 10, 0, 4, &highlights);
        let row0_hl = &rows[0].segments[0].highlights;
        let row1_hl = &rows[1].segments[0].highlights;
        assert_eq!(row0_hl.len(), 1);
        assert_eq!((row0_hl[0].start, row0_hl[0].end), (6, 9));
        assert_eq!(row1_hl.len(), 1);
        assert_eq!((row1_hl[0].start, row1_hl[0].end), (0, 4));
    }
}
