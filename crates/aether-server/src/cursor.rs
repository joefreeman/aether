//! Cursor motion resolution and position arithmetic.
//!
//! Positions are `(line, col_bytes)`. ropey indexes by char internally, so we round-trip through
//! char offsets for arithmetic. `count` in motions is in chars (Unicode scalars) for phase 1; a
//! grapheme-aware revision can come later.

use crate::state::Buffer;
use crate::wrap::{self, RowInfo};
use aether_protocol::cursor::{Direction, Motion, VerticalDirection, WordBoundary};
use aether_protocol::viewport::WrapMode;
use aether_protocol::LogicalPosition;

/// Convert a (line, byte-col) position to an absolute char index in the rope. Clamped to valid
/// positions in the buffer.
pub fn pos_to_char(buf: &Buffer, pos: LogicalPosition) -> usize {
    let line_count = buf.text.len_lines().max(1);
    let line_idx = (pos.line as usize).min(line_count - 1);
    let line_start_char = buf.text.line_to_char(line_idx);
    let line_slice = buf.text.line(line_idx);
    let byte_offset = (pos.col as usize).min(line_byte_len_excl_newline_slice(line_slice) as usize);
    let char_offset_in_line = line_slice.byte_to_char(byte_offset);
    line_start_char + char_offset_in_line
}

/// Convert an absolute char index back to a (line, byte-col) position.
pub fn char_to_pos(buf: &Buffer, char_idx: usize) -> LogicalPosition {
    let total = buf.text.len_chars();
    let char_idx = char_idx.min(total);
    let line_idx = buf.text.char_to_line(char_idx);
    let line_start_char = buf.text.line_to_char(line_idx);
    let char_offset = char_idx - line_start_char;
    let line_slice = buf.text.line(line_idx);
    let byte_offset = line_slice.char_to_byte(char_offset);
    LogicalPosition { line: line_idx as u32, col: byte_offset as u32 }
}

pub fn line_byte_len_excl_newline(buf: &Buffer, line_idx: u32) -> u32 {
    let slice = buf.text.line(line_idx as usize);
    line_byte_len_excl_newline_slice(slice)
}

fn line_byte_len_excl_newline_slice(slice: ropey::RopeSlice<'_>) -> u32 {
    let len = slice.len_bytes();
    if len > 0 && slice.byte(len - 1) == b'\n' {
        (len - 1) as u32
    } else {
        len as u32
    }
}

pub fn clamp_position(buf: &Buffer, pos: LogicalPosition) -> LogicalPosition {
    let line_count = buf.text.len_lines() as u32;
    let line = pos.line.min(line_count.saturating_sub(1));
    let col = pos.col.min(line_byte_len_excl_newline(buf, line));
    LogicalPosition { line, col }
}

pub fn ordered(a: LogicalPosition, b: LogicalPosition) -> (LogicalPosition, LogicalPosition) {
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

pub fn resolve_motion(buf: &Buffer, current: LogicalPosition, motion: &Motion) -> LogicalPosition {
    match motion {
        Motion::Char { direction, count } => {
            let cur_char = pos_to_char(buf, current);
            let new_char = match direction {
                Direction::Forward => cur_char
                    .saturating_add(*count as usize)
                    .min(buf.text.len_chars()),
                Direction::Backward => cur_char.saturating_sub(*count as usize),
            };
            char_to_pos(buf, new_char)
        }
        Motion::LogicalLine { direction, count, preserve_col } => {
            let line_count = buf.text.len_lines() as u32;
            let new_line = match direction {
                Direction::Forward => current.line.saturating_add(*count),
                Direction::Backward => current.line.saturating_sub(*count),
            };
            let new_line = new_line.min(line_count.saturating_sub(1));
            let new_col = if *preserve_col {
                current.col.min(line_byte_len_excl_newline(buf, new_line))
            } else {
                0
            };
            LogicalPosition { line: new_line, col: new_col }
        }
        Motion::LineStart => LogicalPosition { line: current.line, col: 0 },
        Motion::LineEnd => LogicalPosition {
            line: current.line,
            col: line_byte_len_excl_newline(buf, current.line),
        },
        Motion::LineFirstNonblank => {
            let slice = buf.text.line(current.line as usize);
            let mut byte_offset = 0usize;
            for c in slice.chars() {
                if c == '\n' || !matches!(c, ' ' | '\t') {
                    break;
                }
                byte_offset += c.len_utf8();
            }
            LogicalPosition { line: current.line, col: byte_offset as u32 }
        }
        Motion::BufferStart => LogicalPosition { line: 0, col: 0 },
        Motion::BufferEnd => char_to_pos(buf, buf.text.len_chars()),
        Motion::Goto { position } => clamp_position(buf, *position),
        Motion::Word { direction, count, boundary, exclusive } => {
            let orig_start = pos_to_char(buf, current);
            let mut end = match direction {
                Direction::Forward => {
                    // For exclusive forward, advance start by one char before computing. Without
                    // this, sitting right before a word boundary (the natural resting place of a
                    // previous exclusive press) would degenerate to a no-op: the naive
                    // `word_forward_start` would advance by exactly 1, then the exclusive
                    // subtraction below would undo it. The pre-advance ensures repeated presses
                    // keep making progress.
                    let s = if *exclusive {
                        (orig_start + 1).min(buf.text.len_chars())
                    } else {
                        orig_start
                    };
                    word_forward_start(&buf.text, s, *boundary, *count)
                }
                Direction::Backward => word_backward_start(&buf.text, orig_start, *boundary, *count),
            };
            // Exclusive forward stops one char before the destination word boundary, provided
            // the motion actually advanced.
            if *exclusive && matches!(direction, Direction::Forward) && end > orig_start {
                end -= 1;
            }
            char_to_pos(buf, end)
        }
        Motion::WordEnd { direction, count, boundary } => {
            let start = pos_to_char(buf, current);
            let end = match direction {
                Direction::Forward => word_forward_end(&buf.text, start, *boundary, *count),
                Direction::Backward => word_backward_end(&buf.text, start, *boundary, *count),
            };
            char_to_pos(buf, end)
        }
        // Visual motions are resolved separately by the cursor/move handler (they need viewport
        // state for wrap mode + width). resolve_motion is for buffer-only motions.
        Motion::VisualLine { .. } | Motion::VisualLineStart { .. } | Motion::VisualLineEnd { .. } => {
            current
        }
    }
}

/// Resolve a visual line motion: walk up or down by `count` visual rows under the given wrap
/// settings, preserving the cursor's visual column where possible. When `wrap` is `None` this
/// degenerates to a logical line step (each logical line is one visual row).
pub fn resolve_visual_line(
    buf: &Buffer,
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    current: LogicalPosition,
    direction: VerticalDirection,
    count: u32,
) -> LogicalPosition {
    if matches!(wrap, WrapMode::None) || cols == 0 {
        return logical_line_step(buf, current, direction, count);
    }

    let line_count = buf.text.len_lines() as u32;
    let mut current_line = current.line.min(line_count.saturating_sub(1));
    let mut rows = wrap::compute_rows(&line_text(buf, current_line), cols, marker_width);
    let mut row_idx = find_row_for_col(&rows, current.col as usize);
    let target_visual_col =
        visual_col_of_byte(&rows[row_idx], current.col as usize, marker_width);

    let mut remaining = count;
    while remaining > 0 {
        let advanced = match direction {
            VerticalDirection::Down => {
                if row_idx + 1 < rows.len() {
                    row_idx += 1;
                    true
                } else if current_line + 1 < line_count {
                    current_line += 1;
                    rows = wrap::compute_rows(&line_text(buf, current_line), cols, marker_width);
                    row_idx = 0;
                    true
                } else {
                    false
                }
            }
            VerticalDirection::Up => {
                if row_idx > 0 {
                    row_idx -= 1;
                    true
                } else if current_line > 0 {
                    current_line -= 1;
                    rows = wrap::compute_rows(&line_text(buf, current_line), cols, marker_width);
                    row_idx = rows.len().saturating_sub(1);
                    true
                } else {
                    false
                }
            }
        };
        if !advanced {
            break;
        }
        remaining -= 1;
    }

    let row = &rows[row_idx];
    let new_col_within_text = byte_at_visual_col(row, target_visual_col, marker_width);
    LogicalPosition {
        line: current_line,
        col: row.byte_offset as u32 + new_col_within_text as u32,
    }
}

/// Resolve VisualLineStart: cursor to the first byte of its current visual row.
pub fn resolve_visual_line_start(
    buf: &Buffer,
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, wrap, cols, marker_width, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    LogicalPosition { line: current.line, col: rows[row_idx].byte_offset as u32 }
}

/// Resolve VisualLineEnd: cursor to the last byte of its current visual row.
pub fn resolve_visual_line_end(
    buf: &Buffer,
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, wrap, cols, marker_width, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    let row = &rows[row_idx];
    let end_byte = row.byte_offset + row.text.len();
    LogicalPosition { line: current.line, col: end_byte as u32 }
}

fn wrap_rows_for_cursor(
    buf: &Buffer,
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    current: LogicalPosition,
) -> Vec<RowInfo> {
    let line_count = buf.text.len_lines() as u32;
    let line_idx = current.line.min(line_count.saturating_sub(1));
    if matches!(wrap, WrapMode::None) || cols == 0 {
        let text = line_text(buf, line_idx);
        let len = text.len();
        vec![RowInfo { byte_offset: 0, text, continuation_indent: 0 }]
            .into_iter()
            .map(|mut r| { r.text.truncate(len); r })
            .collect()
    } else {
        wrap::compute_rows(&line_text(buf, line_idx), cols, marker_width)
    }
}

fn line_text(buf: &Buffer, line_idx: u32) -> String {
    let line = buf.text.line(line_idx as usize);
    let mut text: String = line.chunks().collect();
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

fn find_row_for_col(rows: &[RowInfo], col: usize) -> usize {
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

/// Visual column of a byte position within a row, including the continuation marker (rendered
/// by the client on rows where `byte_offset > 0`) and the indent. Bytes beyond the row's
/// visible text clamp to the end of the visible text.
fn visual_col_of_byte(row: &RowInfo, col_in_line: usize, marker_width: u32) -> u32 {
    let relative = col_in_line.saturating_sub(row.byte_offset);
    let clamped = relative.min(row.text.len());
    row_prefix_width(row, marker_width) + clamped as u32
}

/// Inverse of `visual_col_of_byte`: byte offset *within the row's text* that lands at the
/// requested visual column. Visual columns inside the marker/indent prefix clamp to 0.
fn byte_at_visual_col(row: &RowInfo, visual_col: u32, marker_width: u32) -> usize {
    let prefix = row_prefix_width(row, marker_width);
    if visual_col <= prefix {
        return 0;
    }
    let target = (visual_col - prefix) as usize;
    target.min(row.text.len())
}

/// Total visual width the client prepends to a row before its text: continuation marker (only on
/// rows where `byte_offset > 0`) plus the row's continuation indent.
fn row_prefix_width(row: &RowInfo, marker_width: u32) -> u32 {
    let marker = if row.byte_offset > 0 { marker_width } else { 0 };
    marker + row.continuation_indent
}

fn logical_line_step(
    buf: &Buffer,
    current: LogicalPosition,
    direction: VerticalDirection,
    count: u32,
) -> LogicalPosition {
    let line_count = buf.text.len_lines() as u32;
    let new_line = match direction {
        VerticalDirection::Down => current.line.saturating_add(count),
        VerticalDirection::Up => current.line.saturating_sub(count),
    };
    let new_line = new_line.min(line_count.saturating_sub(1));
    let new_col = current.col.min(line_byte_len_excl_newline(buf, new_line));
    LogicalPosition { line: new_line, col: new_col }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharCat {
    Whitespace,
    Word,
    Symbol,
}

fn char_cat(c: char, boundary: WordBoundary) -> CharCat {
    if c.is_whitespace() {
        return CharCat::Whitespace;
    }
    match boundary {
        WordBoundary::BigWord => CharCat::Word, // any non-whitespace is a "WORD" char
        WordBoundary::Word | WordBoundary::Subword => {
            // Subword grouping (camelCase / snake_case splits) — phase 1 treats same as Word.
            if c.is_alphanumeric() || c == '_' {
                CharCat::Word
            } else {
                CharCat::Symbol
            }
        }
    }
}

fn word_forward_start(rope: &ropey::Rope, start: usize, boundary: WordBoundary, count: u32) -> usize {
    let total = rope.len_chars();
    let mut i = start;
    for _ in 0..count {
        if i >= total {
            return total;
        }
        // Skip the current run of same-category (non-whitespace) chars.
        let cat = char_cat(rope.char(i), boundary);
        if cat != CharCat::Whitespace {
            while i < total && char_cat(rope.char(i), boundary) == cat {
                i += 1;
            }
        }
        // Skip whitespace to reach the next word start.
        while i < total && char_cat(rope.char(i), boundary) == CharCat::Whitespace {
            i += 1;
        }
    }
    i
}

fn word_backward_start(rope: &ropey::Rope, start: usize, boundary: WordBoundary, count: u32) -> usize {
    let mut i = start;
    for _ in 0..count {
        if i == 0 {
            return 0;
        }
        i -= 1;
        // Skip whitespace backward.
        while i > 0 && char_cat(rope.char(i), boundary) == CharCat::Whitespace {
            i -= 1;
        }
        if char_cat(rope.char(i), boundary) == CharCat::Whitespace {
            // Reached start; the buffer begins with whitespace.
            return 0;
        }
        // Step back through the current run to its first char.
        let cat = char_cat(rope.char(i), boundary);
        while i > 0 && char_cat(rope.char(i - 1), boundary) == cat {
            i -= 1;
        }
    }
    i
}

fn word_forward_end(rope: &ropey::Rope, start: usize, boundary: WordBoundary, count: u32) -> usize {
    let total = rope.len_chars();
    let mut i = start;
    for _ in 0..count {
        if i >= total {
            return total;
        }
        // Move at least one char so successive `e` makes progress.
        i += 1;
        // Skip whitespace.
        while i < total && char_cat(rope.char(i), boundary) == CharCat::Whitespace {
            i += 1;
        }
        if i >= total {
            return total;
        }
        // Advance to the last char of the current run.
        let cat = char_cat(rope.char(i), boundary);
        while i + 1 < total && char_cat(rope.char(i + 1), boundary) == cat {
            i += 1;
        }
    }
    i
}

fn word_backward_end(rope: &ropey::Rope, start: usize, boundary: WordBoundary, count: u32) -> usize {
    // Vim's `ge` — back to end of previous word.
    let mut i = start;
    for _ in 0..count {
        if i == 0 {
            return 0;
        }
        i -= 1;
        while i > 0 && char_cat(rope.char(i), boundary) == CharCat::Whitespace {
            i -= 1;
        }
    }
    i
}
