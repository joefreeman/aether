//! Cursor motion resolution and position arithmetic.
//!
//! Positions are `(line, col_bytes)`. ropey indexes by char internally, so we round-trip through
//! char offsets for arithmetic. `count` in motions is in chars (Unicode scalars) for phase 1; a
//! grapheme-aware revision can come later.

use crate::picker::SymbolCandidate;
use crate::state::Document;
use crate::wrap::{self, RowInfo};
use aether_protocol::cursor::{
    Direction, Granularity, Motion, SelectionEdge, VerticalDirection, WordBoundary,
};
use aether_protocol::viewport::WrapMode;
use aether_protocol::LogicalPosition;
use unicode_width::UnicodeWidthChar;

/// Convert a (line, byte-col) position to an absolute char index in the rope. Clamped to valid
/// positions in the buffer.
pub fn pos_to_char(buf: &Document, pos: LogicalPosition) -> usize {
    let line_count = buf.text.len_lines().max(1);
    let line_idx = (pos.line as usize).min(line_count - 1);
    let line_start_char = buf.text.line_to_char(line_idx);
    let line_slice = buf.text.line(line_idx);
    let byte_offset = (pos.col as usize).min(line_byte_len_excl_newline_slice(line_slice) as usize);
    let char_offset_in_line = line_slice.byte_to_char(byte_offset);
    line_start_char + char_offset_in_line
}

/// Convert an absolute char index back to a (line, byte-col) position.
pub fn char_to_pos(buf: &Document, char_idx: usize) -> LogicalPosition {
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

pub fn line_byte_len_excl_newline(buf: &Document, line_idx: u32) -> u32 {
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
pub fn line_last_char_byte_idx(buf: &Document, line_idx: u32) -> u32 {
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
fn first_nonblank_col(buf: &Document, line_idx: u32) -> u32 {
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

pub fn clamp_position(buf: &Document, pos: LogicalPosition) -> LogicalPosition {
    let line_count = buf.text.len_lines() as u32;
    let line = pos.line.min(line_count.saturating_sub(1));
    let col = pos.col.min(line_byte_len_excl_newline(buf, line));
    LogicalPosition { line, col }
}

/// The text a motion may move within: a document, bounded to the window the focused element shows
/// of it.
///
/// **Why every resolver takes this rather than a `Document`.** An element windows a *slice* of its
/// buffer — one hunk of a file, in a patch — so a motion that leaves the slice leaves the view: the
/// cursor lands on lines nothing rendered, and every later key acts somewhere the user cannot see.
///
/// That rule used to be a clamp applied to a motion's *result*, by hand, in the two handlers that
/// remembered to: `w`, `x` and Alt-Backspace never asked at all (the last one deleting text above
/// the element, in the real file), and `f` was worse than unbounded — it scanned the whole document,
/// found its char three hundred lines below the hunk, and the clamp pinned the line to the hunk's
/// end while keeping the column it had found down there.
///
/// Bounding the text a motion can *see* answers both: a scan that finds nothing inside the element
/// simply doesn't move, and no handler can forget to bound one, because the resolvers accept nothing
/// else. A whole-buffer element — which is every view but a patch — spans the document, so ordinary
/// editing resolves exactly as it always did.
///
/// Crossing elements stays explicit: `Tab`/`Shift-Tab`, `c`/`Alt-c`, a click. Those don't resolve
/// motions at all — they name an element and return [`aether_protocol::viewport::
/// ViewportFocusElementResult`], which carries the buffer the client must rebind to.
pub struct Scope<'a> {
    doc: &'a Document,
    /// The element's extent in the document's lines, already clamped to what it currently has.
    start_line: u32,
    end_line_exclusive: u32,
}

impl<'a> Scope<'a> {
    /// The whole document — a view of one whole-buffer element, and the scope every non-view path
    /// (tests, an edit resolving against a document it was handed) works in.
    pub fn whole(doc: &'a Document) -> Self {
        let lines = (doc.text.len_lines() as u32).max(1);
        Self {
            doc,
            start_line: 0,
            end_line_exclusive: lines,
        }
    }

    /// A window onto `doc`. The extent is a *claim* about the buffer — a hunk's line range, taken
    /// when the diff ran — so it is clamped to the lines the document actually has now, and never
    /// to nothing: a scope always holds at least the line it starts on.
    pub fn windowed(doc: &'a Document, start_line: u32, end_line_exclusive: u32) -> Self {
        let lines = (doc.text.len_lines() as u32).max(1);
        let start_line = start_line.min(lines - 1);
        let end_line_exclusive = end_line_exclusive.clamp(start_line + 1, lines);
        Self {
            doc,
            start_line,
            end_line_exclusive,
        }
    }

    /// The document itself — for the arithmetic that converts between coordinate systems (char
    /// offsets, line lengths) and for the structures that span the whole of it: the syntax tree, the
    /// LSP outline. Not a way *out*: what those structures name still has to be inside the scope to
    /// be moved to, and the resolvers pass every answer through [`Scope::clamp`].
    pub fn doc(&self) -> &'a Document {
        self.doc
    }

    /// First line of the window.
    pub fn first_line(&self) -> u32 {
        self.start_line
    }

    /// Last line of the window (inclusive) — always a real line.
    pub fn last_line(&self) -> u32 {
        self.end_line_exclusive - 1
    }

    /// The window's lines, as the half-open range the layout speaks in.
    pub fn lines(&self) -> std::ops::Range<u32> {
        self.start_line..self.end_line_exclusive
    }

    /// The scope's text — the only rope a scan may walk, so a scan physically cannot find a match
    /// outside the element. Char indices into it are scope-local; [`Scope::char_of`] and
    /// [`Scope::pos_of`] convert.
    pub fn text(&self) -> ropey::RopeSlice<'a> {
        let (lo, hi) = self.char_bounds();
        self.doc.text.slice(lo..hi)
    }

    /// The scope-local char index of `pos`.
    pub fn char_of(&self, pos: LogicalPosition) -> usize {
        let (lo, hi) = self.char_bounds();
        pos_to_char(self.doc, self.clamp(pos)).clamp(lo, hi) - lo
    }

    /// The position at scope-local char index `local`, clamped into the window. An index at the very
    /// end of the scope lands on the end of its last line rather than the first char of the line
    /// after it — that line belongs to the next element.
    pub fn pos_of(&self, local: usize) -> LogicalPosition {
        let (lo, _) = self.char_bounds();
        self.clamp(char_to_pos(self.doc, lo + local))
    }

    /// Pull a position into the window.
    ///
    /// A position *outside* the window goes to the edge it overshot — the end of the last line, or
    /// the start of the first — and keeps nothing of where it came from. Carrying the column across
    /// is what put the cursor in a place nothing had put it: `f` scanned the file, found its char
    /// three hundred lines down, and the old clamp moved the line back while keeping column 61.
    /// Inside the window only the column is clamped, so `j` down a ragged edge behaves as ever.
    pub fn clamp(&self, pos: LogicalPosition) -> LogicalPosition {
        if pos.line > self.last_line() {
            let line = self.last_line();
            return LogicalPosition {
                line,
                col: line_byte_len_excl_newline(self.doc, line),
            };
        }
        if pos.line < self.start_line {
            return LogicalPosition {
                line: self.start_line,
                col: 0,
            };
        }
        LogicalPosition {
            line: pos.line,
            col: pos.col.min(line_byte_len_excl_newline(self.doc, pos.line)),
        }
    }

    /// Whether `pos` is a position of this window (by line — a column past the line's end is a
    /// clamp, not a different element).
    pub fn contains(&self, pos: LogicalPosition) -> bool {
        self.contains_line(pos.line)
    }

    /// [`Scope::contains`] for a bare line number, for the motions that compute a target line
    /// before they have a column for it.
    pub fn contains_line(&self, line: u32) -> bool {
        self.lines().contains(&line)
    }

    /// Byte offset of the window's first char — the base a scan over [`Scope::text`] adds back to
    /// turn its own offsets into the document's.
    pub fn first_byte(&self) -> usize {
        let lines = self.doc.text.len_lines();
        self.doc
            .text
            .line_to_byte((self.start_line as usize).min(lines))
    }

    /// Absolute byte range of the window — the bounds a *structural* edit must fall inside.
    ///
    /// Motions work in lines and chars; the markdown block edits resolve to a byte range over the
    /// whole document, so they need the window in the same units to be checked against it.
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        let start = self.first_byte();
        start..start + self.text().len_bytes()
    }

    /// Absolute char indices bounding the window: its first char, and one past its last. The upper
    /// bound is the start of the line *after* the window, so the last line's own newline is inside
    /// it — a scan crossing it stops at the window's end rather than at its last line's.
    fn char_bounds(&self) -> (usize, usize) {
        let lines = self.doc.text.len_lines();
        (
            self.doc
                .text
                .line_to_char((self.start_line as usize).min(lines)),
            self.doc
                .text
                .line_to_char((self.end_line_exclusive as usize).min(lines)),
        )
    }
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
    scope: &Scope,
    position: LogicalPosition,
    anchor: LogicalPosition,
    edge: SelectionEdge,
) -> LogicalPosition {
    let buf = scope.doc();
    let (start, end) = ordered(scope.clamp(position), scope.clamp(anchor));
    let to = match edge {
        SelectionEdge::Start => start,
        SelectionEdge::AfterEnd => {
            // One char past the selection's last char — the same char arithmetic as
            // `Motion::Char { Forward, 1 }`, so multi-byte chars and end-of-line behave
            // identically to the old set-then-step client chain.
            scope.pos_of(scope.char_of(end).saturating_add(1))
        }
        SelectionEdge::FirstLineNonblank => LogicalPosition {
            line: start.line,
            col: first_nonblank_col(buf, start.line),
        },
        SelectionEdge::LastLineEnd => LogicalPosition {
            line: end.line,
            col: line_byte_len_excl_newline(buf, end.line),
        },
    };
    scope.clamp(to)
}

/// A counted motion split into its single step and how many of them, for the motions where "N
/// steps" and "count N" are the same thing. `None` for everything else, which then resolves once.
///
/// The list is deliberately short: it holds the motions that need *this* helper to obey the
/// all-or-nothing rule, not every motion the rule applies to. It excludes:
///
/// - **`VisualLine`**, which refuses inside its own resolver — it needs the viewport's wrap
///   geometry, so like `LogicalLine` it resolves elsewhere and applies the rule there. Its former
///   exemption ("must keep clamping") was an artefact of `v`/`Alt-v` borrowing the variant to carry
///   a synthesised row span; `Motion::Page` carries that now, and clamps because a page span is
///   not a count anyone asserted.
/// - **Absolute targets** (`Goto`, `BufferStart`/`End`, `LineStart`/`End`, `MatchBracket`,
///   `SelectionEdge`), which name a destination rather than a repetition — they refuse by not
///   finding it, which they already do.
/// - **`FindChar`**, which obeys the rule through its own counted walk: `find_char` returns `None`
///   unless the count-th match exists, so `3f;` with two semicolons left does not move.
/// - **`LogicalLine`** and **`LogicalLineFirstNonblank`**, which route through `counted_line` —
///   they need the virtual column and tab width, so they resolve elsewhere and refuse there.
/// - **Navigation units**, which are scope-filtered but do **not** yet honour the count rule: their
///   walk keeps the last reachable symbol instead of refusing. Scope and count are separate rules
///   and this exclusion only answers the first.
pub fn single_step(motion: &Motion) -> Option<(Motion, u32)> {
    match motion {
        Motion::Char { direction, count } => Some((
            Motion::Char {
                direction: *direction,
                count: 1,
            },
            *count,
        )),
        Motion::Word {
            direction,
            boundary,
            count,
        } => Some((
            Motion::Word {
                direction: *direction,
                boundary: *boundary,
                count: 1,
            },
            *count,
        )),
        Motion::WordEnd {
            direction,
            boundary,
            count,
        } => Some((
            Motion::WordEnd {
                direction: *direction,
                boundary: *boundary,
                count: 1,
            },
            *count,
        )),
        _ => None,
    }
}

/// **The count rule, in one place: a counted operation is `count` steps, or none.**
///
/// Repeats `step` from `start`. A step that leaves the state unchanged is a *stall* — there was
/// nowhere further to go — and a stall anywhere abandons the whole operation, returning `None` so
/// the caller leaves the cursor exactly where it was.
///
/// This exists because the per-arm version does not converge. Every counted motion and every
/// server-side repeat loop had its own way of running out — `.min(len)`, `.clamp(first, last)`,
/// `saturating_sub`, or simply a loop that stopped early — and fixing them one at a time is how
/// `100j` was fixed while `100l`, `100w` and `100x` went on clamping. There is one rule; it should
/// have one implementation.
///
/// **At `count == 1` this is exactly the old behaviour** for anything whose single step already
/// clamps in place: one stalled step returns `None`, the caller keeps `start`, and `start` is where
/// clamping would have left it. So adopting it cannot change a bare `l`, `w` or `x`.
///
/// Deliberately *not* used for edits (`3J`, `3>`). A motion that stalls has changed nothing and can
/// simply be dropped; an edit that stalls on its third of five steps has already mutated the
/// document, so all-or-nothing there means transactional rollback, which is a different problem.
pub fn all_or_nothing<T: PartialEq + Copy>(
    count: u32,
    start: T,
    mut step: impl FnMut(T) -> T,
) -> Option<T> {
    let mut current = start;
    for _ in 0..count.max(1) {
        let next = step(current);
        if next == current {
            return None;
        }
        current = next;
    }
    Some(current)
}

/// The line a vertical step lands on, or `None` when the motion is refused and the cursor must not
/// move.
///
/// **A count is all-or-nothing; a bare step still clamps.** `100j` five lines from the field's end
/// used to land on the last line; now it does nothing, because a count says "this many" and moving
/// a different number while reporting success is what makes `100j` then `100k` lose your place. An
/// *uncounted* step past the edge keeps clamping — that is the ordinary "already at the end" no-op.
///
/// The split is invisible for `j`/`k`, where clamping to the line you are already on is the same
/// outcome as refusing. It is **not** invisible for every caller, which is why the rule is written
/// as the rule rather than as the coincidence: `Motion::LogicalLineFirstNonblank` also normalises
/// the *column*, so at the first line a bare `Alt-p` clamps in place and still snaps to the first
/// non-blank — behaviour a blanket refusal silently removed.
///
/// Reached by [`Motion::VisualLine`] too, but only with soft wrap off — there a visual row *is* a
/// logical line, so the rule has one implementation for both. The wrapped case walks rows and
/// applies the same policy at the end of its walk; see [`Overshoot`].
fn counted_line(scope: &Scope, from: u32, direction: Direction, count: u32) -> Option<u32> {
    let checked = match direction {
        Direction::Forward => from.checked_add(count),
        Direction::Backward => from.checked_sub(count),
    };
    match checked {
        Some(line) if scope.contains_line(line) => Some(line),
        _ if count <= 1 => {
            let saturated = match direction {
                Direction::Forward => from.saturating_add(count),
                Direction::Backward => from.saturating_sub(count),
            };
            Some(saturated.clamp(scope.first_line(), scope.last_line()))
        }
        _ => None,
    }
}

/// Resolve a motion within `scope` — the element's window onto its buffer, and every character the
/// motion is allowed to see. See [`Scope`] for why that is the argument rather than a document.
pub fn resolve_motion(scope: &Scope, current: LogicalPosition, motion: &Motion) -> LogicalPosition {
    let buf = scope.doc();
    let to = match motion {
        Motion::Char { direction, count } => {
            let cur_char = scope.char_of(current);
            let new_char = match direction {
                Direction::Forward => cur_char
                    .saturating_add(*count as usize)
                    .min(scope.text().len_chars()),
                Direction::Backward => cur_char.saturating_sub(*count as usize),
            };
            scope.pos_of(new_char)
        }
        Motion::LogicalLine {
            direction,
            count,
            preserve_col,
        } => {
            let Some(new_line) = counted_line(scope, current.line, *direction, *count) else {
                return current;
            };
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
            let Some(new_line) = counted_line(scope, current.line, *direction, *count) else {
                return current;
            };
            LogicalPosition {
                line: new_line,
                col: first_nonblank_col(buf, new_line),
            }
        }
        // "The buffer" is the element's window onto it: in a patch, `gg`/`G` are the top and bottom
        // of the hunk you are in — the file's other three thousand lines aren't in the view, and
        // landing on one of them is the bug this whole type exists to prevent.
        Motion::BufferStart => LogicalPosition {
            line: scope.first_line(),
            col: 0,
        },
        Motion::BufferEnd => scope.pos_of(scope.text().len_chars()),
        // `N g` names one line, so a line outside the field is not a destination — refuse, the way
        // `MatchBracket` below does, rather than clamping onto the field's edge and reporting an
        // arrival. The gutter prints buffer line numbers, so the number the user typed and the
        // number they can see are the same one; when it isn't on screen, nothing happens.
        //
        // Only ever reached *with* a count: the uncounted `g`/`Alt-g` resolve as `BufferStart` /
        // `BufferEnd` above, which are the field's own edges. That split is what lets this refuse
        // without breaking a bare `g` in a composed view, where the field rarely starts at line 0.
        Motion::Goto { position } => {
            if !scope.contains(*position) {
                return current;
            }
            scope.clamp(*position)
        }
        // `N Alt-g`: the N-th line from the **field's** end. Refuses like `Goto` when the field has
        // fewer lines than that — a line above the field is not a destination either. The client
        // used to count back from the *view's* line count, a number of the wrong space in any
        // composed view.
        Motion::LineFromEnd { count } => {
            let Some(line) = scope
                .last_line()
                .checked_sub(count.saturating_sub(1))
                .filter(|line| scope.contains_line(*line))
            else {
                return current;
            };
            LogicalPosition { line, col: 0 }
        }
        // The word walks read `scope.text()`, so they run out of text at the element's edge instead
        // of stepping into the next hunk's file. Same for `WordEnd` and `FindChar` below.
        Motion::Word {
            direction,
            count,
            boundary,
        } => {
            let start = scope.char_of(current);
            let end = match direction {
                Direction::Forward => word_forward_start(scope.text(), start, *boundary, *count),
                Direction::Backward => word_backward_start(scope.text(), start, *boundary, *count),
            };
            scope.pos_of(end)
        }
        Motion::WordEnd {
            direction,
            count,
            boundary,
        } => {
            let start = scope.char_of(current);
            let end = match direction {
                Direction::Forward => word_forward_end(scope.text(), start, *boundary, *count),
                Direction::Backward => word_backward_end(scope.text(), start, *boundary, *count),
            };
            scope.pos_of(end)
        }
        // Visual motions are resolved separately by the cursor/move handler (they need viewport
        // state for wrap mode + width), as are selection-edge motions (they need the anchor).
        // resolve_motion is for buffer-only, cursor-position-only motions.
        Motion::VisualLine { .. }
        | Motion::Page { .. }
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
            let to = char_to_pos(buf, buf.text.byte_to_char(target_byte));
            // The syntax tree spans the whole file, so the match can sit outside the element — an
            // opener above the hunk, its closer below it. That isn't a near miss to pull to the
            // element's edge: the bracket is not in the view, so there is nowhere to jump.
            if !scope.contains(to) {
                return current;
            }
            to
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
            let cur_idx = scope.char_of(current);
            let total = scope.text().len_chars();
            let target_idx = find_char(scope.text(), cur_idx, total, *ch, *direction, *count);
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
                    scope.pos_of(final_idx)
                }
                // No such char *in the element*. Staying put is the whole point: scanning the file
                // and clamping the answer put the cursor on a column three hundred lines from where
                // the char actually was.
                None => current,
            }
        }
    };
    // Backstop. The scans above can't leave the scope — they only ever saw its text — so this is
    // for the arms that consult something spanning the whole document, and for a `current` that a
    // concurrent edit has left outside the window.
    scope.clamp(to)
}

/// Find the `count`-th occurrence of `ch` from `cur_idx` in `direction`. Returns the scope-local
/// char index of the match, or `None` if there isn't one — the scan sees the focused element's text
/// and nothing else.
fn find_char(
    text: ropey::RopeSlice<'_>,
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
            let mut found = 0usize;
            for (at, c) in (start..).zip(iter) {
                if c == ch {
                    found += 1;
                    if found == count {
                        return Some(at);
                    }
                }
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

/// What a vertical walk does when it runs out of rows before it has run out of steps.
///
/// The two callers of [`resolve_visual_line`] differ here and only here. `Alt-j`/`Alt-k` carry a
/// count the user typed, so they [`Refuse`](Overshoot::Refuse) — the all-or-nothing rule, for the
/// reasons in [`all_or_nothing`]. `v`/`Alt-v` carry a row span derived from the viewport's height,
/// a number nobody asserted, so they [`Clamp`](Overshoot::Clamp): refusing it would make `v` a dead
/// key for the last screenful of every file, with no way to reach the end.
///
/// Which is exactly why the page motion has its own [`Motion::Page`] rather than borrowing
/// `VisualLine`'s `count` — one field cannot mean both, and the server cannot tell the two numbers
/// apart once they are in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overshoot {
    /// Keep the last reachable row.
    Clamp,
    /// Leave the cursor exactly where it was.
    Refuse,
}

/// Resolve a visual line motion: walk up or down by `count` visual rows under the given wrap
/// settings, preserving the cursor's visual column where possible. When `wrap` is `None` this
/// degenerates to a logical line step (each logical line is one visual row).
///
/// `virtual_col_in` is the cursor's remembered intended visual column from prior vertical
/// motions; if `None`, the current visual column is used. The returned `u32` is the target
/// visual column used by this call — the caller should stash it so repeated vertical motions
/// don't drift across rows with different prefix widths (continuation marker + indent).
///
/// `overshoot` decides what happens when the field runs out of rows first — see [`Overshoot`]. A
/// refusal returns `current` untouched, along with the target column so a chain of vertical
/// motions doesn't forget its intended column just because one press had nowhere to go.
pub fn resolve_visual_line(
    scope: &Scope,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
    virtual_col_in: Option<u32>,
    direction: VerticalDirection,
    count: u32,
    overshoot: Overshoot,
) -> (LogicalPosition, u32) {
    let buf = scope.doc();
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
        // Wrap off: one visual row is one logical line, so this is `counted_line`'s case exactly
        // and refusing goes through the same implementation `j`/`k` refuse with.
        let new_line = match overshoot {
            Overshoot::Refuse => {
                let dir = match direction {
                    VerticalDirection::Down => Direction::Forward,
                    VerticalDirection::Up => Direction::Backward,
                };
                match counted_line(scope, current.line, dir, count) {
                    Some(line) => line,
                    None => return (current, target_display),
                }
            }
            Overshoot::Clamp => {
                let stepped = match direction {
                    VerticalDirection::Down => current.line.saturating_add(count),
                    VerticalDirection::Up => current.line.saturating_sub(count),
                };
                stepped.clamp(scope.first_line(), scope.last_line())
            }
        };
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

    let mut current_line = current.line.clamp(scope.first_line(), scope.last_line());
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
                } else if current_line < scope.last_line() {
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
                } else if current_line > scope.first_line() {
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

    // The walk hit the field's edge with steps still owed. A typed count that cannot be honoured
    // refuses outright rather than landing short — the rule in `all_or_nothing`, applied here
    // because a row walk cannot go through `counted_line`. An uncounted step keeps clamping, the
    // same carve-out `counted_line` makes and for the same reason: a bare `Alt-j` at the last row
    // is the ordinary "already at the end" no-op, not a refusal.
    if remaining > 0 && overshoot == Overshoot::Refuse && count > 1 {
        return (current, target_visual_col);
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
    scope: &Scope,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(scope.doc(), geom, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    LogicalPosition {
        line: current.line,
        col: rows[row_idx].byte_offset as u32,
    }
}

/// Resolve VisualLineEnd: cursor to the last byte of its current visual row.
pub fn resolve_visual_line_end(
    scope: &Scope,
    geom: wrap::WrapGeometry,
    current: LogicalPosition,
) -> LogicalPosition {
    let rows = wrap_rows_for_cursor(scope.doc(), geom, current);
    let row_idx = find_row_for_col(&rows, current.col as usize);
    let row = &rows[row_idx];
    let end_byte = row.byte_offset + row.text.len();
    LogicalPosition {
        line: current.line,
        col: end_byte as u32,
    }
}

fn wrap_rows_for_cursor(
    buf: &Document,
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

fn line_text(buf: &Document, line_idx: u32) -> String {
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
    scope: &Scope,
    current: LogicalPosition,
    virtual_col_in: Option<u32>,
    direction: Direction,
    count: u32,
    preserve_col: bool,
    tab_width: u32,
) -> (LogicalPosition, Option<u32>) {
    let buf = scope.doc();
    // The same all-or-nothing rule `resolve_motion` applies — and this is the resolver `j`/`k`
    // actually reach, because the handler intercepts `Motion::LogicalLine` here for the virtual
    // column and tab width. Refusing in only one of the two is how `100j` kept clamping.
    let Some(new_line) = counted_line(scope, current.line, direction, count) else {
        return (current, virtual_col_in);
    };
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
    rope: ropey::RopeSlice<'_>,
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
    rope: ropey::RopeSlice<'_>,
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

fn word_forward_end(
    rope: ropey::RopeSlice<'_>,
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
    rope: ropey::RopeSlice<'_>,
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
fn word_run_bounds(rope: ropey::RopeSlice<'_>, i: usize, boundary: WordBoundary) -> (usize, usize) {
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
pub fn word_run(scope: &Scope, pos: LogicalPosition) -> (LogicalPosition, LogicalPosition) {
    let (start, end) = word_run_bounds(scope.text(), scope.char_of(pos), WordBoundary::Word);
    (scope.pos_of(start), scope.pos_of(end))
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
    scope: &Scope,
    position: LogicalPosition,
    anchor: LogicalPosition,
    boundary: WordBoundary,
    extend: bool,
) -> (LogicalPosition, LogicalPosition) {
    // Scope-local throughout: "no next word" then means none *in the element*, so `w` at its last
    // word keeps that word selected instead of walking into the next hunk's file.
    let rope = scope.text();
    let total = rope.len_chars();
    let cursor = scope.char_of(position);
    let anchor_char = scope.char_of(anchor);
    let (word_start, word_end) = word_run_bounds(rope, cursor, boundary);

    let advance = if extend {
        cursor == word_end
    } else {
        anchor_char == word_start && cursor == word_end
    };

    if !advance {
        // Grab the whole word under the cursor: anchor to its start, cursor to its end.
        (scope.pos_of(word_end), scope.pos_of(word_start))
    } else {
        let next_start = word_forward_start(rope, cursor, boundary, 1);
        if next_start >= total {
            // No next word: leave the selection on the current word.
            let new_anchor = if extend { anchor_char } else { word_start };
            (scope.pos_of(word_end), scope.pos_of(new_anchor))
        } else {
            let (_, next_end) = word_run_bounds(rope, next_start, boundary);
            let new_anchor = if extend { anchor_char } else { next_start };
            (scope.pos_of(next_end), scope.pos_of(new_anchor))
        }
    }
}

/// Expand a `(position, anchor)` pair outward to `granularity` boundaries, preserving which end
/// the cursor occupies. `Word` snaps each endpoint to its containing char run (see [`word_run`]);
/// `Line` produces the whole-line normal form (`col 0` … `line_end`) over the spanned lines. For
/// a point selection the result is forward-oriented. Inputs must already be clamped.
pub fn snap_selection(
    scope: &Scope,
    position: LogicalPosition,
    anchor: LogicalPosition,
    granularity: Granularity,
) -> (LogicalPosition, LogicalPosition) {
    let backward = (position.line, position.col) < (anchor.line, anchor.col);
    let (lo, hi) = ordered(scope.clamp(position), scope.clamp(anchor));
    let (lo, hi) = match granularity {
        Granularity::Char => (lo, hi),
        Granularity::Word => (word_run(scope, lo).0, word_run(scope, hi).1),
        Granularity::Line => (
            LogicalPosition {
                line: lo.line,
                col: 0,
            },
            LogicalPosition {
                line: hi.line,
                col: line_byte_len_excl_newline(scope.doc(), hi.line),
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
    scope: &Scope,
    symbols: &[SymbolCandidate],
    position: LogicalPosition,
    anchor: LogicalPosition,
    motion: &Motion,
    extend: bool,
) -> (LogicalPosition, Option<LogicalPosition>) {
    let buf = scope.doc();
    // A no-op leaves the selection exactly as it was (no-symbols, or no further unit).
    let unchanged = (position, Some(anchor));
    // The outline describes the whole file, so a patch's element sees only the symbols inside its
    // own window. Filtering the *candidates* rather than the answer is what keeps a count walking:
    // `3o` steps to the third symbol in the element, not into the one above the hunk and then stop.
    let symbols: Vec<SymbolCandidate> = symbols
        .iter()
        .filter(|s| scope.contains(s.start) && scope.contains(s.end))
        .cloned()
        .collect();
    let symbols = symbols.as_slice();
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
            // count walks the outline.
            //
            // A count the outline cannot honour **refuses**, like every other counted motion: `5o`
            // with three symbols left does nothing rather than landing on the third. The count
            // names *which* symbol, and there isn't a fifth — the same rule `all_or_nothing`
            // applies to the motions that route through `single_step`. Nav units resolve here
            // instead (they need the symbol list), so the rule has to be spelled out again;
            // being scope-filtered, which is why they were excluded from `single_step`, answers a
            // different question.
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
                    None => {
                        landed = None;
                        break;
                    }
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
            let edge = symbol_edge(buf, symbols, position, to_end);
            // The candidate filter vets a symbol's **name** span; this returns its **body** edge,
            // and a body can reach past the field its name sits in. A target motion denies at the
            // field's edge, so an edge outside it is not a shorter move — it is no move.
            //
            // Latent today: nothing in the keymap constructs these two motions (`Shift-o` is
            // `IgnoreShift` over `o`, which resolves elsewhere). Guarded anyway, because the next
            // binding that reaches them should not have to rediscover this.
            if scope.contains(edge) {
                (edge, None)
            } else {
                unchanged
            }
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

/// Indices of every symbol whose range contains `pos`, outermost first — the cursor's scope chain,
/// as the status-bar breadcrumb shows it (`impl Foo` › `fn bar`).
///
/// Sorted by `(depth, start)` rather than trusting list order: the outline is flattened depth-first
/// so ancestors *do* precede descendants, but a flat `SymbolInformation` server's depths are
/// reconstructed from range containment (`assign_depth_by_containment`), which re-sorts. Sorting on
/// the same key both consumers use keeps the two paths agreeing.
///
/// Siblings never both contain `pos` in a well-formed outline, so this is the ancestor chain; a
/// server that reports overlapping siblings degrades to "deepest wins", which is what the picker's
/// cursor highlight already does.
pub(crate) fn enclosing_chain(symbols: &[SymbolCandidate], pos: LogicalPosition) -> Vec<usize> {
    let mut hits: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.contains(pos))
        .map(|(i, _)| i)
        .collect();
    hits.sort_by_key(|&i| {
        let s = &symbols[i];
        (s.depth, s.range_start.line, s.range_start.col)
    });
    hits
}

/// Index of the innermost symbol whose range contains `pos` — deepest depth, then latest start,
/// matching the picker's cursor-highlight rule. Used by [`symbol_edge`].
fn enclosing_symbol(symbols: &[SymbolCandidate], pos: LogicalPosition) -> Option<usize> {
    enclosing_chain(symbols, pos).pop()
}

/// Resolve `StartOfNavigationUnit` / `EndOfNavigationUnit` against the outline: land on the
/// enclosing symbol's start (or last char) and — when the cursor is already at that boundary —
/// fall through to the next/previous symbol in the list, so repeated `Shift-o` grows the selection
/// symbol by symbol.
fn symbol_edge(
    buf: &Document,
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
fn symbol_last_char(buf: &Document, sym: &SymbolCandidate) -> LogicalPosition {
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
    /// A document long enough to hold the outline above.
    ///
    /// Not `Document::scratch` on its own any more: a navigation motion is scoped like every other
    /// motion, so it only walks symbols inside the element's window — and an empty document's
    /// window holds no line 22 for `fn top` to be on.
    fn buffer() -> Document {
        let mut doc = Document::scratch(crate::state::DocumentId(1), None);
        doc.text = ropey::Rope::from_str(&"symbol\n".repeat(31));
        doc
    }

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

    /// The breadcrumb chain: every containing symbol, outermost first. Inside `fn b` that's the
    /// `impl S` wrapper then `fn b` itself.
    #[test]
    fn enclosing_chain_is_ordered_outermost_first() {
        let o = outline();
        assert_eq!(enclosing_chain(&o, at(11)), vec![1, 3]);
        // Inside the impl but between its members — the wrapper alone, not the nearest sibling.
        assert_eq!(enclosing_chain(&o, at(14)), vec![1, 3]);
        // A top-level symbol with no parent is a one-element chain.
        assert_eq!(enclosing_chain(&o, at(25)), vec![5]);
    }

    /// Between top-level symbols the chain is empty — the honest answer, and what makes the status
    /// bar go blank rather than sticking to whatever you last visited.
    #[test]
    fn enclosing_chain_is_empty_outside_every_symbol() {
        assert!(enclosing_chain(&outline(), at(21)).is_empty());
    }

    /// The chain's last element is exactly what the `o` motion and the picker highlight resolve to,
    /// so the breadcrumb can never disagree with where a navigation lands.
    #[test]
    fn enclosing_chain_ends_at_the_enclosing_symbol() {
        let o = outline();
        for line in [1, 6, 11, 16, 21, 25] {
            assert_eq!(
                enclosing_chain(&o, at(line)).pop(),
                enclosing_symbol(&o, at(line)),
                "line {line}"
            );
        }
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
        let buf = buffer();
        let scope = Scope::whole(&buf);
        let next = |pos, anchor| {
            resolve_navigation_motion(
                &scope,
                &o,
                pos,
                anchor,
                &Motion::NextNavigationUnit { count: 1 },
                false,
            )
        };
        let prev = |pos, anchor| {
            resolve_navigation_motion(
                &scope,
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
        let buf = buffer();
        let scope = Scope::whole(&buf);
        let nav = |count, forward| {
            let motion = if forward {
                Motion::NextNavigationUnit { count }
            } else {
                Motion::PrevNavigationUnit { count }
            };
            resolve_navigation_motion(&scope, &o, at(0), at(0), &motion, false)
        };
        // From the top, count walks the outline: count 1 → idx 1, count 2 → idx 2, count 3 → idx 3.
        assert_eq!(nav(1, true), (o[1].end, Some(o[1].start)));
        assert_eq!(nav(2, true), (o[2].end, Some(o[2].start)));
        assert_eq!(nav(3, true), (o[3].end, Some(o[3].start)));
        // An over-large count **refuses**, like every other counted motion. This assertion used to
        // pin the opposite — "clamps to the last reachable symbol rather than snapping back" — and
        // that was the last counted motion still doing so. The count names *which* symbol; with
        // five in the outline there is no ninety-ninth, and landing on the fifth is a different
        // request from the one that was made.
        assert_eq!(nav(99, true), (at(0), Some(at(0))));
        // count 0 behaves as 1 (the keymap never sends 0, but the resolver must stay total).
        assert_eq!(nav(0, true), (o[1].end, Some(o[1].start)));
        // From the top there's nothing before it, so Prev at any count is a no-op.
        assert_eq!(nav(3, false), (at(0), Some(at(0))));
    }

    #[test]
    fn extend_grows_the_selection_to_include_the_identifier() {
        let o = outline();
        let buf = buffer();
        let scope = Scope::whole(&buf);
        let ext = |pos, anchor, count, forward| {
            let motion = if forward {
                Motion::NextNavigationUnit { count }
            } else {
                Motion::PrevNavigationUnit { count }
            };
            resolve_navigation_motion(&scope, &o, pos, anchor, &motion, true)
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

#[cfg(test)]
mod scope_tests {
    use super::*;

    /// Six lines, where every line outside the window carries a letter no line inside it has —
    /// so a scan that escapes the window is *visible* as a landing, not merely as a clamp.
    ///
    /// `A` is line 0's alone and `C` line 4's; `t` occurs only inside.
    fn doc() -> Document {
        let mut doc = Document::scratch(crate::state::DocumentId(1), None);
        doc.text = ropey::Rope::from_str(
            "outside A\noutside B\ninside one\ninside two\noutside C\noutside D\n",
        );
        doc
    }

    fn at(line: u32, col: u32) -> LogicalPosition {
        LogicalPosition { line, col }
    }

    /// The window: lines 2 and 3 — a hunk in the middle of a file, the shape every view but a patch
    /// never has and a patch always does.
    fn hunk(doc: &Document) -> Scope<'_> {
        Scope::windowed(doc, 2, 4)
    }

    /// The line motions stop at the window's edges. This is the one rule that already worked, and
    /// it has to keep working through the new mechanism.
    #[test]
    fn line_motions_stop_at_the_windows_edges() {
        let doc = doc();
        let scope = hunk(&doc);
        let down = |from| {
            resolve_motion(
                &scope,
                from,
                &Motion::LogicalLine {
                    direction: Direction::Forward,
                    count: 1,
                    preserve_col: true,
                },
            )
        };
        let up = |from| {
            resolve_motion(
                &scope,
                from,
                &Motion::LogicalLine {
                    direction: Direction::Backward,
                    count: 1,
                    preserve_col: true,
                },
            )
        };
        assert_eq!(down(at(2, 0)).line, 3, "within the window it moves");
        // Uncounted, one step past the edge simply stays — refusal and clamping agree here, which
        // is the property that lets the counted case below differ without changing bare `j`/`k`.
        assert_eq!(down(at(3, 0)).line, 3, "and stops at the last line");
        assert_eq!(up(at(2, 0)).line, 2, "…and at the first");
        // A count the window cannot honour REFUSES — it used to land on the edge. `99j` two lines
        // from the end is not "go to the end", it is a request for a line that isn't there, and
        // answering it with a different line is what made `99j` then `99k` lose your place.
        let overshoot = |dir| {
            resolve_motion(
                &scope,
                at(2, 0),
                &Motion::LogicalLine {
                    direction: dir,
                    count: 99,
                    preserve_col: true,
                },
            )
            .line
        };
        assert_eq!(
            overshoot(Direction::Forward),
            2,
            "forward overshoot refuses"
        );
        assert_eq!(
            overshoot(Direction::Backward),
            2,
            "and so does a backward one, which `saturating_sub` used to swallow into line 0"
        );
    }

    /// Sanity probe for the ordinary editor case: a whole-document scope, `100j` both where the
    /// count *can* be honoured and where it cannot. The rule is all-or-nothing, **not** "never".
    #[test]
    fn a_counted_step_moves_the_whole_count_when_the_field_can_honour_it() {
        let mut doc = Document::scratch(crate::state::DocumentId(1), None);
        doc.text = ropey::Rope::from_str(&"x\n".repeat(500));
        let scope = Scope::whole(&doc);
        let at = |line| LogicalPosition { line, col: 0 };
        let j100 = |from| {
            resolve_motion(
                &scope,
                at(from),
                &Motion::LogicalLine {
                    direction: Direction::Forward,
                    count: 100,
                    preserve_col: true,
                },
            )
            .line
        };
        // Room for all 100: it moves all 100. This is the normal case and always was.
        assert_eq!(j100(0), 100);
        assert_eq!(j100(300), 400);
        // Not enough room: refuses outright rather than landing on the last line.
        assert_eq!(j100(480), 480, "no room for 100, so no movement at all");
    }

    /// A count the window cannot honour refuses; an uncounted step still clamps.
    ///
    /// The two are the same outcome for `j`/`k` — clamping onto the line you are already on moves
    /// nothing — which is why the split is invisible for the motion that motivated it. It is *not*
    /// the same outcome for `LogicalLineFirstNonblank`, which normalises the column as well: at the
    /// first line a bare `Alt-p` must still snap to the first non-blank. That asymmetry is the
    /// reason the rule is "counted refuses" rather than "out-of-range refuses".
    #[test]
    fn a_count_the_window_cannot_honour_refuses_but_a_bare_step_clamps() {
        // Indented text on the windowed lines, so a column normalisation is observable.
        let mut doc = Document::scratch(crate::state::DocumentId(1), None);
        doc.text = ropey::Rope::from_str("l0\nl1\n  l2\n  l3\nl4\nl5\n");
        // A window over lines 2..=3, the two indented ones.
        let scope = Scope::windowed(&doc, 2, 4);
        let at = |line, col| LogicalPosition { line, col };
        let line_step = |from, dir, count| {
            resolve_motion(
                &scope,
                from,
                &Motion::LogicalLine {
                    direction: dir,
                    count,
                    preserve_col: true,
                },
            )
        };
        let nonblank_step = |from, dir, count| {
            resolve_motion(
                &scope,
                from,
                &Motion::LogicalLineFirstNonblank {
                    direction: dir,
                    count,
                },
            )
        };

        // Counted past the window's end: refused, cursor untouched.
        assert_eq!(line_step(at(2, 0), Direction::Forward, 50).line, 2);
        assert_eq!(line_step(at(3, 0), Direction::Backward, 50).line, 3);
        // The same counts one step at a time still walk, so the window itself is traversable.
        assert_eq!(line_step(at(2, 0), Direction::Forward, 1).line, 3);
        assert_eq!(line_step(at(3, 0), Direction::Backward, 1).line, 2);
        // Bare step past the edge: clamps in place, as it always has.
        assert_eq!(line_step(at(3, 0), Direction::Forward, 1).line, 3);

        // The exception the rule exists for: at the window's last line a bare `p` clamps *and*
        // still normalises the column onto the first non-blank (col 2 of "  l3").
        assert_eq!(nonblank_step(at(3, 0), Direction::Forward, 1), at(3, 2));
        // But a count it cannot honour refuses outright — column included.
        assert_eq!(nonblank_step(at(3, 0), Direction::Forward, 50), at(3, 0));
    }

    /// Word motion runs out of text at the window's edge instead of walking into the next hunk's
    /// file. `w`/`b`/`e` went through no clamp at all before this.
    #[test]
    fn word_motion_runs_out_of_text_at_the_window() {
        let doc = doc();
        let scope = hunk(&doc);
        let word = |from, direction| {
            resolve_motion(
                &scope,
                from,
                &Motion::Word {
                    direction,
                    count: 20,
                    boundary: WordBoundary::Word,
                },
            )
        };
        let end = word(at(2, 0), Direction::Forward);
        assert_eq!(end.line, 3, "twenty words forward stop on the last line");
        assert!(scope.contains(end));
        let start = word(at(3, 5), Direction::Backward);
        assert_eq!(start, at(2, 0), "and backward at the first");
    }

    /// `w` — select word — advances through the window and then holds, rather than grabbing a word
    /// from the line below it. It never asked to be clamped at all; it went through its own handler.
    #[test]
    fn select_word_stops_on_the_last_word_of_the_window() {
        let doc = doc();
        let scope = hunk(&doc);
        let mut position = at(2, 0);
        let mut anchor = at(2, 0);
        for _ in 0..12 {
            let (p, a) = resolve_select_word(&scope, position, anchor, WordBoundary::Word, false);
            position = p;
            anchor = a;
            assert!(
                scope.contains(position) && scope.contains(anchor),
                "selection left the window: {position:?}..{anchor:?}"
            );
        }
        assert_eq!(position.line, 3, "it settles on the window's last word");
    }

    /// `f` for a char that is only outside the window doesn't move at all.
    ///
    /// The regression that named this whole change: the scan read the entire document, found its
    /// char hundreds of lines away, and the old clamp pinned the *line* to the window's edge while
    /// keeping the column it had found down there — a cursor in a place nothing put it.
    #[test]
    fn find_char_outside_the_window_does_not_move() {
        let doc = doc();
        let scope = hunk(&doc);
        let find = |ch, direction| {
            resolve_motion(
                &scope,
                at(2, 0),
                &Motion::FindChar {
                    ch,
                    direction,
                    count: 1,
                    till: false,
                },
            )
        };
        assert_eq!(find('C', Direction::Forward), at(2, 0), "`C` is on line 4");
        assert_eq!(find('A', Direction::Backward), at(2, 0), "`A` is on line 0");
        // A char that *is* in the window is still found, on whichever of its lines.
        let found = find('t', Direction::Forward);
        assert!(scope.contains(found), "{found:?}");
        assert_ne!(found, at(2, 0));
    }

    /// `gg` / `G` mean the window's ends. In a patch they are the hunk's, because the file's own
    /// ends are not in the view.
    #[test]
    fn buffer_ends_mean_the_windows_ends() {
        let doc = doc();
        let scope = hunk(&doc);
        assert_eq!(
            resolve_motion(&scope, at(3, 2), &Motion::BufferStart),
            at(2, 0)
        );
        assert_eq!(
            resolve_motion(&scope, at(2, 0), &Motion::BufferEnd),
            at(3, "inside two".len() as u32),
        );
    }

    /// Char steps stop at the window's edges too — including the one at its very end, which must
    /// land on the last line rather than at col 0 of the line after it.
    #[test]
    fn char_steps_stop_at_the_windows_edges() {
        let doc = doc();
        let scope = hunk(&doc);
        let step = |from, direction, count| {
            resolve_motion(&scope, from, &Motion::Char { direction, count })
        };
        assert_eq!(
            step(at(3, 0), Direction::Forward, 500),
            at(3, "inside two".len() as u32)
        );
        assert_eq!(step(at(2, 0), Direction::Backward, 500), at(2, 0));
        // And the step across the window's interior line break still works.
        assert_eq!(
            step(at(2, "inside one".len() as u32), Direction::Forward, 1),
            at(3, 0)
        );
    }

    /// A whole-buffer scope — every view but a patch — imposes nothing, so the motions resolve
    /// exactly as they did before any of this existed.
    #[test]
    fn a_whole_buffer_scope_imposes_nothing() {
        let doc = doc();
        let scope = Scope::whole(&doc);
        assert_eq!(
            resolve_motion(&scope, at(2, 0), &Motion::BufferStart),
            at(0, 0)
        );
        assert_eq!(
            resolve_motion(&scope, at(2, 0), &Motion::BufferEnd).line,
            doc.text.len_lines() as u32 - 1,
        );
        let found = resolve_motion(
            &scope,
            at(2, 0),
            &Motion::FindChar {
                ch: 'C',
                direction: Direction::Forward,
                count: 1,
                till: false,
            },
        );
        assert_eq!(
            found.line, 4,
            "the whole document is in view, so `C` is found"
        );
    }

    /// An extent is a claim about a buffer that an edit can outrun — a hunk said seven lines and
    /// the file has since lost four. The scope clamps at construction, so the motions downstream
    /// can't index past the end of a rope.
    #[test]
    fn a_stale_extent_clamps_to_the_lines_that_are_there() {
        let doc = doc();
        let scope = Scope::windowed(&doc, 4, 900);
        assert_eq!(scope.lines(), 4..doc.text.len_lines() as u32);
        // Even an extent starting past the end still holds one real line.
        let past = Scope::windowed(&doc, 900, 901);
        assert_eq!(past.lines().len(), 1);
        assert!(past.contains(past.clamp(at(900, 900))));
    }
}
