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
        WrapMode::None => vec![visual_row_with_highlights(line_text, 0, highlights)],
        // TODO: distribute highlights across wrapped visual rows. The TUI uses WrapMode::None for
        // now, so highlights are dropped on the floor when wrapping is on.
        WrapMode::Soft => wrap_line(line_text, cols),
    };
    LogicalLineRender { logical_line, visual_rows }
}

fn visual_row_with_highlights(text: &str, continuation_indent: u32, highlights: Vec<Highlight>) -> VisualRow {
    VisualRow {
        continuation_indent,
        segments: vec![Segment { text: text.to_string(), highlights }],
    }
}

fn wrap_line(line: &str, cols: u32) -> Vec<VisualRow> {
    if line.is_empty() {
        return vec![visual_row("", 0)];
    }
    if cols == 0 {
        return vec![visual_row(line, 0)];
    }

    let lead: u32 = leading_whitespace_chars(line);
    // If the indent fills or exceeds the viewport width, continuation rows would have no room
    // for content — wrapping is meaningless. Emit a single row.
    if lead > 0 && lead >= cols {
        return vec![visual_row(line, 0)];
    }

    let mut rows: Vec<VisualRow> = Vec::new();
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
                    rows.push(visual_row(slice.trim_end(), row_indent));
                    row_start_byte = b;
                }
                _ => {
                    // Hard break at current position (no whitespace seen in this row).
                    rows.push(visual_row(&line[row_start_byte..byte_idx], row_indent));
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
        rows.push(visual_row(&line[row_start_byte..], row_indent));
    } else if rows.is_empty() {
        rows.push(visual_row("", 0));
    }
    rows
}

fn visual_row(text: &str, continuation_indent: u32) -> VisualRow {
    VisualRow {
        continuation_indent,
        segments: vec![Segment { text: text.to_string(), highlights: vec![] }],
    }
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
        let rows = wrap_line("", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "");
    }

    #[test]
    fn short_line_no_wrap() {
        let rows = wrap_line("hello", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "hello");
        assert_eq!(rows[0].continuation_indent, 0);
    }

    #[test]
    fn wraps_at_whitespace() {
        let rows = wrap_line("the quick brown fox", 10);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["the quick", "brown fox"]);
        assert_eq!(rows[0].continuation_indent, 0);
        assert_eq!(rows[1].continuation_indent, 0);
    }

    #[test]
    fn continuation_indent_from_leading_whitespace() {
        // Leading 4 spaces, cols=20 — wrapping should happen at whitespace and second row
        // should carry continuation_indent=4.
        let rows = wrap_line("    println!(\"hello world\");", 20);
        assert!(rows.len() >= 2);
        // First row keeps the original leading whitespace as content.
        assert!(row_text(&rows[0]).starts_with("    "));
        assert_eq!(rows[0].continuation_indent, 0);
        // Subsequent rows are marked with the leading-whitespace count.
        assert_eq!(rows[1].continuation_indent, 4);
    }

    #[test]
    fn hard_breaks_when_no_whitespace() {
        let rows = wrap_line("abcdefghijklmnop", 5);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn drops_break_whitespace() {
        // The space at the wrap point isn't carried into the next row.
        let rows = wrap_line("aaa bbb", 4);
        let texts: Vec<_> = rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["aaa", "bbb"]);
    }

    #[test]
    fn render_line_no_wrap_returns_single_row() {
        let r = render_line("anything goes here at all", 0, 5, WrapMode::None, vec![]);
        assert_eq!(r.visual_rows.len(), 1);
        assert_eq!(row_text(&r.visual_rows[0]), "anything goes here at all");
    }

    #[test]
    fn very_long_indent_falls_back_to_unwrapped() {
        // Indent is wider than cols; we don't try to wrap.
        let line = "                            content";
        let rows = wrap_line(line, 10);
        assert_eq!(rows.len(), 1);
    }
}
