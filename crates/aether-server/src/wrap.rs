//! Soft-wrap algorithm.
//!
//! Whitespace-aware, with continuation indent matching the original line's leading whitespace.
//! "Column" is counted in `char`s (Unicode scalars); this is fine for ASCII / typical source code
//! but does not yet handle East-Asian wide characters or grapheme clusters spanning multiple
//! scalars. Refinement deferred.

use aether_protocol::viewport::{Highlight, LogicalLineRender, Segment, VisualRow, WrapMode};

pub fn render_line(
    line_text: &str,
    logical_line: u32,
    cols: u32,
    wrap: WrapMode,
    highlights: Vec<Highlight>,
) -> LogicalLineRender {
    let visual_rows = match wrap {
        WrapMode::None => vec![VisualRow {
            byte_offset: 0,
            continuation_indent: 0,
            segments: vec![Segment { text: line_text.to_string(), highlights }],
        }],
        WrapMode::Soft => wrap_line(line_text, cols, &highlights),
    };
    LogicalLineRender { logical_line, visual_rows }
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

fn wrap_line(line: &str, cols: u32, highlights: &[Highlight]) -> Vec<VisualRow> {
    let row_infos = compute_rows(line, cols);
    row_infos
        .into_iter()
        .map(|info| {
            let row_highlights = slice_highlights(highlights, info.byte_offset, info.text.len());
            VisualRow {
                byte_offset: info.byte_offset as u32,
                continuation_indent: info.continuation_indent,
                segments: vec![Segment { text: info.text, highlights: row_highlights }],
            }
        })
        .collect()
}

pub(crate) fn compute_rows(line: &str, cols: u32) -> Vec<RowInfo> {
    if line.is_empty() {
        return vec![RowInfo { byte_offset: 0, text: String::new(), continuation_indent: 0 }];
    }
    if cols == 0 {
        return vec![RowInfo { byte_offset: 0, text: line.to_string(), continuation_indent: 0 }];
    }

    let lead: u32 = leading_whitespace_chars(line);
    // If the indent fills or exceeds the viewport width, continuation rows would have no room
    // for content — wrapping is meaningless. Emit a single row.
    if lead > 0 && lead >= cols {
        return vec![RowInfo { byte_offset: 0, text: line.to_string(), continuation_indent: 0 }];
    }

    let mut rows: Vec<RowInfo> = Vec::new();
    let mut row_start_byte: usize = 0;
    let mut chars_in_row: u32 = 0;
    // Byte index *after* the last seen whitespace, candidate break point.
    let mut last_break_byte: Option<usize> = None;
    let mut iter = line.char_indices().peekable();

    while let Some(&(byte_idx, c)) = iter.peek() {
        let is_first = rows.is_empty();
        let max_chars = if is_first { cols } else { cols.saturating_sub(lead) };
        let max_chars = max_chars.max(1);

        if chars_in_row >= max_chars {
            let row_indent = if is_first { 0 } else { lead };
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
            chars_in_row = 0;
            last_break_byte = None;
            continue; // re-evaluate `c` against the new (empty) row
        }

        chars_in_row += 1;
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
        rows.push(RowInfo { byte_offset: 0, text: String::new(), continuation_indent: 0 });
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

fn leading_whitespace_chars(line: &str) -> u32 {
    line.chars().take_while(|c| matches!(c, ' ' | '\t')).count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(row: &VisualRow) -> String {
        row.segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn empty_line_one_empty_row() {
        let rows = wrap_line("", 10, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "");
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn short_line_no_wrap() {
        let rows = wrap_line("hello", 10, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "hello");
        assert_eq!(rows[0].continuation_indent, 0);
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn wraps_at_whitespace() {
        let rows = wrap_line("the quick brown fox", 10, &[]);
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
        let rows = wrap_line("    println!(\"hello world\");", 20, &[]);
        assert!(rows.len() >= 2);
        // First row keeps the original leading whitespace as content.
        assert!(row_text(&rows[0]).starts_with("    "));
        assert_eq!(rows[0].continuation_indent, 0);
        // Subsequent rows are marked with the leading-whitespace count.
        assert_eq!(rows[1].continuation_indent, 4);
    }

    #[test]
    fn hard_breaks_when_no_whitespace() {
        let rows = wrap_line("abcdefghijklmnop", 5, &[]);
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
        let rows = wrap_line("aaa bbb", 4, &[]);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["aaa", "bbb"]);
        // Row 0 visible text is "aaa" (bytes 0..3), break whitespace is byte 3, row 1 starts at 4.
        assert_eq!(rows[0].byte_offset, 0);
        assert_eq!(rows[1].byte_offset, 4);
    }

    #[test]
    fn render_line_no_wrap_returns_single_row() {
        let r = render_line("anything goes here at all", 0, 5, WrapMode::None, vec![]);
        assert_eq!(r.visual_rows.len(), 1);
        assert_eq!(row_text(&r.visual_rows[0]), "anything goes here at all");
        assert_eq!(r.visual_rows[0].byte_offset, 0);
    }

    #[test]
    fn very_long_indent_falls_back_to_unwrapped() {
        // Indent is wider than cols; we don't try to wrap.
        let line = "                            content";
        let rows = wrap_line(line, 10, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].byte_offset, 0);
    }

    #[test]
    fn highlights_clipped_to_visual_rows() {
        // "the quick brown fox" wraps at 10 → ["the quick", "brown fox"] with row[1] starting
        // at byte 10. A highlight on "brown" (bytes 10..15) should land entirely on row 1 with
        // row-relative offsets [0, 5).
        let highlights = vec![Highlight { start: 10, end: 15, kind: "keyword".into() }];
        let rows = wrap_line("the quick brown fox", 10, &highlights);
        let row0_hl = &rows[0].segments[0].highlights;
        let row1_hl = &rows[1].segments[0].highlights;
        assert!(row0_hl.is_empty());
        assert_eq!(row1_hl.len(), 1);
        assert_eq!(row1_hl[0].start, 0);
        assert_eq!(row1_hl[0].end, 5);
        assert_eq!(row1_hl[0].kind, "keyword");
    }

    #[test]
    fn highlights_split_across_rows() {
        // Highlight spans the wrap point: bytes 6..14 covers "uick" + " " + "bro".
        // Row 0 visible "the quick" (0..9), row 1 visible "brown fox" (starts at 10).
        // After clipping: row 0 highlight = [6, 9), row 1 highlight = [0, 4).
        let highlights = vec![Highlight { start: 6, end: 14, kind: "string".into() }];
        let rows = wrap_line("the quick brown fox", 10, &highlights);
        let row0_hl = &rows[0].segments[0].highlights;
        let row1_hl = &rows[1].segments[0].highlights;
        assert_eq!(row0_hl.len(), 1);
        assert_eq!((row0_hl[0].start, row0_hl[0].end), (6, 9));
        assert_eq!(row1_hl.len(), 1);
        assert_eq!((row1_hl[0].start, row1_hl[0].end), (0, 4));
    }
}
