//! The jumplist (docs/jumplist.md): a per-client snapshot of one picker's
//! filtered results, stepped from Normal mode with `]` / `[` (`jumplist/step`), stopping (not
//! wrapping) at the ends. Lives in `ServerState.jumplist` next to the nav history; replaced by
//! the next `jumplist/capture`, wiped on workspace switch and disconnect.
//!
//! Entries are deliberately flat (quickfix-style): one presentation-neutral `display` string, a
//! jump target mirroring what selecting the row in the source picker would do, and the source
//! picker's group header for status display / the Jumplist picker. Positions are snapshot
//! coordinates — edits after capture make them stale, and that's accepted the same way the grep
//! picker's persisted hits accept it (jumps clamp on `buffer/open`).

use crate::picker::{PickerCandidates, PickerState};
use aether_protocol::cursor::Direction;
use aether_protocol::picker::{GroupHeader, PickerKind};
use aether_protocol::LogicalPosition;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use std::collections::HashMap;

/// One client's captured list. `source`/`query` describe where it came from (status display,
/// the Jumplist picker's context); `entries` are in normalized order (see [`normalize`]).
pub struct Jumplist {
    pub source: PickerKind,
    pub query: String,
    pub entries: Vec<JumplistEntry>,
}

/// One captured result. `position`/`anchor` mirror `PickerSelectResult::FileAt` for the source
/// row exactly: `position` is where the cursor lands (a span's inclusive end), `anchor` the
/// selection's other end (the span start) or `None` for a point landing.
#[derive(Clone, Debug, PartialEq)]
pub struct JumplistEntry {
    /// Workspace root index + root-relative path when the file is inside a root; `None` for
    /// out-of-root files (references into dependencies). Lets the open route root-relative
    /// when possible (matching how the source pickers open).
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// Absolute canonical path — always present, the file identity used for stepping.
    pub abs_path: String,
    pub position: LogicalPosition,
    pub anchor: Option<LogicalPosition>,
    /// The source picker's group header for this row (file or section label). `None` only
    /// while a capture is being assembled — the ungrouped source kinds (buffer diagnostics,
    /// document symbols, single-file changes) leave it empty and [`assign_file_groups`] fills
    /// it in. Every *stored* entry has one: the Jumplist picker is collapsible, and its row
    /// space requires every row keyed (docs/picker-groups.md).
    pub group: Option<GroupHeader>,
    /// The source row's text — the Jumplist picker's row + fuzzy haystack.
    pub display: String,
}

impl JumplistEntry {
    /// The entry's spatial start — the span's first char for anchored entries, else the landing
    /// point. Stepping compares cursor edges against this (an entry is "after" the cursor when
    /// its start is).
    pub fn start(&self) -> LogicalPosition {
        match self.anchor {
            Some(a) if (a.line, a.col) < (self.position.line, self.position.col) => a,
            _ => self.position,
        }
    }
}

/// Snapshot `picker`'s current filtered, ranked results into a list. Returns `None` when the
/// picker has nothing captured-worthy (empty filtered set, or a kind that doesn't capture) —
/// callers leave any previously captured list untouched in that case. The second element of the
/// pair is each entry's source *candidate* index, so the capture handler can locate the
/// client's highlighted item (which identifies by candidate) in the normalized list.
///
/// The snapshot honours exactly what the picker shows: the ranked (query- and chip-filtered)
/// set, with two adjustments. DocumentSymbols drops ancestor context rows (rows `rerank` pulled
/// in without them matching the query — capturing "what matched", not the connective tissue).
/// And the order is normalized via [`normalize`] so stepping is spatial regardless of the
/// source ranking.
pub fn capture(picker: &PickerState, matcher: &mut Matcher) -> Option<(Jumplist, Vec<u32>)> {
    let entries = match &picker.candidates {
        PickerCandidates::Grep(v) => ranked_entries(picker, |ci| {
            let c = &v[ci];
            let start = LogicalPosition {
                line: c.line,
                col: c.col,
            };
            let last_col = grep_match_last_char_col(&c.preview, c.col, c.match_byte_len);
            let position = LogicalPosition {
                line: c.line,
                col: last_col,
            };
            JumplistEntry {
                path_index: Some(c.path_index),
                relative_path: Some(c.relative_path.clone()),
                abs_path: c.abs_path.clone(),
                position,
                anchor: (position != start).then_some(start),
                group: Some(GroupHeader::File {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                }),
                display: c.preview.clone(),
            }
        }),
        PickerCandidates::Diagnostics(v) => {
            // The buffer-scoped picker renders flat and leaves the path parts empty (it opens the
            // current buffer, not a path); the workspace one carries real parts. Treat an empty
            // relative path as *absent* rather than `Some("")` — a bogus relative path would make
            // the jumplist open resolve to `root + ""` (a directory). `assign_file_groups` then
            // derives the parts + file header from `abs_path`.
            ranked_entries(picker, |ci| {
                let c = &v[ci];
                let (path_index, relative_path) = relative_parts(c.path_index, &c.relative_path);
                JumplistEntry {
                    path_index,
                    relative_path,
                    abs_path: c.abs_path.clone(),
                    position: LogicalPosition {
                        line: c.line,
                        col: c.col,
                    },
                    anchor: None,
                    group: None,
                    display: c.message.clone(),
                }
            })
        }
        PickerCandidates::References(v) => ranked_entries(picker, |ci| {
            let c = &v[ci];
            let start = LogicalPosition {
                line: c.line,
                col: c.col,
            };
            let end = LogicalPosition {
                line: c.end_line,
                col: c.end_col,
            };
            JumplistEntry {
                path_index: None,
                relative_path: None,
                abs_path: c.abs_path.clone(),
                position: end,
                anchor: (end != start).then_some(start),
                group: Some(GroupHeader::Label {
                    label: if c.is_definition {
                        "Definition".into()
                    } else {
                        "References".into()
                    },
                }),
                display: c.preview.clone(),
            }
        }),
        PickerCandidates::Symbols(v) => {
            // `rerank` keeps non-matching ancestors as context rows (no `match_indices`, not
            // selectable). Re-run the same pattern to keep only real matches.
            let matched = symbol_match_mask(picker, matcher);
            let mut entries = Vec::new();
            for (ri, &ci) in picker.ranked.iter().enumerate() {
                if !matched[ri] {
                    continue;
                }
                let c = &v[ci as usize];
                entries.push((
                    ci,
                    JumplistEntry {
                        path_index: None,
                        relative_path: None,
                        abs_path: c.abs_path.clone(),
                        position: c.end,
                        anchor: (c.end != c.start).then_some(c.start),
                        group: None,
                        display: c.name.clone(),
                    },
                ));
            }
            entries
        }
        // Re-capture from the Jumplist picker itself: the candidates already *are* entries, so
        // the mapping is identity over the filtered ranked set — the list narrows in place.
        // (The caller preserves the original `source`/`query`; see `jumplist_capture`.)
        PickerCandidates::Jumplist(v) => ranked_entries(picker, |ci| v[ci].clone()),
        PickerCandidates::GitChanges(v) => {
            let re = picker.content_regex();
            ranked_entries(picker, |ci| {
                let c = &v[ci];
                let (path_index, relative_path) = relative_parts(c.path_index, &c.relative_path);
                JumplistEntry {
                    path_index,
                    relative_path,
                    abs_path: c.abs_path.clone(),
                    // Query-aware, like select: land on the matched line, not the hunk anchor.
                    position: LogicalPosition {
                        line: c.select_line(re.as_ref()),
                        col: 0,
                    },
                    anchor: None,
                    // Both the workspace-wide and buffer-locked pickers group by file in the
                    // jumplist (`assign_file_groups`) — see the module note on grouping.
                    group: None,
                    display: c.preview(re.as_ref()).0,
                }
            })
        }
        _ => return None,
    };
    if entries.is_empty() {
        return None;
    }
    let mut entries = entries;
    normalize(&mut entries);
    let (candidate_indices, entries): (Vec<u32>, Vec<JumplistEntry>) = entries.into_iter().unzip();
    Some((
        Jumplist {
            source: picker.kind,
            query: picker.query.clone(),
            entries,
        },
        candidate_indices,
    ))
}

/// A candidate's workspace-relative parts as `Option`s, treating the empty-string sentinel some
/// buffer-scoped pickers use (they render flat and never compute a relative path) as *absent*.
/// Keeping the parts self-consistent — `abs_path` is always the source of truth — avoids the
/// `Some("")` that would make an open resolve to `root + ""`, i.e. the root directory.
fn relative_parts(path_index: u32, relative_path: &str) -> (Option<u32>, Option<String>) {
    if relative_path.is_empty() {
        (None, None)
    } else {
        (Some(path_index), Some(relative_path.to_string()))
    }
}

/// Enrich freshly captured entries with their file identity: derive each entry's workspace-relative
/// parts from `abs_path` (when it lives inside `roots`) and give every still-ungrouped entry a
/// group header. Run by the capture handler, which has the active workspace's roots.
///
/// Two jobs, one pass:
/// - **Grouping.** The buffer-scoped source pickers (buffer diagnostics, single-file changes) and
///   document symbols render flat — no file header. But the jumplist is reopened from anywhere,
///   often on a *different* buffer, so every row must show which file it belongs to. Entries that
///   already carry a header (grep's `File`, references' `Definition`/`References` labels) keep it.
///   Out-of-workspace files (references into dependencies) get their absolute path as a `Label`
///   header — the WorkspaceSymbols convention, and what makes grouping *total*: the Jumplist
///   picker's collapsible row space keys every row (docs/picker-groups.md).
/// - **Path consistency.** Buffer-scoped candidates arrive with no relative parts (see
///   [`relative_parts`]); filling them from `abs_path` makes the open resolve the same file the
///   source picker's *select* would (which opens by `abs_path`) instead of erroring.
///   Out-of-workspace files keep `None` parts and open by absolute path.
pub fn assign_file_groups(entries: &mut [JumplistEntry], roots: &[std::path::PathBuf]) {
    for e in entries {
        if e.relative_path.is_none() {
            if let Some((i, rel)) = crate::workspace_index::workspace_relative_parts(
                std::path::Path::new(&e.abs_path),
                roots,
            ) {
                e.path_index = Some(i);
                e.relative_path = Some(rel);
            }
        }
        if e.group.is_none() {
            e.group = Some(match (e.path_index, e.relative_path.clone()) {
                (Some(path_index), Some(relative_path)) => GroupHeader::File {
                    path_index,
                    relative_path,
                },
                _ => GroupHeader::Label {
                    label: e.abs_path.clone(),
                },
            });
        }
    }
}

/// Whether a captured list is worth path-scoping — echoed as `PickerViewResult::path_filterable`
/// to gate the Jumplist picker's dir/glob chips client-side. True when the entries span more than
/// one file *and* at least one sits inside a workspace root: a single-file list is all-or-nothing
/// under a path filter (the same reason `GitChangesFile` offers no scope chips), and an
/// all-external list has no root-relative paths for scopes or globs to match. Callers pass stored
/// entries (post-[`assign_file_groups`], so in-root entries have their relative parts filled).
pub fn path_filterable(entries: &[JumplistEntry]) -> bool {
    let mut first_file = None;
    let mut multi_file = false;
    let mut any_in_root = false;
    for e in entries {
        multi_file |= *first_file.get_or_insert(&e.abs_path) != &e.abs_path;
        any_in_root |= e.relative_path.is_some();
        if multi_file && any_in_root {
            return true;
        }
    }
    false
}

/// Map every ranked index through `make`, keeping the candidate index alongside — the common
/// per-kind loop.
fn ranked_entries(
    picker: &PickerState,
    mut make: impl FnMut(usize) -> JumplistEntry,
) -> Vec<(u32, JumplistEntry)> {
    picker
        .ranked
        .iter()
        .map(|&ci| (ci, make(ci as usize)))
        .collect()
}

/// Which ranked rows actually match the picker's query (all of them on an empty query) — the
/// same `Pattern` scoring `rerank` used, so ancestor context rows score `None`.
fn symbol_match_mask(picker: &PickerState, matcher: &mut Matcher) -> Vec<bool> {
    if picker.query.is_empty() {
        return vec![true; picker.ranked.len()];
    }
    let pattern = Pattern::parse(&picker.query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    picker
        .ranked
        .iter()
        .map(|&ci| {
            let haystack = Utf32Str::new(picker.candidates.display_at(ci as usize), &mut buf);
            pattern.score(haystack, matcher).is_some()
        })
        .collect()
}

/// Byte col of the *last char* of a grep match starting at byte `col` with byte length `len`
/// within `line_text` — the cursor end of the selection that covers the match (multi-byte
/// safe: steps back one char from the exclusive end). Falls back to `col` (a point landing)
/// when the offsets don't line up with the text.
fn grep_match_last_char_col(line_text: &str, col: u32, len: u32) -> u32 {
    let start = col as usize;
    let end = start + len as usize;
    match line_text
        .get(start..end)
        .and_then(|m| m.chars().next_back())
    {
        Some(last) => (end - last.len_utf8()) as u32,
        None => col,
    }
}

/// Normalize capture order: group runs in first-appearance order, files within a group in path
/// order, entries within a file in position order. For the kinds whose ranking already
/// preserves document order (grep, changes) this is an identity transform; it only bites where
/// a fuzzy query score-ranked the rows (diagnostics, references) — spatial stepping wants
/// document order regardless of match score. Stable, so exact ties keep their ranked order.
/// Entries ride with their source candidate index (untouched, just carried).
fn normalize(entries: &mut Vec<(u32, JumplistEntry)>) {
    let mut group_rank: HashMap<Option<GroupKey>, usize> = HashMap::new();
    let ranks: Vec<usize> = entries
        .iter()
        .map(|(_, e)| {
            let next = group_rank.len();
            *group_rank.entry(group_key(&e.group)).or_insert(next)
        })
        .collect();
    let mut decorated: Vec<(usize, (u32, JumplistEntry))> =
        ranks.into_iter().zip(entries.drain(..)).collect();
    decorated.sort_by(|(ra, (_, a)), (rb, (_, b))| {
        ra.cmp(rb)
            .then_with(|| a.abs_path.cmp(&b.abs_path))
            .then_with(|| {
                let (sa, sb) = (a.start(), b.start());
                (sa.line, sa.col).cmp(&(sb.line, sb.col))
            })
    });
    entries.extend(decorated.into_iter().map(|(_, pair)| pair));
}

/// Hashable identity of a group header (`GroupHeader` itself doesn't derive `Hash`).
type GroupKey = (u32, String);

fn group_key(g: &Option<GroupHeader>) -> Option<GroupKey> {
    g.as_ref().map(|g| match g {
        GroupHeader::File {
            path_index,
            relative_path,
        } => (*path_index, relative_path.clone()),
        GroupHeader::Label { label } => (u32::MAX, label.clone()),
    })
}

/// Resolve one `jumplist/step`: the index of the entry `count` steps in `direction` from the
/// cursor, or `None` when the cursor is already at the boundary in that direction (the list does
/// **not** cycle). `current_abs` is the current buffer's canonical path (`None` for a scratch
/// buffer); `edge` is the cursor selection's *outer* edge in the step direction (max edge
/// forward, min backward), so an entry the cursor sits on is "current" and gets skipped —
/// repeated presses make progress.
///
/// Directional rules, generalizing the old grep navigation:
/// - Current file has entries: the first entry (list order) strictly past `edge`; when the
///   cursor is past them all, the entry after the file's last occurrence — or `None` if that
///   was already the last entry overall.
/// - Current file absent: virtual insertion by path comparison — the first entry of the file
///   that would sort immediately after (before) it, or `None` when nothing sorts that way.
/// - Scratch buffer: the first (forward) / last (backward) entry overall — entering the list
///   from outside, always a move.
///
/// A `count` past the boundary clamps to the last/first entry (still a move); only a bare step
/// that can't advance at all yields `None`. `entries` must be non-empty (a captured list always
/// is).
pub fn step_index(
    entries: &[JumplistEntry],
    direction: Direction,
    current_abs: Option<&str>,
    edge: LogicalPosition,
    count: u32,
) -> Option<usize> {
    let len = entries.len();
    let first = match direction {
        Direction::Forward => step_forward(entries, current_abs, edge)?,
        Direction::Backward => step_backward(entries, current_abs, edge)?,
    };
    let extra = count.saturating_sub(1) as usize;
    Some(match direction {
        Direction::Forward => (first + extra).min(len - 1),
        Direction::Backward => first.saturating_sub(extra),
    })
}

/// The outcome of a `CurrentFile`-scoped step ([`step_in_file`]): an in-file target, the file's
/// boundary in the step direction, or a file with no entries at all.
#[derive(Debug, PartialEq)]
pub enum InFileStep {
    Moved(usize),
    AtEnd,
    NoneInFile,
}

/// Resolve one `Alt-]` / `Alt-[`: step within the current buffer's file only (`Alt-]` = forward,
/// `Alt-[` = backward), never falling through to another file. `current_abs` is the buffer's
/// canonical path (`None` for a scratch buffer — no file, so [`InFileStep::NoneInFile`]); `edge`
/// is the cursor selection's outer edge in the step direction, so the entry the cursor sits on is
/// skipped. A `count` past the file's first/last entry clamps to it. Returns:
/// - [`InFileStep::Moved`] with the list index of the target,
/// - [`InFileStep::AtEnd`] when the file has entries but none lie past the cursor that way,
/// - [`InFileStep::NoneInFile`] when the file (or a scratch buffer) has no entries.
pub fn step_in_file(
    entries: &[JumplistEntry],
    direction: Direction,
    current_abs: Option<&str>,
    edge: LogicalPosition,
    count: u32,
) -> InFileStep {
    let Some(cur) = current_abs else {
        return InFileStep::NoneInFile;
    };
    // List indices of this file's entries, in list order (already position-sorted within a file
    // by `normalize`).
    let in_file: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.abs_path == cur)
        .map(|(i, _)| i)
        .collect();
    if in_file.is_empty() {
        return InFileStep::NoneInFile;
    }
    let extra = count.saturating_sub(1) as usize;
    let past = |i: usize| {
        let s = entries[i].start();
        (s.line, s.col)
    };
    match direction {
        Direction::Forward => {
            let Some(pos) = in_file
                .iter()
                .position(|&i| past(i) > (edge.line, edge.col))
            else {
                return InFileStep::AtEnd;
            };
            InFileStep::Moved(in_file[(pos + extra).min(in_file.len() - 1)])
        }
        Direction::Backward => {
            let Some(pos) = in_file
                .iter()
                .rposition(|&i| past(i) < (edge.line, edge.col))
            else {
                return InFileStep::AtEnd;
            };
            InFileStep::Moved(in_file[pos.saturating_sub(extra)])
        }
    }
}

/// The entry "at or after" the cursor in list order, wrapping to the first entry overall when
/// nothing follows — `picker/view`'s `center_on_cursor` resolution, so `Space j` opens framed
/// on "where you are" in the list. *Inclusive*, unlike [`step_index`]'s skip-current strictness:
/// the entry the cursor sits on is the answer, so reopening the picker right after a jump
/// highlights the entry just landed on. Same directional rules as stepping otherwise (in-file
/// first, then the file's last occurrence + 1, virtual insertion by path for absent files,
/// scratch → the first entry). `entries` must be non-empty.
pub fn nearest_index(
    entries: &[JumplistEntry],
    current_abs: Option<&str>,
    edge: LogicalPosition,
) -> usize {
    let len = entries.len();
    let Some(cur) = current_abs else {
        return 0;
    };
    let mut last_in_file = None;
    for (i, e) in entries.iter().enumerate() {
        if e.abs_path != cur {
            continue;
        }
        let s = e.start();
        if (s.line, s.col) >= (edge.line, edge.col) {
            return i;
        }
        last_in_file = Some(i);
    }
    if let Some(last) = last_in_file {
        return (last + 1) % len;
    }
    entries
        .iter()
        .position(|e| e.abs_path.as_str() > cur)
        .unwrap_or(0)
}

fn step_forward(
    entries: &[JumplistEntry],
    current_abs: Option<&str>,
    edge: LogicalPosition,
) -> Option<usize> {
    let len = entries.len();
    let Some(cur) = current_abs else {
        return Some(0);
    };
    let mut last_in_file = None;
    for (i, e) in entries.iter().enumerate() {
        if e.abs_path != cur {
            continue;
        }
        let s = e.start();
        if (s.line, s.col) > (edge.line, edge.col) {
            return Some(i);
        }
        last_in_file = Some(i);
    }
    if let Some(last) = last_in_file {
        // Past this file's entries: the next entry overall, or nothing if this was the last.
        return (last + 1 < len).then_some(last + 1);
    }
    entries.iter().position(|e| e.abs_path.as_str() > cur)
}

fn step_backward(
    entries: &[JumplistEntry],
    current_abs: Option<&str>,
    edge: LogicalPosition,
) -> Option<usize> {
    let len = entries.len();
    let Some(cur) = current_abs else {
        return Some(len - 1);
    };
    let mut first_in_file = None;
    for (i, e) in entries.iter().enumerate().rev() {
        if e.abs_path != cur {
            continue;
        }
        let s = e.start();
        if (s.line, s.col) < (edge.line, edge.col) {
            return Some(i);
        }
        first_in_file = Some(i);
    }
    if let Some(first) = first_in_file {
        // Before this file's entries: the previous entry overall, or nothing if this was the first.
        // (`then`, not `then_some`, so `first - 1` isn't evaluated when `first == 0`.)
        return (first > 0).then(|| first - 1);
    }
    entries.iter().rposition(|e| e.abs_path.as_str() < cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(abs: &str, line: u32, col: u32) -> JumplistEntry {
        JumplistEntry {
            path_index: Some(0),
            relative_path: Some(abs.trim_start_matches('/').into()),
            abs_path: abs.into(),
            position: LogicalPosition { line, col },
            anchor: None,
            group: Some(GroupHeader::File {
                path_index: 0,
                relative_path: abs.trim_start_matches('/').into(),
            }),
            display: format!("{abs}:{line}:{col}"),
        }
    }

    fn labeled(abs: &str, line: u32, label: &str) -> JumplistEntry {
        JumplistEntry {
            group: Some(GroupHeader::Label {
                label: label.into(),
            }),
            ..entry(abs, line, 0)
        }
    }

    fn pos(line: u32, col: u32) -> LogicalPosition {
        LogicalPosition { line, col }
    }

    /// Pair test entries with synthetic candidate indices (their original position), the shape
    /// `normalize` carries through the sort.
    fn indexed(entries: Vec<JumplistEntry>) -> Vec<(u32, JumplistEntry)> {
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| (i as u32, e))
            .collect()
    }

    #[test]
    fn normalize_sorts_within_file_and_keeps_group_first_appearance() {
        // Score-ranked diagnostics: /b first (best match), then /a twice out of line order.
        let mut e = indexed(vec![
            entry("/b", 5, 0),
            entry("/a", 9, 0),
            entry("/a", 2, 0),
        ]);
        normalize(&mut e);
        let order: Vec<(&str, u32)> = e
            .iter()
            .map(|(_, e)| (e.abs_path.as_str(), e.position.line))
            .collect();
        // /b appeared first, so its group leads; /a's entries come position-sorted.
        assert_eq!(order, vec![("/b", 5), ("/a", 2), ("/a", 9)]);
        // Candidate indices travelled with their entries.
        assert_eq!(e.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![0, 2, 1]);
    }

    #[test]
    fn normalize_orders_files_within_a_label_group_by_path() {
        let mut e = indexed(vec![
            labeled("/z", 1, "References"),
            labeled("/a", 7, "References"),
            labeled("/m", 3, "Definition"),
        ]);
        normalize(&mut e);
        let order: Vec<&str> = e.iter().map(|(_, e)| e.abs_path.as_str()).collect();
        // "References" appeared first so it stays first; within it, files sort by path.
        assert_eq!(order, vec!["/a", "/z", "/m"]);
    }

    #[test]
    fn step_forward_within_file_then_falls_through_and_stops() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        // From /a:1 (on the first entry) → the next in-file entry.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/a"), pos(1, 0), 1),
            Some(1)
        );
        // Past /a's entries → the next file's first.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/a"), pos(5, 0), 1),
            Some(2)
        );
        // Past /b's entries → no wrap; stepping stops at the end.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/b"), pos(2, 0), 1),
            None
        );
    }

    #[test]
    fn step_backward_mirrors_and_stops() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/a"), pos(5, 0), 1),
            Some(0)
        );
        // Before /a's entries → no wrap; stepping stops at the start.
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/a"), pos(1, 0), 1),
            None
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/b"), pos(2, 0), 1),
            Some(1)
        );
    }

    #[test]
    fn step_virtually_inserts_files_not_in_the_list() {
        let e = vec![entry("/a", 1, 0), entry("/c", 2, 0)];
        // /b sits between /a and /c.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/b"), pos(0, 0), 1),
            Some(1)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/b"), pos(0, 0), 1),
            Some(0)
        );
        // Past either end (a file that sorts after / before every entry): no wrap, stop.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/z"), pos(0, 0), 1),
            None
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/A"), pos(0, 0), 1),
            None
        );
    }

    #[test]
    fn step_scratch_buffer_enters_the_list_at_the_ends() {
        let e = vec![entry("/a", 1, 0), entry("/b", 2, 0)];
        assert_eq!(
            step_index(&e, Direction::Forward, None, pos(0, 0), 1),
            Some(0)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, None, pos(0, 0), 1),
            Some(1)
        );
    }

    #[test]
    fn step_count_advances_and_clamps() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/a"), pos(0, 0), 2),
            Some(1)
        );
        // Count past the end clamps to the last entry (no wrap) — still a move.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/a"), pos(0, 0), 4),
            Some(2)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Some("/b"), pos(2, 0), 2),
            Some(0)
        );
    }

    #[test]
    fn anchored_entries_compare_by_span_start() {
        let mut spanned = entry("/a", 3, 8);
        spanned.anchor = Some(pos(3, 4)); // span [4..=8] on line 3
        let e = vec![entry("/a", 3, 2), spanned.clone(), entry("/a", 3, 12)];
        // Cursor edge at the span's start (col 4): the entry counts as current, step past it.
        assert_eq!(
            step_index(&e, Direction::Forward, Some("/a"), pos(3, 4), 1),
            Some(2)
        );
        assert_eq!(spanned.start(), pos(3, 4));
    }

    #[test]
    fn step_in_file_walks_only_the_current_file_then_stops() {
        // /a has two entries, /b sits between them in list order (different file).
        let e = vec![entry("/a", 1, 0), entry("/b", 2, 0), entry("/a", 5, 0)];
        // Forward from the top of /a → its first entry, skipping /b entirely.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/a"), pos(0, 0), 1),
            InFileStep::Moved(0)
        );
        // From /a:1 → /a's next entry (index 2), still not /b.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/a"), pos(1, 0), 1),
            InFileStep::Moved(2)
        );
        // On /a's last entry → no fall-through to /b; stops at the file's end.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/a"), pos(5, 0), 1),
            InFileStep::AtEnd
        );
        // Backward from /a's last → its first; before the first → stop.
        assert_eq!(
            step_in_file(&e, Direction::Backward, Some("/a"), pos(5, 0), 1),
            InFileStep::Moved(0)
        );
        assert_eq!(
            step_in_file(&e, Direction::Backward, Some("/a"), pos(1, 0), 1),
            InFileStep::AtEnd
        );
    }

    #[test]
    fn step_in_file_reports_none_for_a_file_without_entries() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0)];
        // /b has no captured entries.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/b"), pos(0, 0), 1),
            InFileStep::NoneInFile
        );
        // A scratch buffer (no path) likewise has nothing in "its" file.
        assert_eq!(
            step_in_file(&e, Direction::Forward, None, pos(0, 0), 1),
            InFileStep::NoneInFile
        );
    }

    #[test]
    fn step_in_file_count_clamps_within_the_file() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/a", 9, 0)];
        // Forward 2 from the top → the second entry (first-past-edge, then one more); a larger
        // count clamps to the last.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/a"), pos(0, 0), 2),
            InFileStep::Moved(1)
        );
        assert_eq!(
            step_in_file(&e, Direction::Forward, Some("/a"), pos(0, 0), 9),
            InFileStep::Moved(2)
        );
        // Backward 9 from the last clamps to the first.
        assert_eq!(
            step_in_file(&e, Direction::Backward, Some("/a"), pos(9, 0), 9),
            InFileStep::Moved(0)
        );
    }

    #[test]
    fn nearest_index_is_inclusive_and_wraps() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        // Inclusive: sitting exactly on an entry's start resolves to that entry (unlike
        // stepping, which skips it).
        assert_eq!(nearest_index(&e, Some("/a"), pos(1, 0)), 0);
        // Between entries: the next one at-or-after.
        assert_eq!(nearest_index(&e, Some("/a"), pos(2, 0)), 1);
        // Past the file's entries: the entry after its last occurrence.
        assert_eq!(nearest_index(&e, Some("/a"), pos(9, 0)), 2);
        // Past everything: wraps to the first entry overall.
        assert_eq!(nearest_index(&e, Some("/b"), pos(9, 0)), 0);
        // File not in the list: virtual insertion by path; scratch buffers take the top.
        assert_eq!(nearest_index(&e, Some("/ab"), pos(0, 0)), 2);
        assert_eq!(nearest_index(&e, None, pos(0, 0)), 0);
    }

    #[test]
    fn grep_match_end_is_multibyte_safe() {
        // "héllo": match "hél" from byte 0 is 4 bytes; last char 'l' starts at byte 3.
        assert_eq!(grep_match_last_char_col("héllo", 0, 4), 3);
        // Single-char match: end == start.
        assert_eq!(grep_match_last_char_col("abc", 1, 1), 1);
        // Out-of-range offsets fall back to the start.
        assert_eq!(grep_match_last_char_col("abc", 10, 4), 10);
    }

    #[test]
    fn relative_parts_treats_the_empty_sentinel_as_absent() {
        // Buffer-scoped candidates carry an empty relative path — that's "unset", not a real
        // root-relative "" (which would open the root directory).
        assert_eq!(relative_parts(0, ""), (None, None));
        assert_eq!(
            relative_parts(2, "src/main.rs"),
            (Some(2), Some("src/main.rs".to_string()))
        );
    }

    /// A headerless, path-less entry (the buffer-scoped diagnostics shape) with an `abs_path`.
    fn headerless(abs: &str) -> JumplistEntry {
        JumplistEntry {
            path_index: None,
            relative_path: None,
            abs_path: abs.into(),
            position: pos(3, 0),
            anchor: None,
            group: None,
            display: "unused variable `x`".into(),
        }
    }

    #[test]
    fn assign_file_groups_derives_parts_and_groups_headerless_entries() {
        let roots = vec![std::path::PathBuf::from("/ws")];
        let mut entries = vec![headerless("/ws/src/lib.rs")];
        assign_file_groups(&mut entries, &roots);
        // Relative parts derived from abs_path — so the open resolves the file, not `root + ""`.
        assert_eq!(entries[0].path_index, Some(0));
        assert_eq!(entries[0].relative_path.as_deref(), Some("src/lib.rs"));
        // And a File header so the picker shows which file the row belongs to.
        assert_eq!(
            entries[0].group,
            Some(GroupHeader::File {
                path_index: 0,
                relative_path: "src/lib.rs".into(),
            })
        );
    }

    #[test]
    fn path_filterable_needs_multiple_files_with_an_in_root_entry() {
        // Two in-root files: scoping can distinguish them.
        assert!(path_filterable(&[entry("/a", 1, 0), entry("/b", 2, 0)]));
        // One file (however many entries): a path filter is all-or-nothing.
        assert!(!path_filterable(&[entry("/a", 1, 0), entry("/a", 9, 0)]));
        // Multiple files but all external (no relative parts): nothing root-relative to match.
        assert!(!path_filterable(&[
            headerless("/dep/a.rs"),
            headerless("/dep/b.rs"),
        ]));
        // Mixed: one in-root + one external still benefits (scope to the in-root side).
        assert!(path_filterable(&[
            entry("/ws/a.rs", 1, 0),
            headerless("/dep/b.rs")
        ]));
        assert!(!path_filterable(&[]));
    }

    #[test]
    fn assign_file_groups_keeps_labels_and_labels_external_files_by_abs_path() {
        let roots = vec![std::path::PathBuf::from("/ws")];
        let mut labeled_entry = headerless("/ws/a.rs");
        labeled_entry.group = Some(GroupHeader::Label {
            label: "References".into(),
        });
        let external = headerless("/elsewhere/dep.rs");
        let mut entries = vec![labeled_entry, external];
        assign_file_groups(&mut entries, &roots);
        // An existing header (references' Label) is preserved; parts are still derived.
        assert!(matches!(entries[0].group, Some(GroupHeader::Label { .. })));
        assert_eq!(entries[0].relative_path.as_deref(), Some("a.rs"));
        // Out-of-workspace: no parts to derive (opens by absolute path), but grouping is total —
        // the collapsible picker keys every row — so the absolute path becomes the header.
        assert_eq!(entries[1].path_index, None);
        assert_eq!(entries[1].relative_path, None);
        assert_eq!(
            entries[1].group,
            Some(GroupHeader::Label {
                label: "/elsewhere/dep.rs".into(),
            })
        );
    }
}
