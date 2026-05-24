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
        Motion::LineFirstNonblank => {
            let slice = buf.text.line(current.line as usize);
            let mut byte_offset = 0usize;
            for c in slice.chars() {
                if c == '\n' || !matches!(c, ' ' | '\t') {
                    break;
                }
                byte_offset += c.len_utf8();
            }
            LogicalPosition {
                line: current.line,
                col: byte_offset as u32,
            }
        }
        Motion::BufferStart => LogicalPosition { line: 0, col: 0 },
        Motion::BufferEnd => char_to_pos(buf, buf.text.len_chars()),
        Motion::Goto { position } => clamp_position(buf, *position),
        Motion::Word {
            direction,
            count,
            boundary,
            exclusive,
        } => {
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
                Direction::Backward => {
                    word_backward_start(&buf.text, orig_start, *boundary, *count)
                }
            };
            // Exclusive forward stops one char before the destination word boundary, provided
            // the motion actually advanced.
            if *exclusive && matches!(direction, Direction::Forward) && end > orig_start {
                end -= 1;
            }
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
        // state for wrap mode + width). resolve_motion is for buffer-only motions.
        Motion::VisualLine { .. }
        | Motion::VisualLineStart { .. }
        | Motion::VisualLineEnd { .. } => current,
        Motion::MatchBracket => {
            let Some(syntax) = buf.syntax.as_ref() else {
                return current;
            };
            let source: String = buf.text.chunks().collect();
            let cursor_byte = buf.text.char_to_byte(pos_to_char(buf, current));
            let Some((open, close)) =
                crate::brackets::find_match_bracket(&syntax.tree, cursor_byte)
            else {
                return current;
            };
            // Jump to whichever bracket isn't under the cursor. If the cursor isn't on either
            // (i.e. inside the pair), default to the opener — Vim's `%` does the same.
            let target_byte = if cursor_byte == open {
                close
            } else if cursor_byte == close {
                open
            } else {
                open
            };
            let _ = source; // (kept for future predicate work)
            char_to_pos(buf, buf.text.byte_to_char(target_byte))
        }
        Motion::NextNavigationUnit | Motion::PrevNavigationUnit => {
            let forward = matches!(motion, Motion::NextNavigationUnit);
            let Some(syntax) = buf.syntax.as_ref() else {
                return current;
            };
            let cursor_byte = buf.text.char_to_byte(pos_to_char(buf, current));
            let nav_kinds = syntax.config.navigation_kinds;
            if nav_kinds.is_empty() {
                return current;
            }
            match find_navigation_target(&syntax.tree, cursor_byte, nav_kinds, forward) {
                Some(target) => char_to_pos(buf, buf.text.byte_to_char(target.start_byte())),
                None => current,
            }
        }
        Motion::EndOfNavigationUnit | Motion::StartOfNavigationUnit => {
            let to_end = matches!(motion, Motion::EndOfNavigationUnit);
            let Some(syntax) = buf.syntax.as_ref() else {
                return current;
            };
            let cursor_byte = buf.text.char_to_byte(pos_to_char(buf, current));
            let nav_kinds = syntax.config.navigation_kinds;
            if nav_kinds.is_empty() {
                return current;
            }
            // First press from inside a unit jumps to that unit's boundary. A repeat press,
            // where the cursor already sits at the boundary (or where the cursor isn't inside
            // any unit), falls through to the next/prev sibling — so `}}}` walks through
            // adjacent units, growing the selection one unit at a time.
            let enclosing = enclosing_navigation_unit(&syntax.tree, cursor_byte, nav_kinds);
            let already_at_boundary = match enclosing {
                Some(u) if to_end => cursor_byte >= u.end_byte().saturating_sub(1),
                Some(u) => cursor_byte <= u.start_byte(),
                None => true,
            };
            let target = if already_at_boundary {
                find_navigation_target(&syntax.tree, cursor_byte, nav_kinds, to_end)
            } else {
                enclosing
            };
            let Some(target) = target else { return current };
            let target_byte = if to_end {
                // Tree-sitter end byte is exclusive — back up one to land on the unit's last
                // *char*, so an inclusive selection ends exactly at the unit's boundary.
                target.end_byte().saturating_sub(1).max(target.start_byte())
            } else {
                target.start_byte()
            };
            char_to_pos(buf, buf.text.byte_to_char(target_byte))
        }
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

/// Walk up the tree from the cursor looking for the closest navigation-kind node past (or
/// before) `cursor_byte`, at the cursor's *own* depth — the motion never crosses scope
/// boundaries. The walk-up has two steps:
///
/// 1. **Skip nav-kind ancestors anchored at the cursor** — i.e. if the cursor sits at the
///    exact start of a nav-kind, that nav-kind is the cursor's *self*, not its container; we
///    keep walking so navigation happens among its siblings.
/// 2. **Stop at the first ancestor with any nav-kind children.** That ancestor *is* the
///    cursor's level; pick the next/prev qualifying child or no-op. We never walk past this
///    level even when there's no hit, so a cursor on the last method of a class can't fall
///    out of the class into top-level items.
/// Smallest navigation-kind ancestor of the cursor — the unit the cursor is "inside" or "on".
/// Walks up from the cursor's deepest descendant, returning the first ancestor whose kind is
/// in `nav_kinds`. `None` when the cursor isn't inside any navigation unit (e.g. on a blank
/// line between top-level items).
fn enclosing_navigation_unit<'tree>(
    tree: &'tree tree_sitter::Tree,
    cursor_byte: usize,
    nav_kinds: &[&str],
) -> Option<tree_sitter::Node<'tree>> {
    let is_nav = |kind: &str| nav_kinds.contains(&kind);
    let root = tree.root_node();
    let mut node = root
        .descendant_for_byte_range(cursor_byte, cursor_byte)
        .unwrap_or(root);
    loop {
        if is_nav(node.kind()) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn find_navigation_target<'tree>(
    tree: &'tree tree_sitter::Tree,
    cursor_byte: usize,
    nav_kinds: &[&str],
    forward: bool,
) -> Option<tree_sitter::Node<'tree>> {
    let is_nav = |kind: &str| nav_kinds.contains(&kind);
    let root = tree.root_node();
    let mut node = root
        .descendant_for_byte_range(cursor_byte, cursor_byte)
        .unwrap_or(root);

    loop {
        let on_self = is_nav(node.kind()) && node.start_byte() == cursor_byte;
        if !on_self {
            // Tree-sitter children iterate in source order, so for forward search the first
            // qualifying child is the answer; for backward search the *last* qualifying child
            // wins, which we get by overwriting `best` on every hit.
            let mut walker = node.walk();
            let mut best: Option<tree_sitter::Node<'tree>> = None;
            let mut any_nav_child = false;
            for child in node.children(&mut walker) {
                if !is_nav(child.kind()) {
                    continue;
                }
                any_nav_child = true;
                if forward {
                    if child.start_byte() > cursor_byte {
                        return Some(child);
                    }
                } else if child.start_byte() < cursor_byte {
                    best = Some(child);
                }
            }
            if any_nav_child {
                return best;
            }
        }
        node = node.parent()?;
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
            let mut iter = text.chars_at(start);
            let mut at = start;
            let mut found = 0usize;
            while let Some(c) = iter.next() {
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
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
    current: LogicalPosition,
    virtual_col_in: Option<u32>,
    direction: VerticalDirection,
    count: u32,
) -> (LogicalPosition, u32) {
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
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, wrap, cols, marker_width, tab_width, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    LogicalPosition {
        line: current.line,
        col: rows[row_idx].byte_offset as u32,
    }
}

/// Resolve VisualLineEnd: cursor to the last byte of its current visual row.
pub fn resolve_visual_line_end(
    buf: &Buffer,
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(buf, wrap, cols, marker_width, tab_width, current);
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
    wrap: WrapMode,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
    current: LogicalPosition,
) -> Vec<RowInfo> {
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
