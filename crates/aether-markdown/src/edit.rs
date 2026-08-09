//! Block-edit resolution (docs/markdown-view.md §12, phase 3): each structural command as a
//! pure function from (source, parse, selection bytes, params) to **one text replacement** —
//! range, new text, landing selection — or a refusal. The server applies the replacement
//! through its normal edit pipeline (atomic: one apply, one undo entry, the usual pushes and
//! LSP sync); resolving here keeps the boundary math testable off-line and guarantees edits
//! act on the same boundaries the reading view rendered, because both sides run this crate's
//! parse.
//!
//! Conventions: byte offsets throughout; `range`/`text` describe the replacement against the
//! *current* document, `anchor`/`cursor` are offsets into the *resulting* document. Separator
//! blank lines belong to the *gaps between* blocks, never to the blocks (§12.1): removal takes
//! a block's lines plus the adjacent blank run, moves carry the gap along, and paste re-creates
//! whatever separator its new neighbours already use.
//!
//! That last part is the rule the whole file turns on: the right separator is the one already in
//! force at the seam, not a fixed blank line. Tight list items are newline-adjacent and a blank
//! line splits the list; top-level blocks are blank-separated and *removing* that blank line lets
//! them merge. Both directions are load-bearing — see [`separator_between`] for the case where
//! carrying a gap verbatim silently reparents a block.

use crate::{Block, Element, Span};
use std::ops::Range;

/// A resolved structural edit: replace `range` with `text`, then land the selection at
/// `anchor`/`cursor` (bytes into the resulting document; equal for a collapsed landing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEdit {
    pub range: Range<usize>,
    pub text: String,
    pub anchor: usize,
    pub cursor: usize,
}

/// Why a resolution declined. `Quiet` is a boundary no-op — the first block moving up, depth
/// past the ladder's end — and draws no toast, like `j` at the document's end. `Why` carries
/// the one-line reason the client shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Quiet,
    Why(&'static str),
}

pub type Resolved = Result<BlockEdit, Refusal>;

// ---- shared geometry ----------------------------------------------------------------------------

/// `byte` clamped into `text` and walked back to the nearest char boundary at or below it.
///
/// Every byte offset entering this file has to pass through here before it indexes anything.
/// Parse spans land on boundaries, but the two other sources don't: a *cursor* byte arrives from
/// a client whose parse may be a revision behind, and the file's own `end - 1` idiom (step off a
/// span's trailing newline onto its last line) walks into the middle of a multi-byte character
/// whenever a block's last character is non-ASCII — `Café` at EOF is enough. Slicing there
/// panics, and this runs inside the server, where that takes the connection down mid-edit.
fn floor_boundary(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn line_start(text: &str, byte: usize) -> usize {
    let byte = floor_boundary(text, byte);
    text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// One past the newline terminating the line containing `byte` (or EOF).
fn line_end_incl(text: &str, byte: usize) -> usize {
    let byte = floor_boundary(text, byte);
    text[byte..]
        .find('\n')
        .map(|i| byte + i + 1)
        .unwrap_or(text.len())
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The whole-line chunk covering spans `a..=b`: from `a`'s line start through the (included)
/// newline of `b`'s last content line. Spans include their trailing newline when present, so
/// `end - 1` sits on the last line either way — except *container* spans (lists), which
/// swallow their following blank line(s); separators belong to the gaps between blocks, so
/// trailing blank lines are trimmed back off.
fn chunk_of(text: &str, a: Span, b: Span) -> Range<usize> {
    let start = line_start(text, a.start as usize);
    let mut end = line_end_incl(text, (b.end as usize).saturating_sub(1));
    while end > start {
        let ls = line_start(text, end - 1);
        if is_blank(&text[ls..end]) {
            end = ls;
        } else {
            break;
        }
    }
    start..end
}

/// A point's resolution byte: the cursor's, stepped back off a line's terminating newline onto
/// that line's last content byte (the client's `focus_byte` rule). Only some block spans reach
/// their newline — a fence stops at its closing backtick — so the raw byte would resolve past
/// the block the cursor's bar is drawn on.
fn point_byte(text: &str, byte: u32) -> u32 {
    let at = floor_boundary(text, byte as usize);
    if text.as_bytes().get(at) == Some(&b'\n') && at > line_start(text, at) {
        (at - 1) as u32
    } else {
        at as u32
    }
}

/// End of the run of blank lines starting at `from` (a line start). Returns `from` when the
/// line there isn't blank.
fn blank_run_after(text: &str, from: usize) -> usize {
    let mut at = from;
    while at < text.len() {
        let end = line_end_incl(text, at);
        if !is_blank(&text[at..end]) {
            break;
        }
        at = end;
    }
    at
}

/// Start of the run of blank lines ending at `upto` (a line start). Returns `upto` when the
/// line before isn't blank.
fn blank_run_before(text: &str, upto: usize) -> usize {
    let mut at = upto;
    while at > 0 {
        let start = line_start(text, at - 1);
        if !is_blank(&text[start..at]) {
            break;
        }
        at = start;
    }
    at
}

/// The block range under an inclusive byte selection: `(top, bottom)` element indices. Edges
/// resolve forward at the top and back at the bottom, so a range edge sitting on a separator
/// line never rounds outward past its own blocks. (The reading view's
/// `ReadView::selection_blocks` derives from this.)
pub fn selection_block_range(
    text: &str,
    elements: &[Element],
    min: u32,
    max: u32,
) -> Option<(usize, usize)> {
    // A collapsed cursor is a reading *position*, not a line-grain selection: resolve it the
    // way the reading view derives focus — the innermost block containing the byte — so an op
    // acts on the block whose bar is lit. The extent heuristics below assume the selection's
    // line extent means something, and a point's doesn't: a cursor parked on a multi-child
    // container's own opening bytes (the `>` before its first child's span) sits on a line
    // whose whole-line chunk *is* the first child's, so the exact match below handed the
    // container's focus to that child — `Ctrl-j` on a focused nested quote moved its first
    // paragraph around inside it instead of moving the quote.
    if min == max {
        let i = crate::element_at_matching(elements, point_byte(text, min), Element::is_block)?;
        return Some((i, i));
    }
    let min = floor_boundary(text, min as usize) as u32;
    // First ask whether one block's own whole-line extent *is* the selection. Selections here
    // are line-grain, and that erases the difference between a container and the child it opens
    // with: `x` on a focused quote and `x` on that quote's first paragraph both start at the
    // quote's line. Only the extent tells them apart, so match on it before the prefix skip
    // below — which resolves the top edge inward and would answer "the first child" to both.
    // Innermost wins a tie, the same rule focus resolution follows.
    let sel = line_start(text, min as usize)..line_end_incl(text, max as usize);
    let exact = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let s = e.span();
            e.is_block() && s.start as usize >= sel.start && s.end as usize <= sel.end
        })
        .filter(|(_, e)| chunk_of(text, e.span(), e.span()) == sel)
        .min_by_key(|(_, e)| e.span().end - e.span().start)
        .map(|(i, _)| i);
    if let Some(top) = exact {
        return Some((top, top));
    }
    // A line's *prefix* is the intra-line form of the edge rule. A nested item's span starts at
    // its marker and a quoted block's at its text, so the indent and the `> ` in front of them
    // fall inside the enclosing container's span and outside their own — a whole-line selection
    // of the inner block (what `x` produces, what an in-place edit lands) would resolve its top
    // edge up to the container, and a one-block op would look like a two-block one. The prefix
    // belongs to the block it introduces.
    let head = &text[min as usize..];
    let skipped = min + (quote_prefix_len(head) + indent_after_prefix(head)) as u32;
    let min = skipped.min(max);
    let top = crate::element_at_matching(elements, min, Element::is_block)?;
    let bottom = crate::containing_element(elements, max, Element::is_block)
        .or_else(|| {
            elements
                .iter()
                .enumerate()
                .rev()
                .find(|(_, e)| e.is_block() && e.span().end as u64 <= max as u64 + 1)
                .map(|(i, _)| i)
        })
        .unwrap_or(top);
    Some((top, bottom))
}

/// What kind of container an ancestor is (depth's un-nest looks for the innermost `Item`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ancestor {
    List { ordered: bool },
    Item,
    Other,
}

/// Where a block-grain span sits in the block *tree*: the child sequence of its innermost
/// container (its siblings — for a top-level block that includes whole lists; for an item,
/// its fellow items) plus its index there, and the enclosing container chain outermost →
/// innermost. The element list cannot answer this — it is item-grain with no list nodes, so
/// "the paragraph's next sibling is the whole list" only falls out of the tree.
#[derive(Debug, Clone)]
struct TreePlace {
    siblings: Vec<Span>,
    index: usize,
    ancestors: Vec<(Ancestor, Span)>,
}

/// Find `target` in the block tree. `item_grain` says the target is a *list item*, which must
/// resolve to its fellow items and nothing else: a single-item list has the **same span as its
/// item**, so matching a block sequence by span would hand back the enclosing `[paragraph,
/// list]` as the item's "siblings" — making the item's neighbour its own parent's text. Indent
/// then nested it under the parent instead of a sibling (runaway indentation the reader can't
/// show) and move picked a neighbour chunk overlapping the moved one (a reversed range: a
/// panic).
fn locate(blocks: &[Block], target: Span, item_grain: bool) -> Option<TreePlace> {
    fn in_blocks(
        blocks: &[Block],
        target: Span,
        item_grain: bool,
        ancestors: &mut Vec<(Ancestor, Span)>,
    ) -> Option<TreePlace> {
        let sibs: Vec<Span> = blocks.iter().map(|b| b.span()).collect();
        if !item_grain {
            if let Some(index) = sibs.iter().position(|s| *s == target) {
                return Some(TreePlace {
                    siblings: sibs,
                    index,
                    ancestors: ancestors.clone(),
                });
            }
        }
        // More than one sibling can contain the target, so a branch that leads nowhere must not
        // end the search: inside a *tight* list item pulldown gives the paragraph the item's own
        // span, so the paragraph and the nested list after it both cover a grandchild item. The
        // paragraph is checked first, and bailing there hid every item three or more levels deep
        // — `locate` returned `None`, and depth changes and moves on them were refused outright.
        for b in blocks {
            let bs = b.span();
            if !(bs.start <= target.start && bs.end >= target.end) {
                continue;
            }
            let depth = ancestors.len();
            let found = match b {
                Block::List { items, ordered, .. } => {
                    let item_sibs: Vec<Span> = items.iter().map(|it| it.span).collect();
                    ancestors.push((Ancestor::List { ordered: *ordered }, bs));
                    if let Some(index) = item_sibs.iter().position(|s| *s == target) {
                        return Some(TreePlace {
                            siblings: item_sibs,
                            index,
                            ancestors: ancestors.clone(),
                        });
                    }
                    // Items don't overlap, so at most one can hold the target.
                    items
                        .iter()
                        .find(|it| it.span.start <= target.start && it.span.end >= target.end)
                        .and_then(|it| {
                            ancestors.push((Ancestor::Item, it.span));
                            in_blocks(&it.blocks, target, item_grain, ancestors)
                        })
                }
                Block::Quote { content, .. } | Block::FootnoteDef { content, .. } => {
                    ancestors.push((Ancestor::Other, bs));
                    in_blocks(content, target, item_grain, ancestors)
                }
                _ => None,
            };
            if found.is_some() {
                return found;
            }
            // Dead end: unwind whatever this branch pushed before trying the next sibling.
            ancestors.truncate(depth);
        }
        None
    }
    in_blocks(blocks, target, item_grain, &mut Vec::new())
}

fn front_matter_span(blocks: &[Block]) -> Option<Span> {
    match blocks.first() {
        Some(Block::FrontMatter { span, .. }) => Some(*span),
        _ => None,
    }
}

/// Refuse when a structural op would touch the document's front matter (§12.1). Front matter is
/// *positional* — it is only front matter while it opens the file — so moving it, replacing it,
/// or pushing anything above it silently demotes it to a thematic break and a heading. Every op
/// that relocates or removes block spans runs this.
fn guard_front_matter(blocks: &[Block], spans: &[Span]) -> Result<(), Refusal> {
    let fm = front_matter_span(blocks);
    if fm.is_some() && spans.iter().any(|s| Some(*s) == fm) {
        return Err(Refusal::Why("Front matter stays at the top"));
    }
    Ok(())
}

/// The same front matter, found without a parse: the opening `---`, the body, and the closing
/// fence line. [`resolve_move_paragraph`] runs on blank-line geometry alone so that it works in
/// any file type, and can't ask the parse where the front matter is — but it can read the fence.
/// Returns the full range so that a paragraph *inside* a front matter block with blank lines in
/// it is caught by overlap, not just the whole thing.
fn front_matter_range(text: &str) -> Option<Range<usize>> {
    let first_end = line_end_incl(text, 0);
    if text[..first_end].trim_end() != "---" {
        return None;
    }
    let mut at = first_end;
    while at < text.len() {
        let end = line_end_incl(text, at);
        let line = text[at..end].trim_end();
        if line == "---" || line == "..." {
            return Some(0..end);
        }
        at = end;
    }
    None
}

// ---- move ---------------------------------------------------------------------------------------

/// Length of the *container prefix* opening `line`: the run of blockquote markers, each `>` with
/// its optional space, `> > ` included.
///
/// This is where a block's own line begins. Inside a quote nothing starts at column 0, and every
/// piece of geometry in this file — markers, indentation, nesting depth — is relative to here,
/// not to the true start of the line. Measuring from column 0 instead is why ordered markers
/// went unrecognised inside quotes (the digits sit behind a `>`), and why nesting inserted its
/// padding *in front of* the marker and tore the quote apart.
fn quote_prefix_len(line: &str) -> usize {
    let mut at = 0;
    while let Some(n) = quote_marker_len(&line[at..]) {
        at += n;
    }
    at
}

/// The indentation of `line` — measured, like everything else, from after its container prefix.
fn indent_after_prefix(line: &str) -> usize {
    let base = quote_prefix_len(line);
    line[base..]
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// Whether `line` carries no content of its own — blank, or a bare quote marker (`>`), which is
/// how a blank line inside a quote is spelt.
fn is_blank_line(line: &str) -> bool {
    line[quote_prefix_len(line)..].trim().is_empty()
}

/// The byte range of the ordered-list number + delimiter opening `chunk`'s first line — `  12. `
/// yields the range covering `12.`, container prefix, indent and trailing spaces excluded.
/// `None` when the line doesn't open with one.
fn ordered_marker_range(chunk: &str) -> Option<Range<usize>> {
    let line = chunk.split('\n').next()?;
    let indent = quote_prefix_len(line) + indent_after_prefix(line);
    let digits = line[indent..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    match line.as_bytes().get(indent + digits) {
        Some(b'.') | Some(b')') => Some(indent..indent + digits + 1),
        _ => None,
    }
}

/// Swap the chunk `first` with `second` (whole-line ranges, `first` before `second`) across
/// `gap_text` — the separator that ends up between them once swapped, normally the source's own
/// gap (see [`separator_between`]). The replacement covers `first.start..second.end`. Newline
/// fixups keep every non-final chunk newline-terminated when a chunk from EOF moves up.
fn swap_chunks(
    text: &str,
    first: Range<usize>,
    second: Range<usize>,
    gap_text: &str,
    moved_is_first: bool,
    keep_markers: bool,
) -> BlockEdit {
    let mut first_txt = text[first.clone()].to_string();
    let mut second_txt = text[second.clone()].to_string();
    // `second` may end at EOF without a newline; once swapped it sits mid-document.
    if !second_txt.ends_with('\n') {
        second_txt.push('\n');
    }
    // A non-final chunk normally owns its newline; when it doesn't, fix it rather than assert —
    // the same EOF case as above, reached through a different path.
    if !first_txt.ends_with('\n') {
        first_txt.push('\n');
    }
    if keep_markers {
        // Ordered-list markers number *positions*, not the items occupying them: a renderer
        // starts the list at its first item's number and renumbers the rest from there, so a
        // marker travelling with its item silently restarts the whole list (moving `2.` to the
        // top made the list begin at 2). Trade the two first-line markers back. Doing it here,
        // before the lengths are read, keeps the landing offsets right when the widths differ
        // (`9.` ↔ `10.`).
        if let (Some(f), Some(s)) = (
            ordered_marker_range(&first_txt),
            ordered_marker_range(&second_txt),
        ) {
            let (ft, st) = (
                first_txt[f.clone()].to_string(),
                second_txt[s.clone()].to_string(),
            );
            first_txt.replace_range(f, &st);
            second_txt.replace_range(s, &ft);
        }
    }
    let range = first.start..second.end;
    let new = format!("{second_txt}{gap_text}{first_txt}");
    let (moved_start, moved_len) = if moved_is_first {
        (
            range.start + second_txt.len() + gap_text.len(),
            first_txt.len(),
        )
    } else {
        (range.start, second_txt.len())
    };
    BlockEdit {
        range,
        text: new,
        anchor: moved_start,
        // The landing selection covers the moved chunk whole-line: cursor on its final
        // newline (`end - 1`), anchor at its first line start.
        cursor: (moved_start + moved_len).saturating_sub(1),
    }
}

/// The separator to leave between two swapped siblings.
///
/// Normally the source's own gap, carried verbatim: inside a list that gap *is* the list's
/// structure (empty for a tight list, a blank line for a loose one), so rewriting it would
/// retype the list.
///
/// Everywhere else an *empty* gap is a hazard rather than a style. A list, a quote or a table
/// may interrupt a paragraph with no blank line between them, and that adjacency is not
/// symmetric: put the paragraph underneath instead and it re-parses as a lazy continuation
/// *inside* the container — appended to the last list item, swallowed by the quote — where it is
/// no longer a sibling and can never be moved back out. One blank line restores the boundary.
fn separator_between<'a>(text: &'a str, gap: Range<usize>, place: &TreePlace) -> &'a str {
    let gap = &text[gap];
    let siblings_are_list_items =
        matches!(place.ancestors.last(), Some((Ancestor::List { .. }, _)));
    if gap.is_empty() && !siblings_are_list_items {
        "\n"
    } else {
        gap
    }
}

/// Move the blocks under the selection past their next/previous sibling — separators travel
/// with the gap, tight list items swap cleanly, and the moved blocks land selected.
pub fn resolve_move_block(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    sel_min: u32,
    sel_max: u32,
    down: bool,
) -> Resolved {
    let (top, bottom) =
        selection_block_range(text, elements, sel_min, sel_max).ok_or(Refusal::Quiet)?;
    let (ts, bs) = (elements[top].span(), elements[bottom].span());
    let is_item = |e: &Element| matches!(e, Element::Item { .. });
    let tp = locate(blocks, ts, is_item(&elements[top])).ok_or(Refusal::Quiet)?;
    let bp = locate(blocks, bs, is_item(&elements[bottom])).ok_or(Refusal::Quiet)?;
    if tp.siblings != bp.siblings {
        return Err(Refusal::Why("Selection spans containers"));
    }
    let neighbor_index = if down {
        bp.index + 1
    } else if tp.index == 0 {
        return Err(Refusal::Quiet);
    } else {
        tp.index - 1
    };
    let Some(&ns) = tp.siblings.get(neighbor_index) else {
        return Err(Refusal::Quiet);
    };
    guard_front_matter(blocks, &[ts, bs, ns])?;
    let moved = chunk_of(text, ts, bs);
    let other = chunk_of(text, ns, ns);
    let (first, second) = if down { (moved, other) } else { (other, moved) };
    // Siblings occupy disjoint lines, so the neighbour's chunk sits wholly before the moved one.
    // Should some parse quirk break that, refuse rather than slice a reversed range: this runs
    // inside the server, where a panic takes the daemon down with it.
    if first.end > second.start {
        // No `debug_assert!` here: the dev build *is* the daily driver, so asserting would
        // panic exactly the server this guard exists to keep alive.
        return Err(Refusal::Quiet);
    }
    let gap = separator_between(text, first.end..second.start, &tp);
    // Only between fellow items of an *ordered* list: elsewhere a leading `12.` is content, not
    // a position (an indented code block's first line can look exactly like a marker).
    let ordered_items = matches!(
        tp.ancestors.last(),
        Some((Ancestor::List { ordered: true }, _))
    );
    let indent = indent_before(text, ts.start);
    let edit = swap_chunks(text, first, second, gap, down, ordered_items);
    let landed_start = edit.anchor + indent;
    Ok(collapse_if_point(edit, sel_min == sel_max, landed_start))
}

/// Move the blank-line-delimited paragraph under the selection past its neighbour — the
/// editor's `Ctrl-Alt-j`/`k`, blank-line geometry only, so it works in any file type.
pub fn resolve_move_paragraph(text: &str, sel_min: u32, sel_max: u32, down: bool) -> Resolved {
    // A "paragraph" here is a maximal run of non-blank lines — language-agnostic, the
    // editor's grain. The selection's lines must sit on content, not in a gap.
    let para_around = |byte: usize| -> Option<Range<usize>> {
        let start = line_start(text, byte.min(text.len().saturating_sub(1)));
        if text.is_empty() || is_blank(&text[start..line_end_incl(text, start)]) {
            return None;
        }
        let mut s = start;
        while s > 0 {
            let prev = line_start(text, s - 1);
            if is_blank(&text[prev..s]) {
                break;
            }
            s = prev;
        }
        let mut e = line_end_incl(text, start);
        while e < text.len() {
            let next_end = line_end_incl(text, e);
            if is_blank(&text[e..next_end]) {
                break;
            }
            e = next_end;
        }
        Some(s..e)
    };
    let a = para_around(sel_min as usize).ok_or(Refusal::Quiet)?;
    let b = para_around(sel_max as usize).ok_or(Refusal::Quiet)?;
    let moved = a.start.min(b.start)..a.end.max(b.end);
    let (gap, other) = if down {
        let gap = moved.end..blank_run_after(text, moved.end);
        if gap.end >= text.len() {
            return Err(Refusal::Quiet);
        }
        let other = para_around(gap.end).ok_or(Refusal::Quiet)?;
        (gap, other)
    } else {
        let gap = blank_run_before(text, moved.start)..moved.start;
        if gap.start == 0 {
            return Err(Refusal::Quiet);
        }
        let other = para_around(gap.start - 1).ok_or(Refusal::Quiet)?;
        (gap, other)
    };
    // Front matter is positional (§12.1) — the same rule `guard_front_matter` enforces for the
    // block ops, applied here to the one resolver that runs without a parse. The fence is read
    // straight out of the text for that reason; a non-markdown file that opens `---` pays a
    // refusal it didn't need, which is the safe direction to be wrong in.
    if let Some(fm) = front_matter_range(text) {
        let touches = |r: &Range<usize>| r.start < fm.end && fm.start < r.end;
        if touches(&moved) || touches(&other) {
            return Err(Refusal::Why("Front matter stays at the top"));
        }
    }
    // The gap here is a blank *run* by construction (a paragraph is a maximal run of non-blank
    // lines, so its neighbour is always a blank line away) — nothing to normalize.
    let gap_text = &text[gap];
    let (first, second) = if down { (moved, other) } else { (other, moved) };
    let edit = swap_chunks(text, first, second, gap_text, down, false);
    let landed_start = edit.anchor;
    Ok(collapse_if_point(edit, sel_min == sel_max, landed_start))
}

// ---- delete (cut) -------------------------------------------------------------------------------

/// The "around block" removal range for blocks `top..=bottom`: their lines plus the trailing
/// blank run — or the leading run when there is no trailing one (the document's last block),
/// so blank lines never accumulate (vim's `dap`).
fn around_range(text: &str, ts: Span, bs: Span) -> Range<usize> {
    let body = chunk_of(text, ts, bs);
    let after = blank_run_after(text, body.end);
    if after > body.end {
        body.start..after
    } else {
        // No trailing run — the document's last block (or a tight neighbour, where the
        // leading run is empty too): take the leading one.
        blank_run_before(text, body.start)..body.end
    }
}

/// Removing the *head* of an ordered list, when items survive below it, hands the head's number
/// to the first survivor — same rule as the move's marker swap, for the same reason: a renderer
/// numbers an ordered list from its first item's marker, so letting the head go renumbers
/// everything under it (cutting `1.` off a 1/2/3 list left it starting at 2). Returns the
/// removal's new end and the text replacing it — one contiguous replacement, the survivor's
/// indent re-emitted verbatim with only its number exchanged.
fn ordered_head_handoff(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    top: usize,
    bottom: usize,
    body: &Range<usize>,
    removal: &Range<usize>,
) -> Option<(usize, String)> {
    let is_item = |i: usize| matches!(elements[i], Element::Item { .. });
    if !is_item(top) || !is_item(bottom) {
        return None;
    }
    let tp = locate(blocks, elements[top].span(), true)?;
    let ordered = matches!(
        tp.ancestors.last(),
        Some((Ancestor::List { ordered: true }, _))
    );
    if !ordered || tp.index != 0 {
        return None;
    }
    // A survivor has to exist in the same list, below everything being removed.
    let bp = locate(blocks, elements[bottom].span(), true)?;
    if bp.siblings != tp.siblings || bp.index + 1 >= tp.siblings.len() {
        return None;
    }
    let head = ordered_marker_range(&text[body.start..])?;
    let head_number = &text[body.start + head.start..body.start + head.end];
    let surv = ordered_marker_range(&text[removal.end..])?;
    let mut replacement = text[removal.end..removal.end + surv.start].to_string();
    replacement.push_str(head_number);
    Some((removal.end + surv.end, replacement))
}

/// Cut the selected block(s): around-block removal, the blocks' own lines (separator
/// excluded) returned for the clipboard. Cursor lands collapsed where the removed range was —
/// the next block's line.
pub fn resolve_delete(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    sel_min: u32,
    sel_max: u32,
) -> Result<(BlockEdit, String), Refusal> {
    let (top, bottom) =
        selection_block_range(text, elements, sel_min, sel_max).ok_or(Refusal::Quiet)?;
    let (ts, bs) = (elements[top].span(), elements[bottom].span());
    guard_front_matter(blocks, &[ts, bs])?;
    let body = chunk_of(text, ts, bs);
    let clipboard = text[body.clone()].to_string();
    let mut range = around_range(text, ts, bs);
    let mut replacement = String::new();
    if let Some((end, marked)) =
        ordered_head_handoff(text, blocks, elements, top, bottom, &body, &range)
    {
        range.end = end;
        replacement = marked;
    }
    let new_len = text.len() - (range.end - range.start) + replacement.len();
    let cursor = range.start.min(new_len.saturating_sub(1));
    Ok((
        BlockEdit {
            range,
            text: replacement,
            anchor: cursor,
            cursor,
        },
        clipboard,
    ))
}

// ---- paste --------------------------------------------------------------------------------------

/// Paste `clip` as its own block before the selection's top block (`replace: false`), or in
/// place of the selected block(s) (`replace: true`). Boundary-normalized: whatever shape the
/// clipboard text is in, the pasted block is separated from its new neighbours exactly the way
/// they already separate themselves — a blank line between top-level blocks, nothing between
/// tight list items (a blank line there would split the list). So cut→paste round-trips
/// wherever it lands. The pasted text lands selected.
pub fn resolve_paste(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    sel_min: u32,
    sel_max: u32,
    clip: &str,
    replace: bool,
) -> Resolved {
    let body = clip.trim_end();
    if body.is_empty() {
        return Err(Refusal::Quiet);
    }
    let Some((top, bottom)) = selection_block_range(text, elements, sel_min, sel_max) else {
        // No block to anchor against. That is the empty document — where the paste becomes the
        // whole content — but *not only* the empty document: a file the parse yields no blocks
        // for (nothing but link reference definitions, say) reaches here with every byte of its
        // content intact. Replacing `0..text.len()` on the strength of an empty element list
        // would delete a document the reading view merely had nothing to show for, so anything
        // non-blank keeps its text and takes the paste at the end.
        if text.trim().is_empty() {
            let text_out = format!("{body}\n");
            return Ok(BlockEdit {
                range: 0..text.len(),
                cursor: text_out.len().saturating_sub(1),
                anchor: 0,
                text: text_out,
            });
        }
        let at = text.len();
        let sep = match () {
            _ if text.ends_with("\n\n") => "",
            _ if text.ends_with('\n') => "\n",
            _ => "\n\n",
        };
        let new = format!("{sep}{body}\n");
        let anchor = at + sep.len();
        return Ok(collapse_if_point(
            BlockEdit {
                cursor: anchor + body.len(),
                anchor,
                range: at..at,
                text: new,
            },
            sel_min == sel_max,
            anchor,
        ));
    };
    let (ts, bs) = (elements[top].span(), elements[bottom].span());
    guard_front_matter(blocks, &[ts, bs])?;
    if replace {
        let range = around_range(text, ts, bs);
        let body_chunk = chunk_of(text, ts, bs);
        // Give back exactly the separator the removal consumed, so the surrounding structure is
        // untouched: a trailing-run removal re-adds the blank line after the pasted block
        // (unless it reached EOF, where a trailing blank shouldn't be resurrected); a
        // leading-run removal (the document's last block) re-adds it before; a removal that
        // took no blank run at all — tight list items — re-adds none, only the terminator.
        let prefix = if range.start < body_chunk.start {
            "\n"
        } else {
            ""
        };
        let suffix = if range.end > body_chunk.end && range.end < text.len() {
            "\n\n"
        } else {
            "\n"
        };
        let new = format!("{prefix}{body}{suffix}");
        let anchor = range.start + prefix.len();
        return Ok(collapse_if_point(
            BlockEdit {
                // Whole-line landing over the pasted block: cursor on the newline after it.
                cursor: anchor + body.len(),
                anchor,
                range,
                text: new,
            },
            sel_min == sel_max,
            anchor,
        ));
    }
    let at = line_start(text, ts.start as usize);
    // Match the seam we're inserting into: a blank line before the block we push down, unless
    // its predecessor is newline-adjacent (tight list items), where one would split the list.
    let tight = if at > 0 {
        !is_blank(&text[line_start(text, at - 1)..at])
    } else {
        // Nothing above the document's first block to copy a seam from. Stay tight only when
        // both sides are list items, so cutting an item and pasting it back restores the list
        // instead of splitting it in two — while pasting prose above a list still gets its
        // blank line.
        matches!(elements[top], Element::Item { .. })
            && marker_width(body.split('\n').next().unwrap_or("").trim_start()).is_some()
    };
    let new = format!("{body}\n{}", if tight { "" } else { "\n" });
    Ok(collapse_if_point(
        BlockEdit {
            range: at..at,
            anchor: at,
            // Whole-line landing over the pasted block: cursor on its final newline.
            cursor: at + body.len(),
            text: new,
        },
        sel_min == sel_max,
        at,
    ))
}

// ---- depth --------------------------------------------------------------------------------------

/// Width of a list marker prefix at the start of `line` (indent excluded): `- `, `* `, `+ `,
/// `12. `, `3) ` — marker chars plus the following spaces. `None` when `line` doesn't open
/// with a marker.
fn marker_width(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    let after = if digits > 0 {
        match bytes.get(digits) {
            Some(b'.') | Some(b')') => digits + 1,
            _ => return None,
        }
    } else {
        match bytes.first() {
            Some(b'-') | Some(b'*') | Some(b'+') => 1,
            _ => return None,
        }
    };
    let spaces = bytes[after..].iter().take_while(|b| **b == b' ').count();
    if spaces == 0 {
        return None;
    }
    Some(after + spaces)
}

/// Hand back the selection state the op was given: invoked from a bare reading position, a
/// structural edit leaves one — parked on the block it acted on — rather than manufacturing a
/// selection the user didn't ask for; invoked on a selection, it keeps the selection so repeated
/// presses keep acting on the same blocks.
///
/// `inside` is the byte to park on: the block's *own* start in the resulting text, not its
/// line's. For a nested list item those differ by the indent, and the indent belongs to the
/// parent item's span — parking there would derive the focus to the parent.
fn collapse_if_point(edit: BlockEdit, was_point: bool, inside: usize) -> BlockEdit {
    if was_point {
        BlockEdit {
            anchor: inside,
            cursor: inside,
            ..edit
        }
    } else {
        edit
    }
}

/// The indent of the line `byte` sits on — the gap between a block's line start and its own span.
fn indent_before(text: &str, byte: u32) -> usize {
    byte as usize - line_start(text, byte as usize)
}

/// The whole-line landing over a block edited *in place*: `chunk` is its line extent before the
/// edit, `grew` how many bytes the replacement added (negative when it shrank). Depth leaves the
/// block it changed selected, like move and paste (§12.1) — and here that is load-bearing rather
/// than cosmetic: a *collapsed* landing on a nested item's line start sits in the indent, which
/// belongs to the parent's span, so the focus would jump to the parent and the next press would
/// act on it instead.
fn landing_after_inplace_edit(chunk: &Range<usize>, grew: isize) -> (usize, usize) {
    let end = (chunk.end as isize + grew).max(chunk.start as isize + 1) as usize;
    (chunk.start, end - 1)
}

/// Wrap `chunk` in one more level of blockquote. Blank lines inside get a bare `>` too: a blank
/// line *without* the marker ends the quote, so an unmarked separator would split one quote into
/// two and drop the blocks after it back out — the same separator trap the rest of this file
/// works around.
fn quote_wrap(chunk: &str) -> String {
    split_lines_incl(chunk)
        .map(|l| {
            let (body, nl) = l.strip_suffix('\n').map_or((l, ""), |b| (b, "\n"));
            if body.trim().is_empty() {
                format!(">{nl}")
            } else {
                format!("> {body}{nl}")
            }
        })
        .collect()
}

/// Strip one blockquote level from `chunk`. Lines carrying no marker are lazy continuations and
/// pass through untouched — dropping the marker from the lines that have one leaves the
/// paragraph intact either way.
fn quote_unwrap(chunk: &str) -> String {
    split_lines_incl(chunk)
        .map(|l| &l[quote_marker_len(l).unwrap_or(0)..])
        .collect()
}

/// Length of the blockquote marker opening `line` — up to three leading spaces, the `>`, and one
/// optional space after it. `None` when the line doesn't open a quote.
fn quote_marker_len(line: &str) -> Option<usize> {
    let indent = line.bytes().take_while(|b| *b == b' ').count();
    if indent > 3 || line.as_bytes().get(indent) != Some(&b'>') {
        return None;
    }
    let after = indent + 1;
    Some(after + usize::from(line.as_bytes().get(after) == Some(&b' ')))
}

/// `Ctrl-l`/`Ctrl-h`: deepen or flatten the selection, at whatever "deeper" means for what's
/// selected — a heading's level, a list item's nesting, and for everything else a blockquote
/// level (a quote *is* a container in the block tree, so wrapping is genuinely one step down;
/// changing a paragraph to a code block would be a change of kind, not of depth).
///
/// Headings and items are one-at-a-time and only when they're the whole selection; a run of
/// blocks quotes together, separators and all.
pub fn resolve_depth(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    sel_min: u32,
    sel_max: u32,
    deeper: bool,
) -> Resolved {
    let (top, bottom) =
        selection_block_range(text, elements, sel_min, sel_max).ok_or(Refusal::Quiet)?;
    if top != bottom {
        // A run of list items has its own obvious reading — nest them all — which isn't built;
        // say so rather than silently quoting them instead.
        if (top..=bottom).all(|i| matches!(elements[i], Element::Item { .. })) {
            return Err(Refusal::Why("Nest one list item at a time"));
        }
        return resolve_quote_depth(
            text,
            blocks,
            elements,
            top,
            bottom,
            deeper,
            sel_min == sel_max,
        );
    }
    match &elements[top] {
        Element::Heading { span, level, .. } => {
            let at = span.start as usize;
            if !text[at..].starts_with('#') {
                return Err(Refusal::Why("Setext headings keep their level"));
            }
            if deeper && *level >= 6 {
                return Err(Refusal::Quiet);
            }
            if !deeper && *level <= 1 {
                return Err(Refusal::Quiet);
            }
            let (range, new) = if deeper {
                (at..at, "#".to_string())
            } else {
                (at..at + 1, String::new())
            };
            let grew = new.len() as isize - (range.end - range.start) as isize;
            let (anchor, cursor) = landing_after_inplace_edit(&chunk_of(text, *span, *span), grew);
            // The `#` run is edited at the span's own start, so that start doesn't move.
            Ok(collapse_if_point(
                BlockEdit {
                    range,
                    text: new,
                    anchor,
                    cursor,
                },
                sel_min == sel_max,
                at,
            ))
        }
        Element::Item { span, .. } => {
            let item_range = chunk_of(text, *span, *span);
            // Everything below is measured from after the container prefix: inside a quote the
            // item's line starts at `> `, not at column 0, and padding inserted before that
            // marker tears the quote apart.
            let first_line = &text[item_range.start..line_end_incl(text, item_range.start)];
            let prefix = quote_prefix_len(first_line);
            let indent = span.start as usize - item_range.start - prefix;
            let place = locate(blocks, *span, true).ok_or(Refusal::Quiet)?;
            if deeper {
                // Nest under the previous sibling: indent every line by that sibling's
                // marker-prefix width, so continuation alignment survives any marker
                // (`- ` = 2, `12. ` = 4). No sibling above → nothing to nest under.
                if place.index == 0 {
                    return Err(Refusal::Quiet);
                }
                let ps = place.siblings[place.index - 1];
                let prev_line = &text[ps.start as usize..line_end_incl(text, ps.start as usize)];
                let unit = marker_width(prev_line.trim_end()).ok_or(Refusal::Quiet)?;
                let pad = " ".repeat(unit);
                let mut new: String = split_lines_incl(&text[item_range.clone()])
                    .map(|l| {
                        if is_blank_line(l) {
                            l.to_string()
                        } else {
                            let b = quote_prefix_len(l);
                            format!("{}{pad}{}", &l[..b], &l[b..])
                        }
                    })
                    .collect();
                // An ordered item numbered anything but 1 cannot interrupt a paragraph
                // (CommonMark), so nesting `2.` directly under its parent's own text makes it a
                // *lazy continuation* of that text — the reader shows the two items merged into
                // one line. A nested list that starts a fresh sequence must therefore start at
                // 1. Joining a list that already exists at that indent, the number is free: it
                // follows a list item rather than a paragraph, and the renderer counts from the
                // list's first marker anyway.
                let nests_below_a_list_item = item_range.start > 0 && {
                    let above = &text[line_start(text, item_range.start - 1)..item_range.start];
                    let base = quote_prefix_len(above);
                    indent_after_prefix(above) == indent + unit
                        && marker_width(above[base..].trim()).is_some()
                };
                if !nests_below_a_list_item {
                    if let Some(m) = ordered_marker_range(&new) {
                        new.replace_range(m.start..m.end - 1, "1");
                    }
                }
                let grew = new.len() as isize - (item_range.end - item_range.start) as isize;
                let (anchor, cursor) = landing_after_inplace_edit(&item_range, grew);
                let landed = anchor + prefix + indent + unit;
                Ok(collapse_if_point(
                    BlockEdit {
                        range: item_range,
                        text: new,
                        anchor,
                        cursor,
                    },
                    sel_min == sel_max,
                    landed,
                ))
            } else {
                // Un-nest to the enclosing item's level: strip the indent difference.
                // Top-level items have nowhere shallower to go.
                let (_, pspan) = *place
                    .ancestors
                    .iter()
                    .rev()
                    .find(|(k, _)| *k == Ancestor::Item)
                    .ok_or(Refusal::Quiet)?;
                let pline = line_start(text, pspan.start as usize);
                let parent_indent = pspan.start as usize
                    - pline
                    - quote_prefix_len(&text[pline..line_end_incl(text, pline)]);
                let delta = indent.saturating_sub(parent_indent);
                if delta == 0 {
                    return Err(Refusal::Quiet);
                }
                let new: String = split_lines_incl(&text[item_range.clone()])
                    .map(|l| {
                        let b = quote_prefix_len(l);
                        let strip = l[b..]
                            .bytes()
                            .take(delta)
                            .take_while(|c| *c == b' ')
                            .count();
                        format!("{}{}", &l[..b], &l[b + strip..])
                    })
                    .collect();
                let grew = new.len() as isize - (item_range.end - item_range.start) as isize;
                let (anchor, cursor) = landing_after_inplace_edit(&item_range, grew);
                let landed = anchor + prefix + parent_indent;
                Ok(collapse_if_point(
                    BlockEdit {
                        range: item_range,
                        text: new,
                        anchor,
                        cursor,
                    },
                    sel_min == sel_max,
                    landed,
                ))
            }
        }
        _ => resolve_quote_depth(
            text,
            blocks,
            elements,
            top,
            bottom,
            deeper,
            sel_min == sel_max,
        ),
    }
}

/// The blockquote rung of [`resolve_depth`]: `Ctrl-l` wraps the selected block(s) in one more
/// level, `Ctrl-h` peels one off. Repeats both ways — nested quotes are legal markdown, and the
/// container is the reading grain, so the wrapped result is the block the next press acts on.
fn resolve_quote_depth(
    text: &str,
    blocks: &[Block],
    elements: &[Element],
    top: usize,
    bottom: usize,
    deeper: bool,
    was_point: bool,
) -> Resolved {
    let (ts, bs) = (elements[top].span(), elements[bottom].span());
    guard_front_matter(blocks, &[ts, bs])?;
    let chunk = chunk_of(text, ts, bs);
    let quoted = quote_marker_len(&text[chunk.clone()]).is_some();
    if !deeper && !quoted {
        // Already as shallow as it goes, like `Ctrl-h` on an H1.
        return Err(Refusal::Quiet);
    }
    let new = if deeper {
        quote_wrap(&text[chunk.clone()])
    } else {
        quote_unwrap(&text[chunk.clone()])
    };
    let grew = new.len() as isize - (chunk.end - chunk.start) as isize;
    let (anchor, cursor) = landing_after_inplace_edit(&chunk, grew);
    // Park past the container prefix the wrap just added (or the unwrap just removed), not on
    // the line start: that byte is the `>` itself, which belongs to the *enclosing* quote's span
    // and not to the block we acted on, so focus would lift to the container. Reading it off the
    // result's own first line covers every direction — wrapping a bare paragraph, wrapping one
    // already inside a quote, and peeling a level back off.
    let landed = anchor + quote_prefix_len(new.split('\n').next().unwrap_or(""));
    Ok(collapse_if_point(
        BlockEdit {
            range: chunk,
            text: new,
            anchor,
            cursor,
        },
        was_point,
        landed,
    ))
}

/// Split into lines *keeping* each terminating newline (the final line may lack one).
fn split_lines_incl(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let cut = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let (line, tail) = rest.split_at(cut);
        rest = tail;
        Some(line)
    })
}

// ---- toggle task --------------------------------------------------------------------------------

/// `Enter` on a task item: flip its checkbox. `byte` is the cursor; the innermost task item
/// containing it — and only a containing one — is the target. The document length is
/// unchanged, so the cursor stays put.
pub fn resolve_toggle_task(
    text: &str,
    elements: &[Element],
    byte: u32,
    set: Option<bool>,
) -> Resolved {
    let is_task = |e: &Element| {
        matches!(
            e,
            Element::Item {
                checked: Some(_),
                ..
            }
        )
    };
    // Strictly the cursor's *own* item: any position inside it counts — the containing walk
    // reaches the checkbox from a loose item's second paragraph or a nested fence — but a
    // neighbouring block does not. `Ctrl-a` sends unconditionally (in the editor there is no
    // client parse to gate on), so this resolution is the only gate, and falling forward/back
    // the way block focus derives would let the key act at a distance: from any block near a
    // task list it toggled the next item's box — or the document's last, past the final list —
    // with nothing on screen marking the box it was about to hit. The newline step-back keeps
    // a whole-line-selected item resolving to itself.
    let at = point_byte(text, byte) as usize;
    // A blank line is a separator, and separators belong to the gaps between blocks (§12.1) —
    // but a loose item's *parse* span swallows the blank after it, so without this guard the
    // containing walk would toggle the item above from the gap below it.
    if is_blank_line(&text[line_start(text, at)..line_end_incl(text, at)]) {
        return Err(Refusal::Quiet);
    }
    let idx = crate::containing_element(elements, at as u32, is_task).ok_or(Refusal::Quiet)?;
    let span = elements[idx].span();
    let first_line_end = line_end_incl(text, span.start as usize);
    let line = &text[span.start as usize..first_line_end];
    // The checkbox sits immediately after the item's marker — read it *there* rather than
    // searching the line: a checked item whose text mentions `[ ]` ("handle the [ ] case")
    // would otherwise have its body rewritten instead of its box.
    let box_at = marker_width(line).ok_or(Refusal::Quiet)?;
    let checked = match line.get(box_at..box_at + 3) {
        Some("[ ]") => false,
        Some("[x]") | Some("[X]") => true,
        _ => return Err(Refusal::Quiet),
    };
    // `None` flips; an explicit request for the state the box is already in is a no-op, so a held
    // `Ctrl-a` settles a list instead of flapping it.
    let want = match set {
        Some(want) if want == checked => return Err(Refusal::Quiet),
        Some(want) => want,
        None => !checked,
    };
    let inner = span.start as usize + box_at + 1;
    Ok(BlockEdit {
        range: inner..inner + 1,
        text: if want { "x" } else { " " }.to_string(),
        anchor: byte as usize,
        cursor: byte as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{elements, parse};

    fn fixture(md: &str) -> (Vec<Block>, Vec<Element>) {
        let blocks = parse(md);
        let els = elements(&blocks);
        (blocks, els)
    }

    /// Apply a resolved edit and return the new text.
    fn apply(text: &str, e: &BlockEdit) -> String {
        let mut out = text.to_string();
        out.replace_range(e.range.clone(), &e.text);
        out
    }

    /// Where an edit's landing put the reading position in `out`: the block-grain source it
    /// resolves to, and whether it left a selection. Structural edits hand back the selection
    /// state they were given, so both halves matter.
    fn landing(out: &str, e: &BlockEdit) -> (String, bool) {
        let els = elements(&parse(out));
        let (lo, hi) = (e.anchor.min(e.cursor) as u32, e.anchor.max(e.cursor) as u32);
        let (top, _) = selection_block_range(out, &els, lo, hi).expect("landing resolves");
        let span = els[top].span();
        (
            out[span.start as usize..(span.end as usize).min(out.len())].to_string(),
            e.anchor != e.cursor,
        )
    }

    const DOC: &str = "# Title\n\nAlpha one.\n\nBeta two.\n\nGamma three.\n";
    // Byte map: heading 0..8, Alpha 9..20, Beta 21..31, Gamma 32..45.

    #[test]
    fn move_block_down_swaps_with_the_next_sibling() {
        let (blocks, els) = fixture(DOC);
        // Cursor inside "Alpha one." (byte 9): move down past Beta.
        let e = resolve_move_block(DOC, &blocks, &els, 9, 9, true).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(out, "# Title\n\nBeta two.\n\nAlpha one.\n\nGamma three.\n");
        // The moved block lands selected whole-line: anchor at its new line start, cursor on
        // its final newline.
        assert_eq!(landing(&out, &e), ("Alpha one.\n".into(), false));
    }

    #[test]
    fn move_block_up_and_saturation_at_the_ends() {
        let (blocks, els) = fixture(DOC);
        let e = resolve_move_block(DOC, &blocks, &els, 21, 21, false).unwrap();
        assert_eq!(
            apply(DOC, &e),
            "# Title\n\nBeta two.\n\nAlpha one.\n\nGamma three.\n"
        );
        // The heading has no previous sibling; the last block no next: quiet no-ops.
        assert_eq!(
            resolve_move_block(DOC, &blocks, &els, 0, 0, false),
            Err(Refusal::Quiet)
        );
        assert_eq!(
            resolve_move_block(DOC, &blocks, &els, 33, 33, true),
            Err(Refusal::Quiet)
        );
    }

    #[test]
    fn move_block_down_past_the_eof_block_fixes_newlines() {
        let doc = "Alpha.\n\nOmega";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 0, 0, true).unwrap();
        // Omega gains its newline (now mid-file); the file ends with Alpha's.
        assert_eq!(apply(doc, &e), "Omega\n\nAlpha.\n");
    }

    #[test]
    fn move_tight_list_items_swap_without_separators() {
        let doc = "- one\n- two\n- three\n";
        let (blocks, els) = fixture(doc);
        // Cursor on "two" (byte 8): move it up.
        let e = resolve_move_block(doc, &blocks, &els, 8, 8, false).unwrap();
        assert_eq!(apply(doc, &e), "- two\n- one\n- three\n");
    }

    #[test]
    fn move_paragraph_past_a_whole_list_not_into_it() {
        let doc = "Intro.\n\n- a\n- b\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 0, 0, true).unwrap();
        // The paragraph's sibling is the LIST, not its first item: it jumps the whole list.
        assert_eq!(apply(doc, &e), "- a\n- b\n\nIntro.\n\nOutro.\n");
    }

    #[test]
    fn move_refuses_front_matter_and_container_crossing() {
        let doc = "---\nkey: v\n---\n\nBody.\n";
        let (blocks, els) = fixture(doc);
        let front = resolve_move_block(doc, &blocks, &els, 0, 0, true);
        assert!(matches!(front, Err(Refusal::Why(_))), "{front:?}");
        // Moving Body up would swap with front matter: refused too.
        let up = resolve_move_block(doc, &blocks, &els, 16, 16, false);
        assert!(matches!(up, Err(Refusal::Why(_))), "{up:?}");
        // A nested item can't move past its container's edge.
        let doc2 = "- parent\n  - child\n\nAfter.\n";
        let (blocks2, els2) = fixture(doc2);
        // Cursor on "child" (byte 13): down would cross out of the nested list.
        let down = resolve_move_block(doc2, &blocks2, &els2, 13, 13, true);
        assert_eq!(
            down,
            Err(Refusal::Quiet),
            "no sibling below inside the list"
        );
    }

    #[test]
    fn a_point_on_a_container_acts_on_the_container_not_its_first_child() {
        // The reading view focuses the innermost block *containing* the cursor, and a `j`/`k`
        // walk parks the point on a quote's own `>` — inside the quote's span, before its
        // first child's — so the whole quote is lit. The resolution here must agree: the
        // extent heuristics used to hand the point's line to the quote's single-line first
        // paragraph, so `Ctrl-j` shuffled that paragraph around inside the quote.
        let doc =
            "Intro.\n\n> Outer.\n>\n> > Inner one\n> > and two.\n> >\n> > > Deep.\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let at = doc.find("> Outer").unwrap() as u32;
        let (top, bottom) = selection_block_range(doc, &els, at, at).unwrap();
        assert_eq!(top, bottom);
        assert_eq!(
            els[top].span().start,
            at,
            "the quote, not its first paragraph"
        );
        let e = resolve_move_block(doc, &blocks, &els, at, at, true).unwrap();
        assert_eq!(
            apply(doc, &e),
            "Intro.\n\nOutro.\n\n> Outer.\n>\n> > Inner one\n> > and two.\n> >\n> > > Deep.\n",
            "the whole quote moves, children intact"
        );
        // Cut follows the same resolution: the clipboard is the whole quote.
        let (_, clip) = resolve_delete(doc, &blocks, &els, at, at).unwrap();
        assert_eq!(
            clip,
            "> Outer.\n>\n> > Inner one\n> > and two.\n> >\n> > > Deep.\n"
        );
        // An x whole-line *selection* over just the first paragraph's line is still that
        // paragraph: for a real selection the extent is the signal — a container and the
        // child it opens with share a line, and only the extent tells them apart.
        let line_end = at + doc[at as usize..].find('\n').unwrap() as u32;
        let (t, b) = selection_block_range(doc, &els, at, line_end).unwrap();
        assert_eq!((t, b), (top + 1, top + 1), "extent picks the paragraph");
    }

    #[test]
    fn depth_on_a_focused_container_point_unwraps_the_container() {
        let doc = "> Outer.\n>\n> > Inner.\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 0, 0, false).unwrap();
        assert_eq!(apply(doc, &e), "Outer.\n\n> Inner.\n");
    }

    #[test]
    fn a_point_off_every_block_span_still_resolves_to_the_lit_block() {
        // Fence spans stop at the closing backtick, so the terminating newline's byte is
        // outside the span and resolves forward — the client's `focus_byte` steps back onto
        // the last content byte, and the shared resolution must too, or the op would act one
        // block below the bar.
        let doc = "```rust\nfn a() {}\n```\n\nAfter.\n";
        let (_, els) = fixture(doc);
        let nl = doc.find("```\n").unwrap() as u32 + 3;
        let sel = selection_block_range(doc, &els, nl, nl).unwrap();
        assert_eq!(sel, (0, 0), "the fence, not the paragraph after it");
        // A point on a separator blank line falls forward, exactly like the rendered bar.
        let blank = doc.find("\n\n").unwrap() as u32 + 1;
        let sel = selection_block_range(doc, &els, blank, blank).unwrap();
        assert_eq!(sel, (1, 1));
    }

    #[test]
    fn structural_edits_hand_back_the_selection_state_they_were_given() {
        // From a bare reading position the edit leaves one — parked on the block it acted on —
        // instead of manufacturing a selection nobody asked for. From a selection it keeps the
        // selection, so a repeated press keeps acting on the same blocks rather than dropping
        // all but one.
        let (blocks, els) = fixture(DOC);
        // Point in → point out, on the moved block.
        let e = resolve_move_block(DOC, &blocks, &els, 9, 9, true).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(landing(&out, &e), ("Alpha one.\n".into(), false));
        // Selection in (Alpha..Beta, whole-line) → selection out, still over both.
        let e = resolve_move_block(DOC, &blocks, &els, 9, 31, true).unwrap();
        let out = apply(DOC, &e);
        assert!(e.anchor != e.cursor, "selection kept");
        assert_eq!(
            &out[e.anchor..=e.cursor],
            "Alpha one.\n\nBeta two.\n",
            "both moved blocks stay selected"
        );
        // Same rule for paste and depth.
        let e = resolve_paste(DOC, &blocks, &els, 21, 21, "New block.", false).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(landing(&out, &e), ("New block.\n".into(), false));
        let e = resolve_depth(DOC, &blocks, &els, 0, 0, true).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(landing(&out, &e), ("## Title\n".into(), false));
    }

    #[test]
    fn a_moved_code_fence_keeps_the_reading_position_on_itself() {
        // Fenced spans stop at the closing backtick — unlike a paragraph's, they do not include
        // the trailing newline — so a whole-line landing sits one byte past the block and used
        // to resolve forward onto the *next* fence.
        let doc = "Intro.\n\n```rust\nfn a() {}\n```\n\n```py\ndef b(): ...\n```\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let at = doc.find("```rust").unwrap() as u32;
        let e = resolve_move_block(doc, &blocks, &els, at, at, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(
            out,
            "Intro.\n\n```py\ndef b(): ...\n```\n\n```rust\nfn a() {}\n```\n\nOutro.\n"
        );
        assert_eq!(
            landing(&out, &e),
            ("```rust\nfn a() {}\n```".into(), false),
            "the fence that moved, not the one after it"
        );
    }

    #[test]
    fn move_normalizes_an_empty_gap_so_the_block_cannot_be_absorbed() {
        // A list, a quote or a table may *interrupt* a paragraph with no blank line between
        // them — but the adjacency doesn't survive being reversed. Carrying the empty gap
        // across the swap used to leave the paragraph glued under the container, re-parsed as
        // a lazy continuation of the last list item / inside the quote, unmovable from there.
        let doc = "Intro.\n- a\n- b\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 0, 0, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "- a\n- b\n\nIntro.\n\nOutro.\n");
        // Still a top-level sibling afterwards: the move is reversible.
        let (blocks2, els2) = fixture(&out);
        let back = resolve_move_block(&out, &blocks2, &els2, 10, 10, false).unwrap();
        assert_eq!(apply(&out, &back), "Intro.\n\n- a\n- b\n\nOutro.\n");

        let doc = "Intro.\n> quoted\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 0, 0, true).unwrap();
        assert_eq!(apply(doc, &e), "> quoted\n\nIntro.\n\nOutro.\n");
    }

    #[test]
    fn move_paragraph_unit_is_blank_line_delimited() {
        let text = "fn a() {\n    body\n}\n\nfn b() {\n}\n";
        let e = resolve_move_paragraph(text, 0, 0, true).unwrap();
        assert_eq!(apply(text, &e), "fn b() {\n}\n\nfn a() {\n    body\n}\n");
        // From a blank line: quiet.
        assert_eq!(
            resolve_move_paragraph(text, 20, 20, true),
            Err(Refusal::Quiet)
        );
    }

    /// The number an ordered list renders from — its first item's marker. Everything below is
    /// renumbered by the renderer, so this is the only number a structural op can disturb.
    fn list_start(md: &str) -> Option<u64> {
        parse(md).into_iter().find_map(|b| match b {
            Block::List {
                ordered: true,
                start,
                ..
            } => Some(start),
            _ => None,
        })
    }

    #[test]
    fn ordered_markers_stay_with_positions_when_items_move() {
        // A renderer numbers an ordered list from its first item's marker, so a marker carried
        // along with its item restarts the whole list — moving `2.` to the top made the list
        // begin at 2. The markers belong to the positions; the content moves between them.
        const ORD: &str = "1. one\n2. two\n3. three\n";
        let (blocks, els) = fixture(ORD);
        let e = resolve_move_block(ORD, &blocks, &els, 7, 7, false).unwrap();
        let out = apply(ORD, &e);
        assert_eq!(out, "1. two\n2. one\n3. three\n");
        assert_eq!(list_start(&out), Some(1), "the list still starts at 1");
        assert_eq!(landing(&out, &e), ("1. two\n".into(), false));
        let e = resolve_move_block(ORD, &blocks, &els, 0, 0, true).unwrap();
        assert_eq!(apply(ORD, &e), "1. two\n2. one\n3. three\n");

        // Positional, not renumbered-from-1: a list that starts at 3 keeps starting at 3, and a
        // deliberate all-`1.` list stays that way.
        let doc = "3. one\n4. two\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 7, 7, false).unwrap();
        assert_eq!(apply(doc, &e), "3. two\n4. one\n");
        let doc = "1. one\n1. two\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 7, 7, false).unwrap();
        assert_eq!(apply(doc, &e), "1. two\n1. one\n");

        // Markers of different widths swap without knocking the landing off.
        let doc = "9. nine\n10. ten\n11. eleven\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 8, 8, false).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "9. ten\n10. nine\n11. eleven\n");
        assert_eq!(landing(&out, &e), ("9. ten\n".into(), false));

        // Bullets have no numbering to preserve; they just swap.
        let doc = "- a\n- b\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_move_block(doc, &blocks, &els, 4, 4, false).unwrap();
        assert_eq!(apply(doc, &e), "- b\n- a\n");
    }

    #[test]
    fn cutting_an_ordered_lists_head_hands_its_number_to_the_survivor() {
        const ORD: &str = "1. one\n2. two\n3. three\n";
        let (blocks, els) = fixture(ORD);
        let (e, clip) = resolve_delete(ORD, &blocks, &els, 0, 0).unwrap();
        let out = apply(ORD, &e);
        assert_eq!(out, "1. two\n3. three\n");
        assert_eq!(list_start(&out), Some(1), "cutting `1.` didn't restart it");
        // …and the cut item pastes back where it came from, list intact.
        let (blocks2, els2) = fixture(&out);
        let back = resolve_paste(&out, &blocks2, &els2, 0, 0, &clip, false).unwrap();
        assert_eq!(apply(&out, &back), "1. one\n1. two\n3. three\n");
        assert_eq!(list_start(&apply(&out, &back)), Some(1));

        // A multi-item cut from the head hands off once, from the top item.
        let (e, _) = resolve_delete(ORD, &blocks, &els, 0, 13).unwrap();
        assert_eq!(apply(ORD, &e), "1. three\n");
        // Cutting from the middle leaves the head alone, and cutting everything has no survivor.
        let (e, _) = resolve_delete(ORD, &blocks, &els, 7, 7).unwrap();
        assert_eq!(apply(ORD, &e), "1. one\n3. three\n");
        let (e, _) = resolve_delete(ORD, &blocks, &els, 0, 21).unwrap();
        assert_eq!(apply(ORD, &e), "");
    }

    #[test]
    fn delete_takes_the_trailing_blank_run() {
        let (blocks, els) = fixture(DOC);
        let (e, clip) = resolve_delete(DOC, &blocks, &els, 21, 21).unwrap();
        assert_eq!(clip, "Beta two.\n");
        assert_eq!(apply(DOC, &e), "# Title\n\nAlpha one.\n\nGamma three.\n");
    }

    #[test]
    fn delete_of_the_last_block_takes_the_leading_run() {
        let (blocks, els) = fixture(DOC);
        let (e, clip) = resolve_delete(DOC, &blocks, &els, 33, 33).unwrap();
        assert_eq!(clip, "Gamma three.\n");
        assert_eq!(apply(DOC, &e), "# Title\n\nAlpha one.\n\nBeta two.\n");
    }

    #[test]
    fn delete_multi_block_selection() {
        let (blocks, els) = fixture(DOC);
        // Selection spanning Alpha..Beta (bytes 9..30).
        let (e, clip) = resolve_delete(DOC, &blocks, &els, 9, 30).unwrap();
        assert_eq!(clip, "Alpha one.\n\nBeta two.\n");
        assert_eq!(apply(DOC, &e), "# Title\n\nGamma three.\n");
    }

    #[test]
    fn paste_inserts_before_the_focused_block_normalized() {
        let (blocks, els) = fixture(DOC);
        // Ragged clipboard: trailing newlines are normalized away, one blank line each side.
        let e = resolve_paste(DOC, &blocks, &els, 21, 21, "New block.\n\n\n", false).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(
            out,
            "# Title\n\nAlpha one.\n\nNew block.\n\nBeta two.\n\nGamma three.\n"
        );
        assert_eq!(landing(&out, &e), ("New block.\n".into(), false));
    }

    #[test]
    fn paste_replace_swaps_the_selected_block_in_place() {
        let (blocks, els) = fixture(DOC);
        let e = resolve_paste(DOC, &blocks, &els, 21, 21, "Delta.", true).unwrap();
        assert_eq!(
            apply(DOC, &e),
            "# Title\n\nAlpha one.\n\nDelta.\n\nGamma three.\n"
        );
        // Replacing the LAST block: separator re-added before, not after.
        let e = resolve_paste(DOC, &blocks, &els, 33, 33, "Delta.", true).unwrap();
        assert_eq!(
            apply(DOC, &e),
            "# Title\n\nAlpha one.\n\nBeta two.\n\nDelta.\n"
        );
    }

    #[test]
    fn cut_then_paste_round_trips_inside_a_tight_list() {
        // §12.1's "cut→paste round-trips by construction" has to hold where the blocks are
        // newline-adjacent too: a hard-coded blank separator would split the list in two and
        // turn it loose.
        let doc = "- one\n- two\n- three\n";
        let (blocks, els) = fixture(doc);
        let (cut, clip) = resolve_delete(doc, &blocks, &els, 8, 8).unwrap();
        let after = apply(doc, &cut);
        assert_eq!(
            (after.as_str(), clip.as_str()),
            ("- one\n- three\n", "- two\n")
        );
        let (blocks2, els2) = fixture(&after);
        let back = resolve_paste(
            &after,
            &blocks2,
            &els2,
            cut.cursor as u32,
            cut.cursor as u32,
            &clip,
            false,
        )
        .unwrap();
        assert_eq!(apply(&after, &back), doc, "round-trips exactly");
        // Replace-in-place keeps the list tight as well.
        let e = resolve_paste(doc, &blocks, &els, 8, 8, "- TWO", true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n- TWO\n- three\n");
    }

    #[test]
    fn structural_ops_refuse_front_matter() {
        // Front matter is positional — it is only front matter while it opens the file — so
        // every op that would relocate it, replace it, or push a block above it refuses (§12.1).
        let doc = "---\nkey: v\n---\n\nBody.\n";
        let (blocks, els) = fixture(doc);
        for op in [
            resolve_delete(doc, &blocks, &els, 0, 0).map(|(e, _)| e),
            resolve_paste(doc, &blocks, &els, 0, 0, "Injected.", false),
            resolve_paste(doc, &blocks, &els, 0, 0, "Injected.", true),
            resolve_move_block(doc, &blocks, &els, 0, 0, true),
        ] {
            assert!(matches!(op, Err(Refusal::Why(_))), "{op:?}");
        }
        // The body below it is ordinary: only the front matter itself is protected.
        assert!(resolve_delete(doc, &blocks, &els, 16, 16).is_ok());
    }

    #[test]
    fn paste_into_an_empty_document() {
        let (blocks, els) = fixture("");
        let e = resolve_paste("", &blocks, &els, 0, 0, "Only block.", false).unwrap();
        assert_eq!(apply("", &e), "Only block.\n");
    }

    #[test]
    fn depth_changes_atx_heading_levels_with_ladder_ends() {
        let (blocks, els) = fixture(DOC);
        let e = resolve_depth(DOC, &blocks, &els, 0, 0, true).unwrap();
        assert_eq!(apply(DOC, &e), "#".to_string() + DOC);
        // H1 can't promote.
        assert_eq!(
            resolve_depth(DOC, &blocks, &els, 0, 0, false),
            Err(Refusal::Quiet)
        );
        // H6 can't demote.
        let six = "###### Deep\n\nBody.\n";
        let (blocks6, els6) = fixture(six);
        assert_eq!(
            resolve_depth(six, &blocks6, &els6, 0, 0, true),
            Err(Refusal::Quiet)
        );
        // Everything else deepens by a blockquote level instead (see the quote tests).
        let e = resolve_depth(DOC, &blocks, &els, 9, 9, true).unwrap();
        assert_eq!(
            apply(DOC, &e),
            "# Title\n\n> Alpha one.\n\nBeta two.\n\nGamma three.\n"
        );
    }

    #[test]
    fn depth_wraps_and_unwraps_blockquotes() {
        // A quote is a *container* in the block tree, like a list item — so wrapping is a real
        // step down, and it repeats: nested quotes are legal, and since the container is the
        // reading grain the wrapped result is what the next press acts on.
        let (blocks, els) = fixture(DOC);
        let e = resolve_depth(DOC, &blocks, &els, 9, 9, true).unwrap();
        let once = apply(DOC, &e);
        assert_eq!(&once[9..22], "> Alpha one.\n");
        assert_eq!(landing(&once, &e), ("> Alpha one.\n".into(), false));
        let (b2, e2) = fixture(&once);
        let twice = apply(&once, &resolve_depth(&once, &b2, &e2, 9, 9, true).unwrap());
        assert_eq!(&twice[9..24], "> > Alpha one.\n");
        // …and `Ctrl-h` unwinds it one level at a time, back to a bare paragraph.
        let (b3, e3) = fixture(&twice);
        let back = apply(
            &twice,
            &resolve_depth(&twice, &b3, &e3, 9, 9, false).unwrap(),
        );
        assert_eq!(back, once);
        let (b4, e4) = fixture(&back);
        let flat = apply(&back, &resolve_depth(&back, &b4, &e4, 9, 9, false).unwrap());
        assert_eq!(flat, DOC, "round trips exactly");
        // Already flat: quiet, like `Ctrl-h` on an H1.
        let (b5, e5) = fixture(DOC);
        assert_eq!(
            resolve_depth(DOC, &b5, &e5, 9, 9, false),
            Err(Refusal::Quiet)
        );
    }

    #[test]
    fn quote_depth_lands_on_the_block_it_acted_on() {
        // The line start is the `>` itself, which belongs to the *enclosing* quote's span and
        // not to the block being wrapped — parking there lifted the focus to the container.
        let doc = "> Para one.\n>\n> Para two.\n";
        let (blocks, els) = fixture(doc);
        let two = doc.find("Para two").unwrap() as u32;
        let e = resolve_depth(doc, &blocks, &els, two, two, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "> Para one.\n>\n> > Para two.\n");
        assert_eq!(
            landing(&out, &e),
            ("> Para two.\n".into(), false),
            "the inner quote just created, not the outer one"
        );
        // Peeling it back off lands on the paragraph, not on the quote around it.
        let (b2, e2) = fixture(&out);
        let two = out.find("Para two").unwrap() as u32;
        let back = resolve_depth(&out, &b2, &e2, two, two, false).unwrap();
        let flat = apply(&out, &back);
        assert_eq!(flat, doc);
        assert_eq!(landing(&flat, &back), ("Para two.\n".into(), false));
        // A quoted list item keeps its own focus through a nesting change too.
        let doc = "> - a\n> - b\n";
        let (blocks, els) = fixture(doc);
        let b = doc.find("- b").unwrap() as u32 + 2;
        let e = resolve_depth(doc, &blocks, &els, b, b, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(landing(&out, &e), ("- b\n".into(), false));
    }

    #[test]
    fn quoting_a_run_of_blocks_marks_the_separators_too() {
        // A blank line *without* the marker ends the quote, so an unmarked separator would split
        // one quote in two and drop everything after it back out.
        let (blocks, els) = fixture(DOC);
        let e = resolve_depth(DOC, &blocks, &els, 9, 31, true).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(
            out,
            "# Title\n\n> Alpha one.\n>\n> Beta two.\n\nGamma three.\n"
        );
        // One quote holding both paragraphs, not two quotes.
        assert_eq!(
            crate::parse(&out)
                .iter()
                .filter(|b| matches!(b, Block::Quote { .. }))
                .count(),
            1
        );
        // A selection stays a selection (§12.5), and unwrapping restores the original.
        assert!(e.anchor != e.cursor);
        let (b2, e2) = fixture(&out);
        let back = resolve_depth(&out, &b2, &e2, e.anchor as u32, e.cursor as u32, false).unwrap();
        assert_eq!(apply(&out, &back), DOC);
    }

    #[test]
    fn ops_measure_geometry_from_after_the_container_prefix() {
        // Inside a quote a block's line starts at `> `, not at column 0. Measuring from column 0
        // meant the ordered markers sat behind a `>` and went unrecognised (no renumber on move,
        // no head handoff on cut), and nesting inserted its padding *in front of* the marker,
        // tearing the quote apart.
        let doc = "> 1. one\n> 2. two\n> 3. three\n";
        let (blocks, els) = fixture(doc);
        let two = doc.find("two").unwrap() as u32;
        let e = resolve_move_block(doc, &blocks, &els, two, two, false).unwrap();
        assert_eq!(apply(doc, &e), "> 1. two\n> 2. one\n> 3. three\n");
        let e = resolve_depth(doc, &blocks, &els, two, two, true).unwrap();
        assert_eq!(apply(doc, &e), "> 1. one\n>    1. two\n> 3. three\n");
        let one = doc.find("one").unwrap() as u32;
        let (e, _) = resolve_delete(doc, &blocks, &els, one, one).unwrap();
        assert_eq!(apply(doc, &e), "> 1. two\n> 3. three\n");

        // Bullets nest after the marker too, and un-nesting comes straight back.
        let doc = "> - a\n> - b\n";
        let (blocks, els) = fixture(doc);
        let b = doc.find("- b").unwrap() as u32 + 2;
        let e = resolve_depth(doc, &blocks, &els, b, b, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "> - a\n>   - b\n");
        let (b2, e2) = fixture(&out);
        let at = out.rfind('b').unwrap() as u32;
        let back = resolve_depth(&out, &b2, &e2, at, at, false).unwrap();
        assert_eq!(apply(&out, &back), doc);

        // Two levels deep, so the prefix run (`> > `) is exercised, not just one marker.
        let doc = "> > 1. one\n> > 2. two\n";
        let (blocks, els) = fixture(doc);
        let two = doc.find("two").unwrap() as u32;
        let e = resolve_move_block(doc, &blocks, &els, two, two, false).unwrap();
        assert_eq!(apply(doc, &e), "> > 1. two\n> > 2. one\n");
    }

    #[test]
    fn ops_reach_inside_containers_now_that_children_are_stops() {
        // §12.6: a container's children are elements, so the innermost-first focus rule lands on
        // them and every op acts on the inner block rather than the whole container.
        let doc = "> Para one.\n>\n> Para two.\n";
        let (blocks, els) = fixture(doc);
        let two = doc.find("Para two").unwrap() as u32;
        // Move a paragraph *within* its quote — the `>` prefix travels with the lines.
        let e = resolve_move_block(doc, &blocks, &els, two, two, false).unwrap();
        assert_eq!(apply(doc, &e), "> Para two.\n>\n> Para one.\n");
        // Deepen just that paragraph, leaving its sibling at the outer level.
        let e = resolve_depth(doc, &blocks, &els, two, two, true).unwrap();
        assert_eq!(apply(doc, &e), "> Para one.\n>\n> > Para two.\n");
        // Cut a list item's *second* paragraph without disturbing the first.
        let doc = "- First para.\n\n  Second para.\n\n- Next item.\n";
        let (blocks, els) = fixture(doc);
        let at = doc.find("Second").unwrap() as u32;
        let (e, clip) = resolve_delete(doc, &blocks, &els, at, at).unwrap();
        assert_eq!(apply(doc, &e), "- First para.\n\n- Next item.\n");
        assert_eq!(clip, "  Second para.\n");
    }

    #[test]
    fn quote_depth_refuses_front_matter_and_multi_item_runs() {
        let doc = "---\nkey: v\n---\n\nBody.\n";
        let (blocks, els) = fixture(doc);
        assert!(matches!(
            resolve_depth(doc, &blocks, &els, 0, 0, true),
            Err(Refusal::Why(_))
        ));
        // A run of list items reads as "nest them all", which isn't built — say so rather than
        // silently quoting them instead.
        let doc = "- a\n- b\n- c\n";
        let (blocks, els) = fixture(doc);
        assert_eq!(
            resolve_depth(doc, &blocks, &els, 0, 7, true),
            Err(Refusal::Why("Nest one list item at a time"))
        );
    }

    #[test]
    fn quoting_carries_a_blocks_own_shape_through() {
        // Fences, tables and lists all survive a wrap: every line takes the marker, so the block
        // keeps its structure one level in.
        let doc = "```rust\nfn a() {}\n\nfn b() {}\n```\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 0, 0, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "> ```rust\n> fn a() {}\n>\n> fn b() {}\n> ```\n");
        let inner = crate::parse(&out);
        assert!(
            matches!(inner.first(), Some(Block::Quote { content, .. })
                if matches!(content.first(), Some(Block::Code { .. }))),
            "still one fence, inside the quote: {inner:?}"
        );
        let (b2, e2) = fixture(&out);
        let back = resolve_depth(&out, &b2, &e2, 2, 2, false).unwrap();
        assert_eq!(apply(&out, &back), doc);
    }

    #[test]
    fn depth_nests_an_item_under_its_previous_sibling() {
        let doc = "- one\n- two\n";
        let (blocks, els) = fixture(doc);
        // "two" (byte 8) nests under "one": indented by one's marker width (2).
        let e = resolve_depth(doc, &blocks, &els, 8, 8, true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - two\n");
        // The first item has nothing to nest under.
        assert_eq!(
            resolve_depth(doc, &blocks, &els, 2, 2, true),
            Err(Refusal::Quiet)
        );
    }

    #[test]
    fn nesting_an_ordered_item_starts_its_new_sublist_at_one() {
        // CommonMark: an ordered list interrupts a paragraph only if it starts with 1. Nesting
        // `2.` straight under its parent's text made it a lazy continuation of that text, and
        // the reader drew the two items merged into one line.
        for (doc, at, want) in [
            ("1. one\n2. two\n", 7, "1. one\n   1. two\n"),
            ("1) one\n2) two\n", 7, "1) one\n   1) two\n"),
            ("7. one\n8. two\n", 7, "7. one\n   1. two\n"),
        ] {
            let (blocks, els) = fixture(doc);
            let e = resolve_depth(doc, &blocks, &els, at, at, true).unwrap();
            let out = apply(doc, &e);
            assert_eq!(out, want);
            // Two blocks in the reader — the parent item and its nested list — not one.
            let nested = crate::parse(&out);
            assert_eq!(
                crate::to_plain(&nested).lines().count(),
                3,
                "merged into one item: {out:?}"
            );
        }
        // Joining a sublist that already exists at that indent follows a list item, not a
        // paragraph, so the number stands (the renderer counts from the sublist's first marker).
        let doc = "1. a\n   1. x\n2. b\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 13, 13, true).unwrap();
        assert_eq!(apply(doc, &e), "1. a\n   1. x\n   2. b\n");
        // Bullets have no such rule and are untouched.
        let doc = "- one\n- two\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 6, 6, true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - two\n");
    }

    #[test]
    fn depth_respects_ordered_marker_width_and_unnests() {
        let doc = "1. one\n2. two\n";
        let (blocks, els) = fixture(doc);
        // Ordered marker "1. " is 3 wide — and the new sublist restarts at 1, or it would be a
        // lazy continuation of "one" instead of a nested list (see the test above).
        let e = resolve_depth(doc, &blocks, &els, 10, 10, true).unwrap();
        assert_eq!(apply(doc, &e), "1. one\n   1. two\n");
        // And back out: the nested item un-nests to its parent's level.
        let nested = "- one\n  - two\n";
        let (blocks2, els2) = fixture(nested);
        let e = resolve_depth(nested, &blocks2, &els2, 10, 10, false).unwrap();
        assert_eq!(apply(nested, &e), "- one\n- two\n");
        // A top-level item can't go shallower.
        assert_eq!(
            resolve_depth(nested, &blocks2, &els2, 2, 2, false),
            Err(Refusal::Quiet)
        );
    }

    #[test]
    fn depth_lands_on_the_changed_item_not_its_parent() {
        // A nested item's span starts at its *marker*, so the indent in front of it belongs to
        // the parent's span. Landing on the line start therefore focused the parent — and the
        // next press acted on the parent instead of the item just indented.
        let doc = "- one\n- two\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 8, 8, true).unwrap();
        let out = apply(doc, &e);
        assert_eq!(out, "- one\n  - two\n");
        assert_eq!(
            landing(&out, &e),
            // The item's span starts at its marker; the indent belongs to the parent.
            ("- two\n".into(), false),
            "the indented item, and no selection conjured from a bare position"
        );
        let (blocks2, els2) = fixture(&out);
        let (top, bottom) =
            selection_block_range(&out, &els2, e.anchor as u32, e.cursor as u32).unwrap();
        assert_eq!((top, bottom), (1, 1), "one block, the nested item");
        // So Ctrl-h ping-pongs it straight back rather than un-nesting something else.
        let back = resolve_depth(
            &out,
            &blocks2,
            &els2,
            e.anchor as u32,
            e.cursor as u32,
            false,
        )
        .unwrap();
        assert_eq!(apply(&out, &back), doc);
        // Headings land selected too, whole-line.
        let (blocks, els) = fixture(DOC);
        let e = resolve_depth(DOC, &blocks, &els, 0, 0, true).unwrap();
        let out = apply(DOC, &e);
        assert_eq!(landing(&out, &e), ("## Title\n".into(), false));
    }

    #[test]
    fn selection_edges_resolve_past_indentation_to_the_nested_item() {
        // What `x` produces for a nested item is whole-line, so its top edge sits on the
        // indent. That must resolve *forward* to the item the whitespace introduces — the
        // intra-line twin of the separator-line rule — or every depth change on a nested item
        // is refused as spanning two blocks.
        let doc = "- one\n  - two\n    - three\n";
        let (blocks, els) = fixture(doc);
        assert_eq!(selection_block_range(doc, &els, 6, 13), Some((1, 1)));
        assert_eq!(selection_block_range(doc, &els, 14, 25), Some((2, 2)));
        assert!(resolve_depth(doc, &blocks, &els, 14, 25, false).is_ok());
    }

    #[test]
    fn an_only_child_item_has_no_sibling_to_nest_under_or_swap_with() {
        // A single-item list has the same span as its item, so resolving the item against the
        // enclosing block sequence handed back `[paragraph, list]` as its "siblings" — making
        // the item's neighbour its own parent's text. Indent then nested it under the parent
        // (indent 4 under a 2-space parent: applied to the source, invisible in the reader, and
        // it ran away further on every press) and moving up sliced a reversed range.
        let doc = "- one\n  - two\n";
        let (blocks, els) = fixture(doc);
        assert_eq!(
            resolve_depth(doc, &blocks, &els, 8, 8, true),
            Err(Refusal::Quiet),
            "nothing above it inside the list to nest under"
        );
        for down in [true, false] {
            assert_eq!(
                resolve_move_block(doc, &blocks, &els, 8, 8, down),
                Err(Refusal::Quiet),
                "no sibling to swap with (down={down})"
            );
        }
        // With a real sibling present, both work and step exactly one level.
        let doc = "- one\n  - a\n  - b\n";
        let (blocks, els) = fixture(doc);
        let e = resolve_depth(doc, &blocks, &els, 14, 14, true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - a\n    - b\n");
        let e = resolve_move_block(doc, &blocks, &els, 14, 14, false).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - b\n  - a\n");
    }

    #[test]
    fn indent_never_exceeds_one_level_below_the_previous_sibling() {
        // Whatever the marker, the nested item lines up just past the sibling it nests under —
        // never deeper, which markdown would render as no change at all (or as code).
        // `becomes_only_child`: whether the item lands as the sole item of a new sublist, in
        // which case a second press has nothing left to nest under and must refuse. Where it
        // lands beside an existing sibling it may legitimately keep going, one level per press.
        for (doc, at, want, becomes_only_child) in [
            ("- a\n- b\n", 4, "- a\n  - b\n", true),
            ("* a\n* b\n", 4, "* a\n  * b\n", true),
            ("1. a\n2. b\n", 5, "1. a\n   1. b\n", true),
            ("- a\n  - x\n- b\n", 11, "- a\n  - x\n  - b\n", false),
        ] {
            let (blocks, els) = fixture(doc);
            let e = resolve_depth(doc, &blocks, &els, at, at, true).unwrap();
            let out = apply(doc, &e);
            assert_eq!(out, want, "{doc:?}");
            let (blocks2, els2) = fixture(&out);
            // The moved item's content char, whatever marker precedes it.
            let again = out.rfind('b').unwrap() as u32;
            assert_eq!(
                resolve_depth(&out, &blocks2, &els2, again, again, true).is_err(),
                becomes_only_child,
                "second press on {out:?}"
            );
        }
    }

    #[test]
    fn deeply_nested_items_are_locatable_at_all() {
        // Three levels down, the tree walk used to give up on the item's own paragraph (which
        // shares the item's span in a tight list) before reaching the nested list beside it, so
        // `locate` returned `None` and every structural op on the item refused silently.
        let doc = "- one\n  - two\n    - three\n    - four\n";
        let (blocks, els) = fixture(doc);
        // Un-nest the third level back to the second.
        let e = resolve_depth(doc, &blocks, &els, 18, 18, false).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - two\n  - three\n    - four\n");
        // And the third-level items are siblings that can be reordered.
        let e = resolve_move_block(doc, &blocks, &els, 18, 18, true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - two\n    - four\n    - three\n");
    }

    #[test]
    fn depth_moves_an_items_subtree_with_it() {
        let doc = "- one\n- two\n  - child\n";
        let (blocks, els) = fixture(doc);
        // Nesting "two" carries its child along, preserving relative depth.
        let e = resolve_depth(doc, &blocks, &els, 8, 8, true).unwrap();
        assert_eq!(apply(doc, &e), "- one\n  - two\n    - child\n");
    }

    #[test]
    fn toggle_task_flips_both_ways_and_stays_put() {
        let doc = "- [ ] open\n- [x] done\n";
        let (_, els) = fixture(doc);
        let e = resolve_toggle_task(doc, &els, 6, None).unwrap();
        assert_eq!(apply(doc, &e), "- [x] open\n- [x] done\n");
        assert_eq!((e.anchor, e.cursor), (6, 6), "cursor stays");
        let e = resolve_toggle_task(doc, &els, 17, None).unwrap();
        assert_eq!(apply(doc, &e), "- [ ] open\n- [ ] done\n");
        // A plain paragraph is not a task.
        let (_, els2) = fixture(DOC);
        assert_eq!(
            resolve_toggle_task(DOC, &els2, 9, None),
            Err(Refusal::Quiet)
        );
    }

    /// Loose task items toggle too — including from the cursor sitting in a *second* paragraph,
    /// which resolves to the enclosing item. Both used to refuse quietly: the parse dropped
    /// `checked` on loose items, so no `Element::Item { checked: Some(_) }` existed to target.
    #[test]
    fn toggle_task_works_on_loose_items() {
        let doc = "- [ ] open\n\n- [x] done\n";
        let (_, els) = fixture(doc);
        let e = resolve_toggle_task(doc, &els, 6, None).unwrap();
        assert_eq!(apply(doc, &e), "- [x] open\n\n- [x] done\n");
        let doc = "- [ ] open\n\n  A second paragraph.\n";
        let (_, els) = fixture(doc);
        let e = resolve_toggle_task(doc, &els, 20, None).unwrap();
        assert_eq!(
            apply(doc, &e),
            "- [x] open\n\n  A second paragraph.\n",
            "the box flips, not the paragraph the cursor is in"
        );
    }

    #[test]
    fn toggle_task_reads_the_box_at_the_marker_not_the_body() {
        // The box is found at the marker, not searched for: a *checked* item whose text
        // mentions `[ ]` would otherwise have its body rewritten and its box left alone.
        let doc = "- [x] handle the [ ] case\n";
        let (_, els) = fixture(doc);
        let e = resolve_toggle_task(doc, &els, 6, None).unwrap();
        assert_eq!(apply(doc, &e), "- [ ] handle the [ ] case\n");
        // Ordered and nested markers resolve the same way.
        let doc = "1. [ ] one\n\n- outer\n  - [x] inner [ ] literal\n";
        let (_, els) = fixture(doc);
        let e = resolve_toggle_task(doc, &els, 7, None).unwrap();
        assert_eq!(
            apply(doc, &e),
            "1. [x] one\n\n- outer\n  - [x] inner [ ] literal\n"
        );
        let e = resolve_toggle_task(doc, &els, 30, None).unwrap();
        assert_eq!(
            apply(doc, &e),
            "1. [ ] one\n\n- outer\n  - [ ] inner [ ] literal\n"
        );
    }

    #[test]
    fn toggle_task_refuses_from_neighbouring_blocks() {
        // `Ctrl-a` sends unconditionally, so the resolver is the only gate: a cursor not
        // inside any task item refuses rather than falling forward/back to a neighbour's box
        // nothing on screen marks (from "Outro." it used to reach back to the list's last
        // item; from "Intro." forward to its first).
        let doc = "Intro.\n\n- [ ] open\n\nOutro.\n";
        let (_, els) = fixture(doc);
        assert_eq!(
            resolve_toggle_task(doc, &els, 0, Some(true)),
            Err(Refusal::Quiet),
            "block before the list"
        );
        let outro = doc.find("Outro").unwrap() as u32;
        assert_eq!(
            resolve_toggle_task(doc, &els, outro, Some(true)),
            Err(Refusal::Quiet),
            "block after the list"
        );
        // The separator blank lines refuse too — including the one *after* the item, which
        // the loose item's parse span swallows (separators are gaps, §12.1, so the containing
        // walk must not reach the item from there).
        assert_eq!(resolve_toggle_task(doc, &els, 7, None), Err(Refusal::Quiet));
        assert_eq!(
            resolve_toggle_task(doc, &els, 19, None),
            Err(Refusal::Quiet)
        );
        // A whole-line-selected item parks the cursor on its terminating newline: still the
        // item's own box, via the step-back.
        let nl = doc.find("open\n").unwrap() as u32 + 4;
        let e = resolve_toggle_task(doc, &els, nl, None).unwrap();
        assert_eq!(apply(doc, &e), "Intro.\n\n- [x] open\n\nOutro.\n");
    }

    #[test]
    fn toggle_task_can_be_asked_for_a_state_rather_than_a_flip() {
        // `Ctrl-a`/`Ctrl-Alt-a` name the state they want, so the pair carries the same up/down
        // sense over a checkbox that it does over a number. Asking for the state a box is already
        // in is a quiet no-op: holding the key down a list settles it instead of flapping it.
        let doc = "- [ ] open\n- [x] done\n";
        let (_, els) = fixture(doc);
        let (open_at, done_at) = (6, 17);
        let e = resolve_toggle_task(doc, &els, open_at, Some(true)).unwrap();
        assert_eq!(apply(doc, &e), "- [x] open\n- [x] done\n");
        assert_eq!(
            resolve_toggle_task(doc, &els, done_at, Some(true)),
            Err(Refusal::Quiet),
            "already checked"
        );
        let e = resolve_toggle_task(doc, &els, done_at, Some(false)).unwrap();
        assert_eq!(apply(doc, &e), "- [ ] open\n- [ ] done\n");
        assert_eq!(
            resolve_toggle_task(doc, &els, open_at, Some(false)),
            Err(Refusal::Quiet),
            "already unchecked"
        );
        // `None` still flips, which is what Enter sends.
        let e = resolve_toggle_task(doc, &els, open_at, None).unwrap();
        assert_eq!(apply(doc, &e), "- [x] open\n- [x] done\n");
    }

    #[test]
    fn single_block_document_edges() {
        let doc = "Only.\n";
        let (blocks, els) = fixture(doc);
        assert_eq!(
            resolve_move_block(doc, &blocks, &els, 0, 0, true),
            Err(Refusal::Quiet)
        );
        let (e, clip) = resolve_delete(doc, &blocks, &els, 0, 0).unwrap();
        assert_eq!(clip, "Only.\n");
        assert_eq!(apply(doc, &e), "");
    }

    #[test]
    fn a_selected_container_resolves_to_itself_not_its_first_child() {
        // `x` on a focused quote and `x` on that quote's first paragraph are line-identical at
        // the top edge, so the extent has to decide. Resolving both inward to the child left the
        // quote unmovable — and quietly, since a child with no sibling refuses without a toast.
        let doc = "Intro para.\n\n> Quoted one.\n>\n> Quoted two.\n\nOutro.\n";
        let (blocks, els) = fixture(doc);
        let quote = doc.find('>').unwrap() as u32;
        let quote_end = doc.find("\n\nOutro").unwrap() as u32;
        assert_eq!(
            selection_block_range(doc, &els, quote, quote_end),
            {
                let i = els
                    .iter()
                    .position(|e| e.span().start == quote)
                    .expect("the quote is an element");
                Some((i, i))
            },
            "the whole quote, not the paragraphs inside it"
        );
        let e = resolve_move_block(doc, &blocks, &els, quote, quote_end, true).unwrap();
        assert_eq!(
            apply(doc, &e),
            "Intro para.\n\nOutro.\n\n> Quoted one.\n>\n> Quoted two.\n"
        );
        // Selecting only the quote's *first* paragraph still resolves to that paragraph.
        let one_end = doc.find(">\n").unwrap() as u32 - 1;
        let (top, bottom) = selection_block_range(doc, &els, quote, one_end).unwrap();
        assert_eq!(top, bottom);
        assert_eq!(
            &doc[els[top].span().start as usize..els[top].span().end as usize],
            "Quoted one.\n"
        );
    }

    #[test]
    fn every_op_survives_a_multi_byte_final_character() {
        // A document ending without a trailing newline puts the last block's span end *on* its final
        // character. The `end - 1` step onto the last line then lands inside that character when
        // it is multi-byte, and every slice from there used to panic — inside the server.
        let doc = "# Title\n\nAlpha one.\n\nCafé";
        let (blocks, els) = fixture(doc);
        let at = doc.find("Café").unwrap() as u32;
        let (e, clip) = resolve_delete(doc, &blocks, &els, at, at).unwrap();
        assert_eq!(clip, "Café");
        assert_eq!(apply(doc, &e), "# Title\n\nAlpha one.\n");
        let e = resolve_move_block(doc, &blocks, &els, at, at, false).unwrap();
        assert_eq!(apply(doc, &e), "# Title\n\nCafé\n\nAlpha one.\n");
        let e = resolve_paste(doc, &blocks, &els, at, at, "New.", true).unwrap();
        assert_eq!(apply(doc, &e), "# Title\n\nAlpha one.\n\nNew.\n");
        let e = resolve_depth(doc, &blocks, &els, 0, 0, true).unwrap();
        assert_eq!(apply(doc, &e), "## Title\n\nAlpha one.\n\nCafé");
        // A cursor byte landing mid-character resolves instead of panicking (the client's parse
        // can be a revision behind the cursor it is handed).
        let mid = doc.len() as u32 - 1;
        assert!(selection_block_range(doc, &els, mid, mid).is_some());
    }

    #[test]
    fn paste_into_a_document_the_parse_finds_no_blocks_in_keeps_its_text() {
        // Link reference definitions are real content that yields no elements. Taking the
        // empty-document branch here replaced the whole file with the clipboard.
        let doc = "[a]: https://example.com\n";
        let (blocks, els) = fixture(doc);
        assert!(
            selection_block_range(doc, &els, 0, 0).is_none(),
            "no blocks"
        );
        let e = resolve_paste(doc, &blocks, &els, 0, 0, "New block.", false).unwrap();
        assert_eq!(apply(doc, &e), "[a]: https://example.com\n\nNew block.\n");
        // The genuinely empty document still takes the paste as its whole content.
        let (blocks, els) = fixture("");
        let e = resolve_paste("", &blocks, &els, 0, 0, "New block.", false).unwrap();
        assert_eq!(apply("", &e), "New block.\n");
    }

    #[test]
    fn paragraph_move_refuses_front_matter_like_the_block_move_does() {
        // `Ctrl-Alt-j` resolves without a parse, so it needs its own fence check: front matter is
        // only front matter while it opens the file.
        let doc = "---\nkey: v\n---\n\nBody.\n";
        let down = resolve_move_paragraph(doc, 0, 0, true);
        assert!(matches!(down, Err(Refusal::Why(_))), "{down:?}");
        let at = doc.find("Body.").unwrap() as u32;
        let up = resolve_move_paragraph(doc, at, at, false);
        assert!(matches!(up, Err(Refusal::Why(_))), "{up:?}");
        // Below the front matter the move works normally.
        let doc = "---\nkey: v\n---\n\nBody.\n\nMore.\n";
        let at = doc.find("Body.").unwrap() as u32;
        let e = resolve_move_paragraph(doc, at, at, true).unwrap();
        assert_eq!(apply(doc, &e), "---\nkey: v\n---\n\nMore.\n\nBody.\n");
    }
}
