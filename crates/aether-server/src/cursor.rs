//! Cursor motion resolution and position arithmetic.
//!
//! Positions are `(line, col_bytes)`. ropey indexes by char internally, so we round-trip through
//! char offsets for arithmetic. `count` in motions is in chars (Unicode scalars) for phase 1; a
//! grapheme-aware revision can come later.

use crate::picker::SymbolCandidate;
use crate::state::Buffer;
use crate::wrap::{self, RowInfo};
use aether_protocol::cursor::{
    Direction, Granularity, Motion, SelectionEdge, VerticalDirection, WordBoundary,
};
use aether_protocol::viewport::WrapMode;
use aether_protocol::LogicalPosition;
use unicode_width::UnicodeWidthChar;

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
    LogicalPosition {
        line: line_idx as u32,
        col: byte_offset as u32,
    }
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

/// Byte offset (within the line) of the last char on the line that isn't the trailing newline.
/// For a non-empty visible line this is the start byte of the last visible char (so the cursor
/// "block" covers that char rather than the newline). For an empty line — only a newline, or
/// the final line with no trailing newline and zero visible content — returns 0.
pub fn line_last_char_byte_idx(buf: &Buffer, line_idx: u32) -> u32 {
    let slice = buf.text.line(line_idx as usize);
    let len_excl_nl = line_byte_len_excl_newline_slice(slice) as usize;
    if len_excl_nl == 0 {
        return 0;
    }
    // Walk back from the byte just past the last visible char to its start boundary. We use
    // the rope's byte_to_char (which falls onto a char even mid-byte for multi-byte UTF-8)
    // plus char_to_byte to land on the char's first byte.
    let line_start_byte = buf.text.line_to_byte(line_idx as usize);
    let last_byte_in_line = line_start_byte + len_excl_nl - 1;
    let last_char_idx = buf.text.byte_to_char(last_byte_in_line);
    let last_char_byte_start = buf.text.char_to_byte(last_char_idx);
    (last_char_byte_start - line_start_byte) as u32
}

/// Byte offset of the first non-blank (not space/tab) char on the line. The trailing newline
/// stops the scan, so an all-blank line yields its line-end position and an empty line yields 0.
fn first_nonblank_col(buf: &Buffer, line_idx: u32) -> u32 {
    let slice = buf.text.line(line_idx as usize);
    let mut byte_offset = 0usize;
    for c in slice.chars() {
        if c == '\n' || !matches!(c, ' ' | '\t') {
            break;
        }
        byte_offset += c.len_utf8();
    }
    byte_offset as u32
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

/// Resolve [`Motion::SelectionEdge`] — the Insert-entry collapse targets. Unlike the rest
/// of the motions this reads the whole selection, so it gets its own resolver taking both
/// endpoints (`resolve_motion` only sees the cursor position).
pub fn resolve_selection_edge(
    buf: &Buffer,
    position: LogicalPosition,
    anchor: LogicalPosition,
    edge: SelectionEdge,
) -> LogicalPosition {
    let (start, end) = ordered(clamp_position(buf, position), clamp_position(buf, anchor));
    match edge {
        SelectionEdge::Start => start,
        SelectionEdge::AfterEnd => {
            // One char past the selection's last char — the same char arithmetic as
            // `Motion::Char { Forward, 1 }`, so multi-byte chars and end-of-line behave
            // identically to the old set-then-step client chain.
            let c = pos_to_char(buf, end)
                .saturating_add(1)
                .min(buf.text.len_chars());
            char_to_pos(buf, c)
        }
        SelectionEdge::FirstLineNonblank => LogicalPosition {
            line: start.line,
            col: first_nonblank_col(buf, start.line),
        },
        SelectionEdge::LastLineEnd => LogicalPosition {
            line: end.line,
            col: line_byte_len_excl_newline(buf, end.line),
        },
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
        Motion::LogicalLine {
            direction,
            count,
            preserve_col,
        } => {
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
            LogicalPosition {
                line: new_line,
                col: new_col,
            }
        }
        Motion::LineStart => LogicalPosition {
            line: current.line,
            col: 0,
        },
        Motion::LineEnd => LogicalPosition {
            line: current.line,
            col: line_last_char_byte_idx(buf, current.line),
        },
        Motion::LineFirstNonblank => LogicalPosition {
            line: current.line,
            col: first_nonblank_col(buf, current.line),
        },
        Motion::LogicalLineFirstNonblank { direction, count } => {
            let line_count = buf.text.len_lines() as u32;
            let new_line = match direction {
                Direction::Forward => current.line.saturating_add(*count),
                Direction::Backward => current.line.saturating_sub(*count),
            };
            let new_line = new_line.min(line_count.saturating_sub(1));
            LogicalPosition {
                line: new_line,
                col: first_nonblank_col(buf, new_line),
            }
        }
        Motion::BufferStart => LogicalPosition { line: 0, col: 0 },
        Motion::BufferEnd => char_to_pos(buf, buf.text.len_chars()),
        Motion::Goto { position } => clamp_position(buf, *position),
        Motion::Word {
            direction,
            count,
            boundary,
        } => {
            let start = pos_to_char(buf, current);
            let end = match direction {
                Direction::Forward => word_forward_start(&buf.text, start, *boundary, *count),
                Direction::Backward => word_backward_start(&buf.text, start, *boundary, *count),
            };
            char_to_pos(buf, end)
        }
        Motion::WordEnd {
            direction,
            count,
            boundary,
        } => {
            let start = pos_to_char(buf, current);
            let end = match direction {
                Direction::Forward => word_forward_end(&buf.text, start, *boundary, *count),
                Direction::Backward => word_backward_end(&buf.text, start, *boundary, *count),
            };
            char_to_pos(buf, end)
        }
        // Visual motions are resolved separately by the cursor/move handler (they need viewport
        // state for wrap mode + width), as are selection-edge motions (they need the anchor).
        // resolve_motion is for buffer-only, cursor-position-only motions.
        Motion::VisualLine { .. }
        | Motion::VisualLineStart { .. }
        | Motion::VisualLineEnd { .. }
        | Motion::SelectionEdge { .. } => current,
        Motion::MatchBracket { inner } => {
            let Some(syntax) = buf.syntax.as_ref() else {
                return current;
            };
            let cursor_byte = buf.text.char_to_byte(pos_to_char(buf, current));
            let Some((open, close)) =
                crate::brackets::find_match_bracket(&syntax.tree, cursor_byte)
            else {
                return current;
            };
            // Outer: jump to whichever bracket isn't under the cursor; default to the opener
            // when the cursor sits between them (Vim's `%`).
            //
            // Inner: jump *one char inside* the matching bracket so the brackets themselves
            // can be excluded from any extend-selection that follows. Toggle when the cursor
            // already sits at one inner side (open+1 or close-1) so a repeat press lands on
            // the opposite side — that's what makes `Alt-m Shift-Alt-m` produce the inside
            // selection. For empty pairs (`()`) the inner positions collapse to the brackets
            // themselves, so the motion is a no-op.
            let target_byte = if *inner {
                let inner_open = open + 1;
                let inner_close = close.saturating_sub(1);
                if inner_open >= close {
                    return current;
                }
                if cursor_byte == open || cursor_byte == inner_open {
                    inner_close
                } else {
                    inner_open
                }
            } else if cursor_byte == open {
                close
            } else {
                // On the closer *or* between the pair — both land on the opener.
                open
            };
            char_to_pos(buf, buf.text.byte_to_char(target_byte))
        }
        // Navigation-unit motions (`o`) are resolved by `resolve_navigation_motion` against the
        // LSP document-symbol outline, never here — `resolve_motion` only sees them if the handler
        // routing changes, so keep them a no-op rather than reintroducing a tree-sitter walk.
        Motion::NextNavigationUnit { .. }
        | Motion::PrevNavigationUnit { .. }
        | Motion::EndOfNavigationUnit
        | Motion::StartOfNavigationUnit => current,
        Motion::FindChar {
            ch,
            direction,
            count,
            till,
        } => {
            let cur_idx = pos_to_char(buf, current);
            let total = buf.text.len_chars();
            let target_idx = find_char(&buf.text, cur_idx, total, *ch, *direction, *count);
            match target_idx {
                Some(idx) => {
                    let final_idx = if *till {
                        match direction {
                            Direction::Forward => idx.saturating_sub(1),
                            Direction::Backward => (idx + 1).min(total),
                        }
                    } else {
                        idx
                    };
                    char_to_pos(buf, final_idx)
                }
                None => current,
            }
        }
    }
}

/// Find the `count`-th occurrence of `ch` from `cur_idx` in `direction`. Returns the absolute
/// char index of the match, or `None` if not found within the buffer bounds.
fn find_char(
    text: &ropey::Rope,
    cur_idx: usize,
    total: usize,
    ch: char,
    direction: Direction,
    count: u32,
) -> Option<usize> {
    let count = count.max(1) as usize;
    match direction {
        Direction::Forward => {
            // Start one char past the cursor so `f x` from an existing 'x' lands on the *next*.
            let start = (cur_idx + 1).min(total);
            let iter = text.chars_at(start);
            let mut at = start;
            let mut found = 0usize;
            for c in iter {
                if c == ch {
                    found += 1;
                    if found == count {
                        return Some(at);
                    }
                }
                at += 1;
            }
            None
        }
        Direction::Backward => {
            // Scan backward starting one char before the cursor.
            let mut at = cur_idx;
            let mut found = 0usize;
            while at > 0 {
                at -= 1;
                if text.char(at) == ch {
                    found += 1;
                    if found == count {
                        return Some(at);
                    }
                }
            }
            None
        }
    }
}

/// Resolve a visual line motion: walk up or down by `count` visual rows under the given wrap
/// settings, preserving the cursor's visual column where possible. When `wrap` is `None` this
/// degenerates to a logical line step (each logical line is one visual row).
///
/// `virtual_col_in` is the cursor's remembered intended visual column from prior vertical
/// motions; if `None`, the current visual column is used. The returned `u32` is the target
/// visual column used by this call — the caller should stash it so repeated vertical motions
/// don't drift across rows with different prefix widths (continuation marker + indent).
pub fn resolve_visual_line(
    buf: &Buffer,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
    virtual_col_in: Option<u32>,
    direction: VerticalDirection,
    count: u32,
) -> (LogicalPosition, u32) {
    let wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    } = geom;
    if matches!(wrap, WrapMode::None) || cols == 0 {
        // No-wrap fast path: treat the entire logical line as one row. The virtual column is in
        // display cells (same currency as the wrap path), so multi-byte chars like `—` round-
        // trip correctly when moving across lines that contain them.
        let cur_text = line_text(buf, current.line);
        let cur_row = RowInfo {
            byte_offset: 0,
            text: cur_text,
            continuation_indent: 0,
        };
        let target_display = virtual_col_in
            .unwrap_or_else(|| visual_col_of_byte(&cur_row, current.col as usize, 0, tab_width));
        let line_count = buf.text.len_lines() as u32;
        let new_line = match direction {
            VerticalDirection::Down => current.line.saturating_add(count),
            VerticalDirection::Up => current.line.saturating_sub(count),
        };
        let new_line = new_line.min(line_count.saturating_sub(1));
        let new_text = line_text(buf, new_line);
        let new_row = RowInfo {
            byte_offset: 0,
            text: new_text,
            continuation_indent: 0,
        };
        let new_col = byte_at_visual_col(&new_row, target_display, 0, tab_width) as u32;
        return (
            LogicalPosition {
                line: new_line,
                col: new_col,
            },
            target_display,
        );
    }

    let line_count = buf.text.len_lines() as u32;
    let mut current_line = current.line.min(line_count.saturating_sub(1));
    let mut rows = wrap::compute_rows(&line_text(buf, current_line), cols, marker_width, tab_width);
    let mut row_idx = find_row_for_col(&rows, current.col as usize);
    let target_visual_col = virtual_col_in.unwrap_or_else(|| {
        visual_col_of_byte(
            &rows[row_idx],
            current.col as usize,
            marker_width,
            tab_width,
        )
    });

    let mut remaining = count;
    while remaining > 0 {
        let advanced = match direction {
            VerticalDirection::Down => {
                if row_idx + 1 < rows.len() {
                    row_idx += 1;
                    true
                } else if current_line + 1 < line_count {
                    current_line += 1;
                    rows = wrap::compute_rows(
                        &line_text(buf, current_line),
                        cols,
                        marker_width,
                        tab_width,
                    );
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
                    rows = wrap::compute_rows(
                        &line_text(buf, current_line),
                        cols,
                        marker_width,
                        tab_width,
                    );
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
    let new_col_within_text = byte_at_visual_col(row, target_visual_col, marker_width, tab_width);
    let new_pos = LogicalPosition {
        line: current_line,
        col: row.byte_offset as u32 + new_col_within_text as u32,
    };
    (new_pos, target_visual_col)
}

/// Resolve VisualLineStart: cursor to the first byte of its current visual row.
pub fn resolve_visual_line_start(
    buf: &Buffer,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, geom, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    LogicalPosition {
        line: current.line,
        col: rows[row_idx].byte_offset as u32,
    }
}

/// Resolve VisualLineEnd: cursor to the last byte of its current visual row.
pub fn resolve_visual_line_end(
    buf: &Buffer,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, geom, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    let row = &rows[row_idx];
    let end_byte = row.byte_offset + row.text.len();
    LogicalPosition {
        line: current.line,
        col: end_byte as u32,
    }
}

fn wrap_rows_for_cursor(
    buf: &Buffer,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
) -> Vec<RowInfo> {
    let wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    } = geom;
    let line_count = buf.text.len_lines() as u32;
    let line_idx = current.line.min(line_count.saturating_sub(1));
    if matches!(wrap, WrapMode::None) || cols == 0 {
        let text = line_text(buf, line_idx);
        let len = text.len();
        vec![RowInfo {
            byte_offset: 0,
            text,
            continuation_indent: 0,
        }]
        .into_iter()
        .map(|mut r| {
            r.text.truncate(len);
            r
        })
        .collect()
    } else {
        wrap::compute_rows(&line_text(buf, line_idx), cols, marker_width, tab_width)
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

/// Visual column of a byte position within a row, in *display cells* (so multi-byte chars like
/// `—` and `→` count as one cell each, and CJK chars as two). Includes the continuation marker
/// (rendered by the client on rows where `byte_offset > 0`) and the indent. Bytes beyond the
/// row's visible text clamp to the end of the visible text.
fn visual_col_of_byte(row: &RowInfo, col_in_line: usize, marker_width: u32, tab_width: u32) -> u32 {
    let relative_byte = col_in_line
        .saturating_sub(row.byte_offset)
        .min(row.text.len());
    let mut display_col: u32 = 0;
    let mut byte_cursor: usize = 0;
    for c in row.text.chars() {
        if byte_cursor >= relative_byte {
            break;
        }
        display_col += step_width(c, display_col, tab_width);
        byte_cursor += c.len_utf8();
    }
    row_prefix_width(row, marker_width) + display_col
}

/// Inverse of `visual_col_of_byte`: byte offset *within the row's text* whose start sits at (or
/// just before) the requested visual column. A target column landing in the middle of a wide
/// char rounds down to that char's start. Visual columns inside the marker/indent prefix clamp
/// to 0.
fn byte_at_visual_col(row: &RowInfo, visual_col: u32, marker_width: u32, tab_width: u32) -> usize {
    let prefix = row_prefix_width(row, marker_width);
    if visual_col <= prefix {
        return 0;
    }
    let target = visual_col - prefix;
    let mut display_col: u32 = 0;
    let mut byte: usize = 0;
    for c in row.text.chars() {
        let w = step_width(c, display_col, tab_width);
        if display_col + w > target {
            break;
        }
        display_col += w;
        byte += c.len_utf8();
    }
    byte
}

/// Width a single char contributes at `current_col`. Tabs use tab-stop math; everything else
/// falls back to `UnicodeWidthChar`. Mirrors `wrap::char_display_width` — kept private here so
/// the cursor module stays self-contained.
fn step_width(c: char, current_col: u32, tab_width: u32) -> u32 {
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

/// Total visual width the client prepends to a row before its text: continuation marker (only on
/// rows where `byte_offset > 0`) plus the row's continuation indent.
fn row_prefix_width(row: &RowInfo, marker_width: u32) -> u32 {
    let marker = if row.byte_offset > 0 { marker_width } else { 0 };
    marker + row.continuation_indent
}

/// Resolve a `LogicalLine` motion, threading the virtual column (in display cells, matching
/// `resolve_visual_line`) so that vertical hops over short or empty lines remember the cursor's
/// original column. Multi-byte chars and double-wide chars are honoured.
pub fn resolve_logical_line(
    buf: &Buffer,
    current: LogicalPosition,
    virtual_col_in: Option<u32>,
    direction: Direction,
    count: u32,
    preserve_col: bool,
    tab_width: u32,
) -> (LogicalPosition, Option<u32>) {
    let line_count = buf.text.len_lines() as u32;
    let new_line = match direction {
        Direction::Forward => current.line.saturating_add(count),
        Direction::Backward => current.line.saturating_sub(count),
    };
    let new_line = new_line.min(line_count.saturating_sub(1));
    if !preserve_col {
        return (
            LogicalPosition {
                line: new_line,
                col: 0,
            },
            None,
        );
    }
    let cur_row = RowInfo {
        byte_offset: 0,
        text: line_text(buf, current.line),
        continuation_indent: 0,
    };
    let target_display = virtual_col_in
        .unwrap_or_else(|| visual_col_of_byte(&cur_row, current.col as usize, 0, tab_width));
    let new_row = RowInfo {
        byte_offset: 0,
        text: line_text(buf, new_line),
        continuation_indent: 0,
    };
    let new_col = byte_at_visual_col(&new_row, target_display, 0, tab_width) as u32;
    (
        LogicalPosition {
            line: new_line,
            col: new_col,
        },
        Some(target_display),
    )
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

fn word_forward_start(
    rope: &ropey::Rope,
    start: usize,
    boundary: WordBoundary,
    count: u32,
) -> usize {
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

fn word_backward_start(
    rope: &ropey::Rope,
    start: usize,
    boundary: WordBoundary,
    count: u32,
) -> usize {
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

fn word_backward_end(
    rope: &ropey::Rope,
    start: usize,
    boundary: WordBoundary,
    count: u32,
) -> usize {
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

/// Inclusive `(start, end)` char indices of the same-category run containing char `i`. Runs
/// follow `boundary`'s categories (word chars / symbols / whitespace), except a newline never
/// joins a run: it's always its own one-char unit. `i >= total` returns `(i, i)`.
fn word_run_bounds(rope: &ropey::Rope, i: usize, boundary: WordBoundary) -> (usize, usize) {
    let total = rope.len_chars();
    if i >= total {
        return (i, i);
    }
    let c = rope.char(i);
    if c == '\n' {
        return (i, i);
    }
    let cat = char_cat(c, boundary);
    let joins = |c: char| c != '\n' && char_cat(c, boundary) == cat;
    let mut start = i;
    while start > 0 && joins(rope.char(start - 1)) {
        start -= 1;
    }
    let mut end = i;
    while end + 1 < total && joins(rope.char(end + 1)) {
        end += 1;
    }
    (start, end)
}

/// Inclusive (start, end) of the same-category char run containing `pos` — the "word" a
/// double-click selects. Runs follow `WordBoundary::Word` categories (word chars / symbols /
/// whitespace), except a newline never joins a run: clicking at end-of-line selects just the
/// line-end position rather than a whitespace run spilling into the next line's indentation.
pub fn word_run(buf: &Buffer, pos: LogicalPosition) -> (LogicalPosition, LogicalPosition) {
    let (start, end) = word_run_bounds(&buf.text, pos_to_char(buf, pos), WordBoundary::Word);
    (char_to_pos(buf, start), char_to_pos(buf, end))
}

/// Resolve the `w` / `Alt-w` "select word" gesture, returning the new `(position, anchor)`.
///
/// "The word" is the run containing the cursor (`word_run_bounds` under `boundary`). The first
/// press *grabs* that word — anchor to its start, cursor to its end — and a repeat press advances
/// to the next word. Whether a press grabs or advances is decided by where the selection already
/// sits:
///
/// - **Hop** (`!extend`): advance only once the selection already covers exactly the current word,
///   forward-oriented (`anchor == start && cursor == end`). A bare point cursor satisfies this
///   only on a *single-char* word (`start == end`), so multi-char words are grabbed first and
///   single-char words are stepped over — there's no way to tell a point resting on a one-char
///   word apart from that word already being selected, so we keep moving to guarantee progress.
/// - **Grow** (`extend`): advance once the cursor sits on its word's last char (`cursor == end`),
///   keeping the anchor put so the selection grows by a word. A point on a single-char word is
///   already on that edge, which is what keeps repeated `Shift-w` presses making progress.
///
/// On advance, a hop moves the anchor to the next word's start; a grow leaves it. When there is no
/// next word the selection stays put (a stable end state rather than a destructive no-op).
pub fn resolve_select_word(
    buf: &Buffer,
    position: LogicalPosition,
    anchor: LogicalPosition,
    boundary: WordBoundary,
    extend: bool,
) -> (LogicalPosition, LogicalPosition) {
    let rope = &buf.text;
    let total = rope.len_chars();
    let cursor = pos_to_char(buf, position);
    let anchor_char = pos_to_char(buf, anchor);
    let (word_start, word_end) = word_run_bounds(rope, cursor, boundary);

    let advance = if extend {
        cursor == word_end
    } else {
        anchor_char == word_start && cursor == word_end
    };

    if !advance {
        // Grab the whole word under the cursor: anchor to its start, cursor to its end.
        (char_to_pos(buf, word_end), char_to_pos(buf, word_start))
    } else {
        let next_start = word_forward_start(rope, cursor, boundary, 1);
        if next_start >= total {
            // No next word: leave the selection on the current word.
            let new_anchor = if extend { anchor_char } else { word_start };
            (char_to_pos(buf, word_end), char_to_pos(buf, new_anchor))
        } else {
            let (_, next_end) = word_run_bounds(rope, next_start, boundary);
            let new_anchor = if extend { anchor_char } else { next_start };
            (char_to_pos(buf, next_end), char_to_pos(buf, new_anchor))
        }
    }
}

/// Expand a `(position, anchor)` pair outward to `granularity` boundaries, preserving which end
/// the cursor occupies. `Word` snaps each endpoint to its containing char run (see [`word_run`]);
/// `Line` produces the whole-line normal form (`col 0` … `line_end`) over the spanned lines. For
/// a point selection the result is forward-oriented. Inputs must already be clamped.
pub fn snap_selection(
    buf: &Buffer,
    position: LogicalPosition,
    anchor: LogicalPosition,
    granularity: Granularity,
) -> (LogicalPosition, LogicalPosition) {
    let backward = (position.line, position.col) < (anchor.line, anchor.col);
    let (lo, hi) = ordered(position, anchor);
    let (lo, hi) = match granularity {
        Granularity::Char => (lo, hi),
        Granularity::Word => (word_run(buf, lo).0, word_run(buf, hi).1),
        Granularity::Line => (
            LogicalPosition {
                line: lo.line,
                col: 0,
            },
            LogicalPosition {
                line: hi.line,
                col: line_byte_len_excl_newline(buf, hi.line),
            },
        ),
    };
    if backward {
        (lo, hi)
    } else {
        (hi, lo)
    }
}

// ---- symbol-driven navigation units (`o`) -------------------------------------------------------
//
// `o`/`Alt-o` step linearly down/up the buffer's LSP document-symbol outline — the same flat list
// (in document order) the `Space o` picker shows — landing on each symbol's name. `Shift-o`/
// `Shift-Alt-o` select to the end/start of the symbol the cursor is in. It's LSP-only: with no
// outline (still loading, or the buffer has no language server) every motion is a no-op, never
// falling back to a different source, so behaviour is identical before and after symbols load.

/// Resolve a navigation-unit motion against the document-symbol outline, returning
/// `(cursor, anchor_override)`. `o`/`Alt-o` (`Next`/`Prev`) land the target symbol's *identifier
/// selected* — cursor on the name's last char, anchor at its start (`Some(start)`). The `Shift-o`
/// edge motions return `None` for the anchor (the handler keeps the existing one, i.e. extends).
/// `symbols` is the buffer's cached outline; empty (still loading / no server) makes it a no-op.
pub fn resolve_navigation_motion(
    buf: &Buffer,
    symbols: &[SymbolCandidate],
    position: LogicalPosition,
    anchor: LogicalPosition,
    motion: &Motion,
    extend: bool,
) -> (LogicalPosition, Option<LogicalPosition>) {
    // A no-op leaves the selection exactly as it was (no-symbols, or no further unit).
    let unchanged = (position, Some(anchor));
    if symbols.is_empty() {
        return unchanged;
    }
    match motion {
        Motion::NextNavigationUnit { count } | Motion::PrevNavigationUnit { count } => {
            let forward = matches!(motion, Motion::NextNavigationUnit { .. });
            let (lo, hi) = ordered(position, anchor);
            // Where the walk starts. Without extend we key off the selection *start* for both
            // directions: after a previous `o` the cursor sits at the symbol's name end, so keying
            // off the cursor would let `Alt-o` re-find the current symbol (whose own start precedes
            // its end). When extending we key off the leading edge in the direction of travel and
            // grow the selection outward. Each step re-keys off the symbol just landed on, so a
            // count walks the outline; if it runs out first we keep the last reachable symbol.
            let mut from = match (extend, forward) {
                (false, _) => lo,
                (true, true) => hi,
                (true, false) => lo,
            };
            let mut landed = None;
            for _ in 0..(*count).max(1) {
                let target = if forward {
                    next_symbol(symbols, from)
                } else {
                    prev_symbol(symbols, from)
                };
                match target {
                    Some(i) => {
                        landed = Some(i);
                        from = symbols[i].start;
                    }
                    None => break,
                }
            }
            match landed {
                // Extend grows the selection to *include* the target identifier: the cursor lands on
                // its far side (name end going forward, name start going back) while the opposite
                // edge of the original selection stays put as the anchor.
                Some(i) if extend => {
                    if forward {
                        (symbols[i].end, Some(lo))
                    } else {
                        (symbols[i].start, Some(hi))
                    }
                }
                // Plain `o`/`Alt-o` select the identifier: anchor at the name start, cursor on its
                // last char.
                Some(i) => (symbols[i].end, Some(symbols[i].start)),
                None => unchanged,
            }
        }
        Motion::StartOfNavigationUnit | Motion::EndOfNavigationUnit => {
            let to_end = matches!(motion, Motion::EndOfNavigationUnit);
            (symbol_edge(buf, symbols, position, to_end), None)
        }
        // Not a navigation motion — kept total; the handler only routes the nav motions here.
        _ => unchanged,
    }
}

fn lc(p: LogicalPosition) -> (u32, u32) {
    (p.line, p.col)
}

/// The next symbol after the cursor in document (picker) order — the one with the smallest name
/// position strictly greater than `pos`. `None` once the cursor is past the last symbol.
fn next_symbol(symbols: &[SymbolCandidate], pos: LogicalPosition) -> Option<usize> {
    symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| lc(s.start) > lc(pos))
        .min_by_key(|(_, s)| lc(s.start))
        .map(|(i, _)| i)
}

/// The previous symbol before the cursor — the one with the largest name position strictly less
/// than `pos`. `None` once the cursor is before the first symbol.
fn prev_symbol(symbols: &[SymbolCandidate], pos: LogicalPosition) -> Option<usize> {
    symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| lc(s.start) < lc(pos))
        .max_by_key(|(_, s)| lc(s.start))
        .map(|(i, _)| i)
}

/// Index of the innermost symbol whose range contains `pos` — deepest depth, then latest start,
/// matching the picker's cursor-highlight rule. Used by [`symbol_edge`].
fn enclosing_symbol(symbols: &[SymbolCandidate], pos: LogicalPosition) -> Option<usize> {
    symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.contains(pos))
        .max_by_key(|(_, s)| (s.depth, s.range_start.line, s.range_start.col))
        .map(|(i, _)| i)
}

/// Resolve `StartOfNavigationUnit` / `EndOfNavigationUnit` against the outline: land on the
/// enclosing symbol's start (or last char) and — when the cursor is already at that boundary —
/// fall through to the next/previous symbol in the list, so repeated `Shift-o` grows the selection
/// symbol by symbol.
fn symbol_edge(
    buf: &Buffer,
    symbols: &[SymbolCandidate],
    pos: LogicalPosition,
    to_end: bool,
) -> LogicalPosition {
    let enclosing = enclosing_symbol(symbols, pos);
    let already_at_boundary = match enclosing {
        Some(i) if to_end => lc(pos) >= lc(symbol_last_char(buf, &symbols[i])),
        Some(i) => lc(pos) <= lc(symbols[i].range_start),
        None => true,
    };
    let target = if already_at_boundary {
        if to_end {
            next_symbol(symbols, pos)
        } else {
            prev_symbol(symbols, pos)
        }
    } else {
        enclosing
    };
    let Some(i) = target else { return pos };
    if to_end {
        symbol_last_char(buf, &symbols[i])
    } else {
        symbols[i].range_start
    }
}

/// A symbol's last char: its `range_end` (an exclusive end position) stepped back one char,
/// clamped so it never precedes the symbol's start.
fn symbol_last_char(buf: &Buffer, sym: &SymbolCandidate) -> LogicalPosition {
    let start = pos_to_char(buf, sym.range_start);
    let end = pos_to_char(buf, sym.range_end);
    char_to_pos(buf, end.saturating_sub(1).max(start))
}

#[cfg(test)]
mod symbol_nav_tests {
    use super::*;

    // Outline (depth-first preorder, source order):
    //   0  struct S   d0  name@0   range 0..2
    //   1  impl S     d0  name@4   range 4..20
    //   2    fn a     d1  name@5   range 5..9
    //   3    fn b     d1  name@10  range 10..14
    //   4    fn c     d1  name@15  range 15..19
    //   5  fn top     d0  name@22  range 22..30
    fn sym(depth: u32, name_line: u32, start_line: u32, end_line: u32) -> SymbolCandidate {
        SymbolCandidate {
            abs_path: String::new(),
            start: LogicalPosition {
                line: name_line,
                col: 0,
            },
            // A 5-char name (cols 0..=4) so the selection span is non-degenerate in tests.
            end: LogicalPosition {
                line: name_line,
                col: 4,
            },
            name: String::new(),
            symbol_kind: aether_protocol::picker::SymbolKind::Function,
            detail: String::new(),
            depth,
            range_start: LogicalPosition {
                line: start_line,
                col: 0,
            },
            range_end: LogicalPosition {
                line: end_line,
                col: 0,
            },
        }
    }

    fn outline() -> Vec<SymbolCandidate> {
        vec![
            sym(0, 0, 0, 2),
            sym(0, 4, 4, 20),
            sym(1, 5, 5, 9),
            sym(1, 10, 10, 14),
            sym(1, 15, 15, 19),
            sym(0, 22, 22, 30),
        ]
    }

    fn at(line: u32) -> LogicalPosition {
        LogicalPosition { line, col: 0 }
    }

    #[test]
    fn steps_down_the_flat_list() {
        let o = outline();
        // Standing on `struct S`'s name → next is `impl S` (idx 1).
        assert_eq!(next_symbol(&o, at(0)), Some(1));
        // On the `impl S` header → the next list row is `fn a` (idx 2), not the next top-level
        // item — it's a plain linear walk, nesting doesn't gate it.
        assert_eq!(next_symbol(&o, at(4)), Some(2));
        // Standing on `fn b`'s name → `fn c` (idx 4).
        assert_eq!(next_symbol(&o, at(10)), Some(4));
        // From inside `fn c` (line 16, the last method) → crosses out of `impl S` to `fn top`
        // (idx 5); there's no scope fence.
        assert_eq!(next_symbol(&o, at(16)), Some(5));
        // Past the last symbol → nothing.
        assert_eq!(next_symbol(&o, at(40)), None);
    }

    #[test]
    fn steps_up_the_flat_list() {
        let o = outline();
        // Standing on `fn c`'s name (line 15) → previous is `fn b` (idx 3).
        assert_eq!(prev_symbol(&o, at(15)), Some(3));
        // On the `impl S` header (line 4) → `struct S` (idx 0).
        assert_eq!(prev_symbol(&o, at(4)), Some(0));
        // Before the first symbol → nothing.
        assert_eq!(prev_symbol(&o, at(0)), None);
        // Past everything → the last symbol `fn top` (idx 5).
        assert_eq!(prev_symbol(&o, at(40)), Some(5));
    }

    #[test]
    fn from_inside_a_body_up_snaps_to_the_enclosing_header() {
        let o = outline();
        // Inside `fn b`'s body (line 11): up snaps to `fn b`'s own name (idx 3, the nearest symbol
        // before the cursor); down steps to the next symbol `fn c` (idx 4).
        assert_eq!(prev_symbol(&o, at(11)), Some(3));
        assert_eq!(next_symbol(&o, at(11)), Some(4));
    }

    #[test]
    fn next_and_prev_select_the_identifier() {
        let o = outline();
        let buf = Buffer::scratch(1, None, 1); // Next/Prev don't touch the buffer
        let next = |pos, anchor| {
            resolve_navigation_motion(
                &buf,
                &o,
                pos,
                anchor,
                &Motion::NextNavigationUnit { count: 1 },
                false,
            )
        };
        let prev = |pos, anchor| {
            resolve_navigation_motion(
                &buf,
                &o,
                pos,
                anchor,
                &Motion::PrevNavigationUnit { count: 1 },
                false,
            )
        };
        // `o` from the top (point cursor) lands the first reachable symbol's identifier
        // *selected*: anchor at its name start, cursor on its last char (`end`).
        assert_eq!(next(at(0), at(0)), (o[1].end, Some(o[1].start))); // idx 1 (impl S)

        // Now standing on that selection (anchor = impl S start, cursor = its name end): `Alt-o`
        // must step to the *previous* symbol, not re-find impl S — the regression this guards.
        // Navigation keys off the selection start, so it lands `struct S` (idx 0).
        assert_eq!(prev(o[1].end, o[1].start), (o[0].end, Some(o[0].start)));
        // And `o` from the same selection advances to the next list row, `fn a` (idx 2).
        assert_eq!(next(o[1].end, o[1].start), (o[2].end, Some(o[2].start)));

        // No further unit → a no-op that *preserves* the current selection (doesn't collapse it).
        assert_eq!(next(o[5].end, o[5].start), (o[5].end, Some(o[5].start)));
        assert_eq!(prev(o[0].end, o[0].start), (o[0].end, Some(o[0].start)));
    }

    #[test]
    fn next_and_prev_honour_count() {
        let o = outline();
        let buf = Buffer::scratch(1, None, 1);
        let nav = |count, forward| {
            let motion = if forward {
                Motion::NextNavigationUnit { count }
            } else {
                Motion::PrevNavigationUnit { count }
            };
            resolve_navigation_motion(&buf, &o, at(0), at(0), &motion, false)
        };
        // From the top, count walks the outline: count 1 → idx 1, count 2 → idx 2, count 3 → idx 3.
        assert_eq!(nav(1, true), (o[1].end, Some(o[1].start)));
        assert_eq!(nav(2, true), (o[2].end, Some(o[2].start)));
        assert_eq!(nav(3, true), (o[3].end, Some(o[3].start)));
        // An over-large count clamps to the last reachable symbol rather than snapping back.
        assert_eq!(nav(99, true), (o[5].end, Some(o[5].start)));
        // count 0 behaves as 1 (the keymap never sends 0, but the resolver must stay total).
        assert_eq!(nav(0, true), (o[1].end, Some(o[1].start)));
        // From the top there's nothing before it, so Prev at any count is a no-op.
        assert_eq!(nav(3, false), (at(0), Some(at(0))));
    }

    #[test]
    fn extend_grows_the_selection_to_include_the_identifier() {
        let o = outline();
        let buf = Buffer::scratch(1, None, 1);
        let ext = |pos, anchor, count, forward| {
            let motion = if forward {
                Motion::NextNavigationUnit { count }
            } else {
                Motion::PrevNavigationUnit { count }
            };
            resolve_navigation_motion(&buf, &o, pos, anchor, &motion, true)
        };

        // `Shift-o` from the top point grows the cursor forward to the first symbol's name end,
        // pinning the anchor at the original edge (it does *not* collapse onto the identifier).
        assert_eq!(ext(at(0), at(0), 1, true), (o[1].end, Some(at(0))));

        // From a selection of `impl S` (idx 1: anchor at name start, cursor at name end), `Shift-o`
        // extends forward to include `fn a` (idx 2): cursor → idx 2's name end, anchor stays at the
        // selection's backward edge (idx 1's start).
        assert_eq!(
            ext(o[1].end, o[1].start, 1, true),
            (o[2].end, Some(o[1].start))
        );

        // `Shift-Alt-o` from that same selection extends *backward* to include `struct S` (idx 0):
        // cursor → idx 0's name *start*, anchor pinned to the selection's forward edge (idx 1's end).
        assert_eq!(
            ext(o[1].end, o[1].start, 1, false),
            (o[0].start, Some(o[1].end))
        );

        // A count grows past several identifiers in one go (forward two from the top → idx 2's end).
        assert_eq!(ext(at(0), at(0), 2, true), (o[2].end, Some(at(0))));

        // Running out of symbols leaves the selection untouched rather than collapsing it.
        assert_eq!(
            ext(o[5].end, o[5].start, 1, true),
            (o[5].end, Some(o[5].start))
        );
    }

    #[test]
    fn enclosing_prefers_the_innermost_symbol() {
        let o = outline();
        // Line 11 is inside both `impl S` (4..20) and `fn b` (10..14) → the deeper one wins.
        assert_eq!(enclosing_symbol(&o, at(11)), Some(3));
        // Line 4 is only inside `impl S`.
        assert_eq!(enclosing_symbol(&o, at(4)), Some(1));
    }
}
