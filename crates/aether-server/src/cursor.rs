//! Cursor motion resolution and position arithmetic.
//!
//! Positions are `(line, col_bytes)`. ropey indexes by char internally, so we round-trip through
//! char offsets for arithmetic. `count` in motions is in chars (Unicode scalars) for phase 1; a
//! grapheme-aware revision can come later.

use crate::state::Buffer;
use aether_protocol::cursor::{Direction, Motion, WordBoundary};
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
        // Visual* motions are not implemented in phase 1; cursor stays put.
        _ => current,
    }
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
