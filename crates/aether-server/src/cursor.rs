//! Cursor motion resolution and position arithmetic.
//!
//! Positions are `(line, col_bytes)`. ropey indexes by char internally, so we round-trip through
//! char offsets for arithmetic. `count` in motions is in chars (Unicode scalars) for phase 1; a
//! grapheme-aware revision can come later.

use crate::state::Buffer;
use aether_protocol::cursor::{Direction, Motion};
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
        // Word and Visual* motions are not implemented in phase 1; cursor stays put.
        _ => current,
    }
}
