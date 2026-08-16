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
//!
//! There are two entry shapes, and most of the care in this module goes into keeping the second
//! one from needing special cases at every call site:
//!
//! - **Positioned** — a location *inside* a file (grep hits, diagnostics, references, symbols,
//!   hunks). Stepping walks these cursor-relative within a file, then falls through across files.
//! - **Whole-target** — a file or buffer with no position at all, captured from the Files and
//!   Buffers pickers. It opens where the cursor last sat in that buffer (`jump_to: None`, which
//!   is exactly what selecting the row in its source picker does); it is always "current" for
//!   its own buffer, so a step out of it lands on the neighbouring entry — `]`/`[` walk a
//!   captured file list one file at a time; and it carries no group, since a per-file header
//!   above a row that *is* that file would only repeat it. Grouping is all-or-nothing per
//!   capture ([`Jumplist::grouped`]), and an ungrouped list renders flat rather than as a
//!   collapsible accordion (docs/picker-groups.md).

use crate::picker::{PickerCandidates, PickerState};
use aether_protocol::cursor::Direction;
use aether_protocol::picker::{GroupHeader, PickerKind};
use aether_protocol::{BufferId, LogicalPosition};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use std::collections::HashMap;

/// One client's captured list. `source`/`query` describe where it came from (status display,
/// the Jumplist picker's context); `entries` are in normalized order (see [`normalize`]).
pub struct Jumplist {
    pub source: PickerKind,
    pub query: String,
    /// Whether the entries carry group headers — `PickerKind::groups_in_jumplist` for the source,
    /// and uniform across the list (see the module note). Drives whether the capture handler runs
    /// [`assign_file_groups`] and whether the Jumplist picker renders collapsible.
    pub grouped: bool,
    pub entries: Vec<JumplistEntry>,
}

/// What a captured entry points at. A file is identified by its path (so the entry survives the
/// buffer being closed, and can be scoped by the dir/glob chips); a buffer with no path — a
/// scratch, the one row shape the Buffers picker offers that isn't a file — can only be
/// identified by its live id, exactly as that picker's own `select` identifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JumplistTarget {
    File {
        /// Workspace root index + root-relative path when the file is inside a root; `None` for
        /// out-of-root files (references into dependencies). Lets the open route root-relative
        /// when possible (matching how the source pickers open).
        path_index: Option<u32>,
        relative_path: Option<String>,
        /// Absolute canonical path — always present, the file identity used for stepping.
        abs_path: String,
    },
    /// A pathless buffer (scratch). Dies with the buffer: opening a closed id errors, the same
    /// way selecting a stale row in the Buffers picker would.
    Buffer { buffer_id: BufferId },
}

/// Where a step is being taken *from*: the current buffer's identity in the same terms entries
/// use. Every buffer is one or the other — file-backed buffers (including external ones and
/// files deleted underneath us) have a canonical path, and the rest are scratch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location<'a> {
    File(&'a str),
    Buffer(BufferId),
}

/// Total order over targets, used both to sort a capture ([`normalize`]) and to virtually insert
/// a *current* location that has no entries into the sequence ([`step_index`]). Files sort first,
/// by path; pathless buffers follow, by id — which for scratch buffers is creation order, i.e.
/// the order their `(scratch N)` numbers were handed out.
type TargetKey<'a> = (u8, &'a str, BufferId);

impl JumplistTarget {
    /// The absolute path, for a file target.
    pub fn abs_path(&self) -> Option<&str> {
        match self {
            JumplistTarget::File { abs_path, .. } => Some(abs_path),
            JumplistTarget::Buffer { .. } => None,
        }
    }

    /// The buffer id, for a pathless target.
    pub fn buffer_id(&self) -> Option<BufferId> {
        match self {
            JumplistTarget::File { .. } => None,
            JumplistTarget::Buffer { buffer_id } => Some(*buffer_id),
        }
    }

    /// The workspace-relative parts, when the target is a file inside a root.
    pub fn relative_parts(&self) -> Option<(u32, &str)> {
        match self {
            JumplistTarget::File {
                path_index: Some(i),
                relative_path: Some(rel),
                ..
            } => Some((*i, rel)),
            _ => None,
        }
    }

    fn sort_key(&self) -> TargetKey<'_> {
        match self {
            JumplistTarget::File { abs_path, .. } => (0, abs_path, 0),
            JumplistTarget::Buffer { buffer_id } => (1, "", *buffer_id),
        }
    }

    /// Whether this target *is* the buffer a step is being taken from.
    fn matches(&self, location: Location<'_>) -> bool {
        match (self, location) {
            (JumplistTarget::File { abs_path, .. }, Location::File(p)) => abs_path == p,
            (JumplistTarget::Buffer { buffer_id }, Location::Buffer(id)) => *buffer_id == id,
            _ => false,
        }
    }
}

impl Location<'_> {
    fn sort_key(&self) -> TargetKey<'_> {
        match self {
            Location::File(p) => (0, p, 0),
            Location::Buffer(id) => (1, "", *id),
        }
    }
}

/// One captured result. `position`/`anchor` mirror `PickerSelectResult::FileAt` for the source
/// row exactly: `position` is where the cursor lands (a span's inclusive end), `anchor` the
/// selection's other end (the span start) or `None` for a point landing.
#[derive(Clone, Debug, PartialEq)]
pub struct JumplistEntry {
    /// The file or buffer this entry jumps to.
    pub target: JumplistTarget,
    /// Where in the target to land, or `None` for a whole-target entry (see the module note),
    /// which opens on the target's last-known cursor instead.
    pub position: Option<LogicalPosition>,
    pub anchor: Option<LogicalPosition>,
    /// The source picker's group header for this row (file or section label). `None` throughout
    /// for an ungrouped capture ([`Jumplist::grouped`]); for a grouped one it is `None` only
    /// while the capture is being assembled — the flat source kinds (buffer diagnostics,
    /// document symbols, single-file changes) leave it empty and [`assign_file_groups`] fills it
    /// in, so that every stored entry has one. That totality is what the collapsible row space
    /// requires of the views it *does* key (docs/picker-groups.md).
    pub group: Option<GroupHeader>,
    /// The source row's text — the Jumplist picker's row + fuzzy haystack.
    pub display: String,
}

impl JumplistEntry {
    /// The entry's spatial start — the span's first char for anchored entries, else the landing
    /// point. Stepping compares cursor edges against this (an entry is "after" the cursor when
    /// its start is). `None` for a whole-target entry: it has no position to compare, which is
    /// what makes it always "current" for its own buffer and never an in-file step.
    pub fn start(&self) -> Option<LogicalPosition> {
        let position = self.position?;
        Some(match self.anchor {
            Some(a) if (a.line, a.col) < (position.line, position.col) => a,
            _ => position,
        })
    }

    /// Shorthand for the common `target.abs_path()`.
    pub fn abs_path(&self) -> Option<&str> {
        self.target.abs_path()
    }

    /// Whether this entry belongs to the buffer a step / status stamp is being taken from.
    pub fn matches_location(&self, location: Location<'_>) -> bool {
        self.target.matches(location)
    }
}

/// The step-origin identity of a buffer: its canonical path when it has one, else its id. The one
/// place that decision is made, so stepping, framing and the status stamp can't disagree about
/// what "the current target" means.
pub fn location_of(canonical_path: Option<&str>, buffer_id: BufferId) -> Location<'_> {
    match canonical_path {
        Some(abs) => Location::File(abs),
        None => Location::Buffer(buffer_id),
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
        // The file-shaped pickers: a row is a whole target, not a location in one. No position
        // (the open lands on the target's last-known cursor, like selecting the row there would)
        // and no group (see the module note) — the two together are what make a captured file
        // list render and step as a flat sequence of files.
        PickerCandidates::Files { files, .. } => ranked_entries(picker, |ci| {
            let c = &files[ci];
            JumplistEntry {
                target: JumplistTarget::File {
                    path_index: Some(c.path_index),
                    relative_path: Some(c.relative_path.clone()),
                    abs_path: c.abs.clone(),
                },
                position: None,
                anchor: None,
                group: None,
                display: c.relative_path.clone(),
            }
        }),
        // File-backed buffers capture as *file* targets: the path is the more durable identity
        // (the entry survives the buffer closing, and the dir/glob chips can speak about it), and
        // opening by path re-attaches the same document anyway. Only the pathless ones — scratch
        // buffers — need the buffer id, which is how this picker's own `select` identifies them.
        PickerCandidates::Buffers(v) => ranked_entries(picker, |ci| {
            let c = &v[ci];
            let target = match &c.abs_path {
                Some(abs) => JumplistTarget::File {
                    path_index: c.path.as_ref().map(|(i, _)| *i),
                    relative_path: c.path.as_ref().map(|(_, rel)| rel.clone()),
                    abs_path: abs.clone(),
                },
                None => JumplistTarget::Buffer {
                    buffer_id: c.buffer_id,
                },
            };
            JumplistEntry {
                target,
                position: None,
                anchor: None,
                group: None,
                display: c.display.clone(),
            }
        }),
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
                target: JumplistTarget::File {
                    path_index: Some(c.path_index),
                    relative_path: Some(c.relative_path.clone()),
                    abs_path: c.abs_path.clone(),
                },
                position: Some(position),
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
                    target: JumplistTarget::File {
                        path_index,
                        relative_path,
                        abs_path: c.abs_path.clone(),
                    },
                    position: Some(LogicalPosition {
                        line: c.line,
                        col: c.col,
                    }),
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
                target: JumplistTarget::File {
                    path_index: None,
                    relative_path: None,
                    abs_path: c.abs_path.clone(),
                },
                position: Some(end),
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
                        target: JumplistTarget::File {
                            path_index: None,
                            relative_path: None,
                            abs_path: c.abs_path.clone(),
                        },
                        position: Some(c.end),
                        anchor: (c.end != c.start).then_some(c.start),
                        group: None,
                        display: c.name.clone(),
                    },
                ));
            }
            entries
        }
        // Unlike DocumentSymbols there's no context-row mask to apply: workspace symbols rank
        // without filtering (the LSP server did the matching — docs/workspace-symbols.md), so
        // every ranked row is a real result and the snapshot takes them all, path chips already
        // honoured by `rerank`.
        PickerCandidates::WorkspaceSymbols(v) => ranked_entries(picker, |ci| {
            let c = &v[ci];
            JumplistEntry {
                target: JumplistTarget::File {
                    // In-root: `display_path` *is* the root-relative path. Out-of-root symbols
                    // (dependencies, the stdlib) have no parts — `assign_file_groups` then gives
                    // them their absolute-path `Label` header, the convention this picker set.
                    path_index: c.path_index,
                    relative_path: c.path_index.is_some().then(|| c.display_path.clone()),
                    abs_path: c.abs_path.clone(),
                },
                position: Some(LogicalPosition {
                    line: c.line,
                    col: c.col,
                }),
                anchor: None,
                group: None,
                display: c.name.clone(),
            }
        }),
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
                    target: JumplistTarget::File {
                        path_index,
                        relative_path,
                        abs_path: c.abs_path.clone(),
                    },
                    // Query-aware, like select: land on the matched line, not the hunk anchor.
                    position: Some(LogicalPosition {
                        line: c.select_line(re.as_ref()),
                        col: 0,
                    }),
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
    // A re-capture from the Jumplist picker maps entries through unchanged, so it inherits their
    // grouping rather than re-deriving it from a source kind that is only "Jumplist".
    let grouped = match picker.kind {
        PickerKind::Jumplist => entries.first().is_some_and(|(_, e)| e.group.is_some()),
        kind => kind.groups_in_jumplist(),
    };
    let (candidate_indices, entries): (Vec<u32>, Vec<JumplistEntry>) = entries.into_iter().unzip();
    Some((
        Jumplist {
            source: picker.kind,
            query: picker.query.clone(),
            grouped,
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
        if let JumplistTarget::File {
            path_index,
            relative_path,
            abs_path,
        } = &mut e.target
        {
            if relative_path.is_none() {
                if let Some((i, rel)) = crate::workspace_index::workspace_relative_parts(
                    std::path::Path::new(abs_path),
                    roots,
                ) {
                    *path_index = Some(i);
                    *relative_path = Some(rel);
                }
            }
        }
        if e.group.is_none() {
            e.group = Some(match e.target.relative_parts() {
                Some((path_index, relative_path)) => GroupHeader::File {
                    path_index,
                    relative_path: relative_path.to_string(),
                },
                // Out-of-workspace files label by absolute path; a pathless buffer (which only
                // reaches here if an ungrouped source ever grew a group) by its own row text.
                None => GroupHeader::Label {
                    label: e
                        .target
                        .abs_path()
                        .unwrap_or(e.display.as_str())
                        .to_string(),
                },
            });
        }
    }
}

/// Whether a captured list is worth path-scoping — echoed as `PickerViewResult::path_filterable`
/// to gate the Jumplist picker's dir/glob chips client-side. True when the entries span more than
/// one target *and* at least one sits inside a workspace root: a single-file list is all-or-nothing
/// under a path filter (the same reason `GitChangesFile` offers no scope chips), and an
/// all-external list has no root-relative paths for scopes or globs to match. Callers pass stored
/// entries (post-[`assign_file_groups`], so in-root entries have their relative parts filled).
pub fn path_filterable(entries: &[JumplistEntry]) -> bool {
    let mut first: Option<TargetKey> = None;
    let mut multi_target = false;
    let mut any_in_root = false;
    for e in entries {
        let key = e.target.sort_key();
        multi_target |= *first.get_or_insert(key) != key;
        any_in_root |= e.target.relative_parts().is_some();
        if multi_target && any_in_root {
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

/// Normalize capture order: group runs in first-appearance order, targets within a group in
/// [`TargetKey`] order (files by path, then pathless buffers by id), entries within a target in
/// position order. For the kinds whose ranking already preserves document order (grep, changes)
/// this is an identity transform; it only bites where a fuzzy query score-ranked the rows
/// (diagnostics, references, and the file-shaped captures, whose ranking is a fuzzy score over
/// paths) — stepping is spatial, and wants document order regardless of match score. Stable, so
/// exact ties keep their ranked order. Entries ride with their source candidate index (untouched,
/// just carried).
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
            .then_with(|| a.target.sort_key().cmp(&b.target.sort_key()))
            .then_with(|| {
                // Position-less entries (there is at most one per target) sort ahead of any
                // positioned sibling, which only arises if a source ever mixed the two shapes.
                let key = |e: &JumplistEntry| e.start().map(|s| (s.line, s.col));
                key(a).cmp(&key(b))
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
/// **not** cycle). `current` is the step buffer's identity; `edge` is the cursor selection's
/// *outer* edge in the step direction (max edge forward, min backward), so an entry the cursor
/// sits on is "current" and gets skipped — repeated presses make progress.
///
/// Directional rules, generalizing the old grep navigation:
/// - Current target has entries: the first entry (list order) strictly past `edge`; when the
///   cursor is past them all, the entry after the target's last occurrence — or `None` if that
///   was already the last entry overall. A whole-target entry is never "past" the cursor, so a
///   list of them steps one target per press.
/// - Current target absent: virtual insertion by [`TargetKey`] comparison — the first entry of
///   the target that would sort immediately after (before) it, or `None` when nothing sorts
///   that way.
/// - Pathless buffer not in the list: the first (forward) / last (backward) entry overall —
///   entering the list from outside, always a move.
///
/// A `count` past the boundary clamps to the last/first entry (still a move); only a bare step
/// that can't advance at all yields `None`. `entries` must be non-empty (a captured list always
/// is).
pub fn step_index(
    entries: &[JumplistEntry],
    direction: Direction,
    current: Location<'_>,
    edge: LogicalPosition,
    count: u32,
) -> Option<usize> {
    let len = entries.len();
    let first = match direction {
        Direction::Forward => step_forward(entries, current, edge)?,
        Direction::Backward => step_backward(entries, current, edge)?,
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

/// Resolve one `}` / `{`: step within the current buffer's own entries only (`}` = forward,
/// `{` = backward), never falling through to another file. `current` is the buffer's identity;
/// `edge` is the cursor selection's outer edge in the step direction, so the entry the cursor
/// sits on is skipped. A `count` past the file's first/last entry clamps to it. Returns:
/// - [`InFileStep::Moved`] with the list index of the target,
/// - [`InFileStep::AtEnd`] when the file has entries but none lie past the cursor that way,
/// - [`InFileStep::NoneInFile`] when the buffer has no *positioned* entries — nothing captured
///   for it at all, or only a whole-target entry, which has no interior to walk.
pub fn step_in_file(
    entries: &[JumplistEntry],
    direction: Direction,
    current: Location<'_>,
    edge: LogicalPosition,
    count: u32,
) -> InFileStep {
    // List indices of this buffer's positioned entries, in list order (already position-sorted
    // within a target by `normalize`), each with the start the cursor compares against.
    let in_file: Vec<(usize, LogicalPosition)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.target.matches(current))
        .filter_map(|(i, e)| e.start().map(|s| (i, s)))
        .collect();
    if in_file.is_empty() {
        return InFileStep::NoneInFile;
    }
    let extra = count.saturating_sub(1) as usize;
    match direction {
        Direction::Forward => {
            let Some(pos) = in_file
                .iter()
                .position(|(_, s)| (s.line, s.col) > (edge.line, edge.col))
            else {
                return InFileStep::AtEnd;
            };
            InFileStep::Moved(in_file[(pos + extra).min(in_file.len() - 1)].0)
        }
        Direction::Backward => {
            let Some(pos) = in_file
                .iter()
                .rposition(|(_, s)| (s.line, s.col) < (edge.line, edge.col))
            else {
                return InFileStep::AtEnd;
            };
            InFileStep::Moved(in_file[pos.saturating_sub(extra)].0)
        }
    }
}

/// The entry "at or after" the cursor in list order, wrapping to the first entry overall when
/// nothing follows — `picker/view`'s `center_on_cursor` resolution, so `Space j` opens framed
/// on "where you are" in the list. *Inclusive*, unlike [`step_index`]'s skip-current strictness:
/// the entry the cursor sits on is the answer, so reopening the picker right after a jump
/// highlights the entry just landed on. A whole-target entry for the current buffer is the
/// strongest form of that — being *in* the buffer is being on the entry, wherever the cursor
/// sits. Same directional rules as stepping otherwise (in-file first, then the target's last
/// occurrence + 1, virtual insertion by [`TargetKey`] for absent targets, an absent pathless
/// buffer → the first entry). `entries` must be non-empty.
pub fn nearest_index(
    entries: &[JumplistEntry],
    current: Location<'_>,
    edge: LogicalPosition,
) -> usize {
    let len = entries.len();
    let mut last_in_file = None;
    for (i, e) in entries.iter().enumerate() {
        if !e.target.matches(current) {
            continue;
        }
        match e.start() {
            None => return i,
            Some(s) if (s.line, s.col) >= (edge.line, edge.col) => return i,
            Some(_) => last_in_file = Some(i),
        }
    }
    if let Some(last) = last_in_file {
        return (last + 1) % len;
    }
    virtual_insert(entries, current, Direction::Forward).unwrap_or(0)
}

fn step_forward(
    entries: &[JumplistEntry],
    current: Location<'_>,
    edge: LogicalPosition,
) -> Option<usize> {
    let len = entries.len();
    let mut last_in_file = None;
    for (i, e) in entries.iter().enumerate() {
        if !e.target.matches(current) {
            continue;
        }
        // A whole-target entry never lies past the cursor: it *is* where the cursor is, so it
        // counts as current and the step falls through to the next entry.
        if let Some(s) = e.start() {
            if (s.line, s.col) > (edge.line, edge.col) {
                return Some(i);
            }
        }
        last_in_file = Some(i);
    }
    if let Some(last) = last_in_file {
        // Past this target's entries: the next entry overall, or nothing if this was the last.
        return (last + 1 < len).then_some(last + 1);
    }
    virtual_insert(entries, current, Direction::Forward)
}

fn step_backward(
    entries: &[JumplistEntry],
    current: Location<'_>,
    edge: LogicalPosition,
) -> Option<usize> {
    let mut first_in_file = None;
    for (i, e) in entries.iter().enumerate().rev() {
        if !e.target.matches(current) {
            continue;
        }
        if let Some(s) = e.start() {
            if (s.line, s.col) < (edge.line, edge.col) {
                return Some(i);
            }
        }
        first_in_file = Some(i);
    }
    if let Some(first) = first_in_file {
        // Before this target's entries: the previous entry overall, or nothing if this was the
        // first. (`then`, not `then_some`, so `first - 1` isn't evaluated when `first == 0`.)
        return (first > 0).then(|| first - 1);
    }
    virtual_insert(entries, current, Direction::Backward)
}

/// Where a location with no entries of its own lands: the first entry of the target that sorts
/// immediately after it (forward) or before it (backward). A *pathless* buffer instead enters the
/// list at its end — it sorts after every file, so path comparison would only ever say "nothing
/// that way", and entering from outside should always be a move (the long-standing scratch rule).
fn virtual_insert(
    entries: &[JumplistEntry],
    current: Location<'_>,
    direction: Direction,
) -> Option<usize> {
    let key = current.sort_key();
    match (current, direction) {
        (Location::Buffer(_), Direction::Forward) => Some(0),
        (Location::Buffer(_), Direction::Backward) => Some(entries.len() - 1),
        (Location::File(_), Direction::Forward) => {
            entries.iter().position(|e| e.target.sort_key() > key)
        }
        (Location::File(_), Direction::Backward) => {
            entries.iter().rposition(|e| e.target.sort_key() < key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_target(abs: &str) -> JumplistTarget {
        JumplistTarget::File {
            path_index: Some(0),
            relative_path: Some(abs.trim_start_matches('/').into()),
            abs_path: abs.into(),
        }
    }

    fn entry(abs: &str, line: u32, col: u32) -> JumplistEntry {
        JumplistEntry {
            target: file_target(abs),
            position: Some(LogicalPosition { line, col }),
            anchor: None,
            group: Some(GroupHeader::File {
                path_index: 0,
                relative_path: abs.trim_start_matches('/').into(),
            }),
            display: format!("{abs}:{line}:{col}"),
        }
    }

    /// A whole-file entry — the Files/Buffers capture shape: no position, no group.
    fn whole_file(abs: &str) -> JumplistEntry {
        JumplistEntry {
            target: file_target(abs),
            position: None,
            anchor: None,
            group: None,
            display: abs.trim_start_matches('/').into(),
        }
    }

    /// A whole-*buffer* entry — a scratch row from the Buffers picker.
    fn scratch(buffer_id: BufferId) -> JumplistEntry {
        JumplistEntry {
            target: JumplistTarget::Buffer { buffer_id },
            position: None,
            anchor: None,
            group: None,
            display: format!("(scratch {buffer_id})"),
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

    /// The workspace-symbols capture arm: every shown row snapshots (rank-don't-filter means no
    /// context-row mask), in-root candidates carry their `(path_index, display_path)` file
    /// identity, out-of-root ones stay pathless for `assign_file_groups`' Label fallback.
    /// Regression: the arm was missing entirely, so `Ctrl-j` in the picker always answered
    /// "Nothing to capture".
    #[test]
    fn capture_snapshots_workspace_symbols_with_their_file_identity() {
        use crate::picker::{make_matcher, PickerCandidates, PickerState, WorkspaceSymbolCandidate};
        let cand = |path_index: Option<u32>, display: &str, abs: &str, line: u32, name: &str| {
            WorkspaceSymbolCandidate {
                abs_path: abs.into(),
                path_index,
                display_path: display.into(),
                line,
                col: 3,
                name: name.into(),
                symbol_kind: aether_protocol::picker::SymbolKind::Function,
                container: String::new(),
            }
        };
        let picker = PickerState::new(PickerCandidates::WorkspaceSymbols(vec![
            cand(Some(0), "src/a.rs", "/w/src/a.rs", 9, "alpha"),
            cand(Some(0), "src/a.rs", "/w/src/a.rs", 2, "beta"),
            // Out-of-root (a dependency): no parts to carry.
            cand(None, "/dep/x.rs", "/dep/x.rs", 1, "dep_fn"),
        ]));
        let (list, indices) = capture(&picker, &mut make_matcher()).expect("captures");
        assert_eq!(list.source, PickerKind::WorkspaceSymbols);
        assert_eq!(list.entries.len(), 3, "every shown row snapshots");

        // Normalized: still ungrouped at this stage (`assign_file_groups` runs in the handler),
        // so files order by path — /dep before /w — and within a file spatially (beta at line 2
        // before alpha at 9), candidate indices travelling alongside.
        let order: Vec<(&str, Option<u32>)> = list
            .entries
            .iter()
            .map(|e| (e.display.as_str(), e.position.map(|p| p.line)))
            .collect();
        assert_eq!(
            order,
            vec![("dep_fn", Some(1)), ("beta", Some(2)), ("alpha", Some(9))]
        );
        assert_eq!(indices, vec![2, 1, 0]);
        assert!(list.grouped, "a position-shaped source captures grouped");

        let beta = &list.entries[1];
        assert_eq!(beta.target.relative_parts(), Some((0, "src/a.rs")));
        assert_eq!(beta.abs_path(), Some("/w/src/a.rs"));
        let pos = beta.position.expect("positioned");
        assert_eq!((pos.line, pos.col), (2, 3));
        let dep = &list.entries[0];
        assert_eq!(
            dep.target.relative_parts(),
            None,
            "out-of-root symbols stay pathless (Label header downstream)"
        );
    }

    /// The Files picker captures whole-target entries: no position (the open lands on the
    /// buffer's last-known cursor, like selecting the row there) and no group, which is what
    /// makes the Jumplist picker render the capture flat.
    #[test]
    fn capture_snapshots_files_as_ungrouped_whole_file_entries() {
        use crate::picker::{make_matcher, PickerCandidates, PickerState};
        use crate::workspace_index::CachedFile;
        let file = |rel: &str| CachedFile {
            abs: format!("/w/{rel}"),
            path_index: 0,
            relative_path: rel.into(),
        };
        let picker = PickerState::new(PickerCandidates::Files {
            files: std::sync::Arc::new(vec![file("src/b.rs"), file("src/a.rs")]),
            git_status: std::sync::Arc::new(vec![None, None]),
        });
        let (list, _) = capture(&picker, &mut make_matcher()).expect("captures");
        assert!(!list.grouped, "a file-shaped source captures ungrouped");
        // Normalized into path order, not the picker's ranking.
        let rows: Vec<&str> = list.entries.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(rows, vec!["src/a.rs", "src/b.rs"]);
        for e in &list.entries {
            assert_eq!(e.position, None, "whole-file entries carry no position");
            assert_eq!(e.group, None, "and no group header");
        }
        assert_eq!(
            list.entries[0].target.relative_parts(),
            Some((0, "src/a.rs")),
            "the file identity is carried, so the dir/glob chips can scope it"
        );
    }

    /// The Buffers picker splits by identity: a file-backed buffer captures by *path* (durable,
    /// scopeable, survives the buffer closing), a scratch one by buffer id.
    #[test]
    fn capture_splits_buffers_into_file_and_buffer_targets() {
        use crate::picker::{make_matcher, BufferCandidate, PickerCandidates, PickerState};
        use aether_protocol::picker::BufferDirtyState;
        let picker = PickerState::new(PickerCandidates::Buffers(vec![
            BufferCandidate {
                buffer_id: 4,
                display: "src/a.rs".into(),
                status: BufferDirtyState::Clean,
                path: Some((0, "src/a.rs".into())),
                abs_path: Some("/w/src/a.rs".into()),
                transient: false,
            },
            BufferCandidate {
                buffer_id: 9,
                display: "(scratch 1)".into(),
                status: BufferDirtyState::Unsaved,
                path: None,
                abs_path: None,
                transient: false,
            },
        ]));
        let (list, _) = capture(&picker, &mut make_matcher()).expect("captures");
        assert!(!list.grouped);
        // Files sort ahead of pathless buffers (`TargetKey`), whatever the picker's MRU order.
        assert_eq!(
            list.entries[0].target,
            JumplistTarget::File {
                path_index: Some(0),
                relative_path: Some("src/a.rs".into()),
                abs_path: "/w/src/a.rs".into(),
            }
        );
        assert_eq!(
            list.entries[1].target,
            JumplistTarget::Buffer { buffer_id: 9 },
            "only the pathless row needs the buffer id"
        );
        assert_eq!(list.entries[1].display, "(scratch 1)");
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
            .map(|(_, e)| (e.abs_path().unwrap(), e.position.unwrap().line))
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
        let order: Vec<&str> = e.iter().map(|(_, e)| e.abs_path().unwrap()).collect();
        // "References" appeared first so it stays first; within it, files sort by path.
        assert_eq!(order, vec!["/a", "/z", "/m"]);
    }

    #[test]
    fn step_forward_within_file_then_falls_through_and_stops() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        // From /a:1 (on the first entry) → the next in-file entry.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(1, 0), 1),
            Some(1)
        );
        // Past /a's entries → the next file's first.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(5, 0), 1),
            Some(2)
        );
        // Past /b's entries → no wrap; stepping stops at the end.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/b"), pos(2, 0), 1),
            None
        );
    }

    #[test]
    fn step_backward_mirrors_and_stops() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/a"), pos(5, 0), 1),
            Some(0)
        );
        // Before /a's entries → no wrap; stepping stops at the start.
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/a"), pos(1, 0), 1),
            None
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/b"), pos(2, 0), 1),
            Some(1)
        );
    }

    #[test]
    fn step_virtually_inserts_files_not_in_the_list() {
        let e = vec![entry("/a", 1, 0), entry("/c", 2, 0)];
        // /b sits between /a and /c.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/b"), pos(0, 0), 1),
            Some(1)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/b"), pos(0, 0), 1),
            Some(0)
        );
        // Past either end (a file that sorts after / before every entry): no wrap, stop.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/z"), pos(0, 0), 1),
            None
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/A"), pos(0, 0), 1),
            None
        );
    }

    #[test]
    fn step_scratch_buffer_enters_the_list_at_the_ends() {
        let e = vec![entry("/a", 1, 0), entry("/b", 2, 0)];
        // A pathless buffer with nothing captured for it sorts past every file, so it enters the
        // list from outside rather than virtually inserting — always a move, either direction.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::Buffer(7), pos(0, 0), 1),
            Some(0)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::Buffer(7), pos(0, 0), 1),
            Some(1)
        );
    }

    /// A captured file list steps one file per press, from anywhere in the file: a whole-target
    /// entry is never "past" the cursor, so the walk always falls through to the neighbour.
    #[test]
    fn step_walks_a_whole_file_list_one_target_at_a_time() {
        let e = vec![whole_file("/a"), whole_file("/b"), whole_file("/c")];
        // Deep inside /b — the cursor position is irrelevant to a whole-target entry.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/b"), pos(40, 3), 1),
            Some(2)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/b"), pos(40, 3), 1),
            Some(0)
        );
        // Still stops at the ends rather than cycling.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/c"), pos(0, 0), 1),
            None
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/a"), pos(0, 0), 1),
            None
        );
        // And a count still advances and clamps.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 9),
            Some(2)
        );
    }

    /// A captured scratch buffer is a target like any other: stepping *into* it works by path
    /// order (it sorts after every file), and stepping out of it lands on its neighbour.
    #[test]
    fn step_walks_into_and_out_of_a_captured_scratch_buffer() {
        let e = vec![whole_file("/a"), scratch(9), scratch(12)];
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 1),
            Some(1)
        );
        // Sitting in scratch 9: forward to the next scratch, backward to the file.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::Buffer(9), pos(0, 0), 1),
            Some(2)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::Buffer(9), pos(0, 0), 1),
            Some(0)
        );
        // The last scratch is the end of the list.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::Buffer(12), pos(0, 0), 1),
            None
        );
    }

    /// `}`/`{` have nothing to walk in a file list — every entry is the whole file, so there is
    /// no interior. The client's `NoneInFile` toast points back at `]`.
    #[test]
    fn step_in_file_finds_nothing_to_walk_in_a_whole_file_list() {
        let e = vec![whole_file("/a"), whole_file("/b")];
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 1),
            InFileStep::NoneInFile
        );
        assert_eq!(
            step_in_file(&e, Direction::Backward, Location::File("/a"), pos(9, 0), 1),
            InFileStep::NoneInFile
        );
    }

    /// Reopening the picker (`Space j`) while inside a captured file frames *that* file's row,
    /// wherever the cursor sits in it — being in the buffer is being on the entry.
    #[test]
    fn nearest_index_frames_the_current_buffers_whole_target_entry() {
        let e = vec![whole_file("/a"), whole_file("/b"), scratch(9)];
        assert_eq!(nearest_index(&e, Location::File("/b"), pos(40, 3)), 1);
        assert_eq!(nearest_index(&e, Location::File("/a"), pos(0, 0)), 0);
        assert_eq!(nearest_index(&e, Location::Buffer(9), pos(0, 0)), 2);
        // A buffer that isn't in the list still frames the top, as before.
        assert_eq!(nearest_index(&e, Location::Buffer(4), pos(0, 0)), 0);
    }

    #[test]
    fn step_count_advances_and_clamps() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 2),
            Some(1)
        );
        // Count past the end clamps to the last entry (no wrap) — still a move.
        assert_eq!(
            step_index(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 4),
            Some(2)
        );
        assert_eq!(
            step_index(&e, Direction::Backward, Location::File("/b"), pos(2, 0), 2),
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
            step_index(&e, Direction::Forward, Location::File("/a"), pos(3, 4), 1),
            Some(2)
        );
        assert_eq!(spanned.start(), Some(pos(3, 4)));
    }

    #[test]
    fn step_in_file_walks_only_the_current_file_then_stops() {
        // /a has two entries, /b sits between them in list order (different file).
        let e = vec![entry("/a", 1, 0), entry("/b", 2, 0), entry("/a", 5, 0)];
        // Forward from the top of /a → its first entry, skipping /b entirely.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 1),
            InFileStep::Moved(0)
        );
        // From /a:1 → /a's next entry (index 2), still not /b.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(1, 0), 1),
            InFileStep::Moved(2)
        );
        // On /a's last entry → no fall-through to /b; stops at the file's end.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(5, 0), 1),
            InFileStep::AtEnd
        );
        // Backward from /a's last → its first; before the first → stop.
        assert_eq!(
            step_in_file(&e, Direction::Backward, Location::File("/a"), pos(5, 0), 1),
            InFileStep::Moved(0)
        );
        assert_eq!(
            step_in_file(&e, Direction::Backward, Location::File("/a"), pos(1, 0), 1),
            InFileStep::AtEnd
        );
    }

    #[test]
    fn step_in_file_reports_none_for_a_file_without_entries() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0)];
        // /b has no captured entries.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/b"), pos(0, 0), 1),
            InFileStep::NoneInFile
        );
        // A scratch buffer (no path) likewise has nothing in "its" file.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::Buffer(7), pos(0, 0), 1),
            InFileStep::NoneInFile
        );
    }

    #[test]
    fn step_in_file_count_clamps_within_the_file() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/a", 9, 0)];
        // Forward 2 from the top → the second entry (first-past-edge, then one more); a larger
        // count clamps to the last.
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 2),
            InFileStep::Moved(1)
        );
        assert_eq!(
            step_in_file(&e, Direction::Forward, Location::File("/a"), pos(0, 0), 9),
            InFileStep::Moved(2)
        );
        // Backward 9 from the last clamps to the first.
        assert_eq!(
            step_in_file(&e, Direction::Backward, Location::File("/a"), pos(9, 0), 9),
            InFileStep::Moved(0)
        );
    }

    #[test]
    fn nearest_index_is_inclusive_and_wraps() {
        let e = vec![entry("/a", 1, 0), entry("/a", 5, 0), entry("/b", 2, 0)];
        // Inclusive: sitting exactly on an entry's start resolves to that entry (unlike
        // stepping, which skips it).
        assert_eq!(nearest_index(&e, Location::File("/a"), pos(1, 0)), 0);
        // Between entries: the next one at-or-after.
        assert_eq!(nearest_index(&e, Location::File("/a"), pos(2, 0)), 1);
        // Past the file's entries: the entry after its last occurrence.
        assert_eq!(nearest_index(&e, Location::File("/a"), pos(9, 0)), 2);
        // Past everything: wraps to the first entry overall.
        assert_eq!(nearest_index(&e, Location::File("/b"), pos(9, 0)), 0);
        // File not in the list: virtual insertion by path; scratch buffers take the top.
        assert_eq!(nearest_index(&e, Location::File("/ab"), pos(0, 0)), 2);
        assert_eq!(nearest_index(&e, Location::Buffer(7), pos(0, 0)), 0);
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
            target: JumplistTarget::File {
                path_index: None,
                relative_path: None,
                abs_path: abs.into(),
            },
            position: Some(pos(3, 0)),
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
        assert_eq!(entries[0].target.relative_parts(), Some((0, "src/lib.rs")));
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
        // A captured file list is the ordinary multi-file case — chips apply.
        assert!(path_filterable(&[whole_file("/a"), whole_file("/b")]));
        // Pathless buffers are distinct targets but have nothing for a scope to match.
        assert!(!path_filterable(&[scratch(9), scratch(12)]));
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
        assert_eq!(entries[0].target.relative_parts(), Some((0, "a.rs")));
        // Out-of-workspace: no parts to derive (opens by absolute path), but grouping is total —
        // the collapsible picker keys every row — so the absolute path becomes the header.
        assert_eq!(entries[1].target.relative_parts(), None);

        assert_eq!(
            entries[1].group,
            Some(GroupHeader::Label {
                label: "/elsewhere/dep.rs".into(),
            })
        );
    }
}
