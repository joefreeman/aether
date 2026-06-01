//! Server-side picker state. One `PickerState` per `(ClientId, PickerKind)`; the server owns
//! the query, the ranked match list, and the subscribed window. The client owns the highlighted
//! row (it persists the last item locally and uses `view { center_on }` to restore on resume).
//!
//! Matching uses `nucleo_matcher` directly — sort once on query change, slice the window on
//! demand. No background ticking; for v1 the walk is the only slow step and that lives in
//! `WorkspaceIndex`. When the workspace grows enough to warrant streaming results during the
//! walk, switch to `nucleo::Nucleo` and a per-picker tick task.

use crate::workspace_index::CachedFile;
use aether_protocol::picker::{PickerItem, PickerKind, PickerSelectResult, PickerUpdateParams};
use aether_protocol::{BufferId, LogicalPosition};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::sync::Arc;

/// One buffer-picker candidate. Built fresh per `picker/view` / `picker/query` from
/// `ServerState.buffers` + per-client MRU. The buffer set changes often enough that we don't
/// pin an `Arc` snapshot like the file picker does — just rebuild.
#[derive(Debug, Clone)]
pub struct BufferCandidate {
    pub buffer_id: BufferId,
    /// Display string used for both rendering and fuzzy matching. Project-relative for
    /// file-backed buffers; `[scratch N]` for scratch buffers.
    pub display: String,
    pub dirty: bool,
}

/// One project-picker candidate. Built fresh per `picker/view` from
/// `config::list_project_names()` — the configured-projects set changes only via the user
/// editing `~/.config/aether/projects/*.toml` and we re-list on each open anyway.
#[derive(Debug, Clone)]
pub struct ProjectCandidate {
    pub name: String,
}

/// One explorer-picker entry. Children of the picker's `current_path` directory; rebuilt by
/// each `picker/view` (Explorer always re-lists, like Buffers always rebuilds — directories
/// can change underneath us and there's no point caching them).
#[derive(Debug, Clone)]
pub struct ExplorerEntry {
    pub name: String,
    pub is_dir: bool,
}

/// The directory listing the explorer picker is currently matching against. `path` is the
/// canonical absolute path of the listing; `parent` is the parent's canonical path *if it's
/// still inside the project boundary* (otherwise `None`, meaning Alt-h is a no-op).
#[derive(Debug, Clone)]
pub struct ExplorerCandidates {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<ExplorerEntry>,
}

/// One grep-picker candidate. One per *match* (a line with N matches yields N candidates), in
/// the order ripgrep emitted them — walker order, then line order within each file.
#[derive(Debug, Clone)]
pub struct GrepHitCandidate {
    /// Index into the project's root list this file lives under.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`. Stored separately from `abs_path` so the picker can
    /// render without re-resolving against project roots on every push.
    pub relative_path: String,
    /// Absolute canonical path. Returned via `PickerSelectResult::FileAt` for the client to feed
    /// into `buffer/open`.
    pub abs_path: String,
    /// 0-based line number within the file.
    pub line: u32,
    /// 0-based byte offset of the match's first byte within the line.
    pub col: u32,
    /// Byte length of the match within the line. Needed alongside `col` so the server can
    /// reconstruct the match's end position for "is the cursor exactly on this match?" checks.
    pub match_byte_len: u32,
    /// The full text of the matching line (trailing newline trimmed). Used as the haystack for
    /// match-indices and as the preview shown in the picker row.
    pub preview: String,
    /// Char offsets into `preview` covered by the match.
    pub match_indices: Vec<u32>,
}

/// How a candidate set turns a non-empty query into a ranked subset. Each `PickerCandidates`
/// variant picks one; `rerank` and `build_window_items` dispatch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    /// Nucleo fuzzy match. Ranking is by score descending; match indices are the char
    /// positions nucleo highlighted. Used by Files and Buffers.
    Fuzzy,
    /// Smart-case prefix match. Natural candidate order preserved; match indices are the
    /// first N chars of the haystack (where N = query char count). Used by Explorer.
    PrefixSmartcase,
    /// No client-driven filter — the candidate set itself *is* the match set, in whatever
    /// order it was assembled. Used by Grep, where ripgrep filters server-side.
    Preserved,
}

/// The candidate set a `PickerState` is matching against. Per-kind variant keeps the candidate
/// data shape strict — selecting an item of the wrong kind out of a Files picker is a type
/// error, not a runtime branch.
#[derive(Debug, Clone)]
pub enum PickerCandidates {
    /// Workspace files. Shared `Arc` because the walk produces one snapshot per refresh and
    /// every picker that touches it borrows the same slice.
    Files(Arc<Vec<CachedFile>>),
    /// Open buffers in MRU order (most-recent first). Cheap to rebuild — small N, no I/O.
    Buffers(Vec<BufferCandidate>),
    /// Grep matches in walker + line order. Grows as the streaming search runs; rerank is a
    /// no-op (the query is the search, so the candidate set already *is* the match set).
    Grep(Vec<GrepHitCandidate>),
    /// Filesystem entries of the picker's current directory. Re-listed on every `picker/view`
    /// (directories can mutate underneath us; no point caching).
    Explorer(ExplorerCandidates),
    /// The project's roots, shown by the Explorer when the client requests Roots mode (via
    /// `picker/view { explorer_roots: true }`). One row per root; selecting one transitions the
    /// explorer back into `Explorer` mode at that root's top.
    ExplorerRoots(Vec<RootCandidate>),
    /// Configured project names. Re-listed on each `picker/view` — small N, no caching needed,
    /// and the user may have edited `~/.config/aether/projects/` between opens.
    Projects(Vec<ProjectCandidate>),
}

/// One row in the Explorer's Roots mode. `absolute_path` is what the client navigates to on
/// select; `basename` is the matcher haystack (the disambiguator the client shows alongside is
/// derived client-side from `path_index` + the project's root list).
#[derive(Debug, Clone)]
pub struct RootCandidate {
    pub path_index: u32,
    pub absolute_path: String,
    pub basename: String,
}

impl PickerCandidates {
    pub fn len(&self) -> usize {
        match self {
            PickerCandidates::Files(v) => v.len(),
            PickerCandidates::Buffers(v) => v.len(),
            PickerCandidates::Grep(v) => v.len(),
            PickerCandidates::Explorer(e) => e.entries.len(),
            PickerCandidates::ExplorerRoots(v) => v.len(),
            PickerCandidates::Projects(v) => v.len(),
        }
    }

    pub fn kind(&self) -> PickerKind {
        match self {
            PickerCandidates::Files(_) => PickerKind::Files,
            PickerCandidates::Buffers(_) => PickerKind::Buffers,
            PickerCandidates::Grep(_) => PickerKind::Grep,
            PickerCandidates::Explorer(_) => PickerKind::Explorer,
            PickerCandidates::ExplorerRoots(_) => PickerKind::Explorer,
            PickerCandidates::Projects(_) => PickerKind::Projects,
        }
    }

    /// Haystack string used for fuzzy matching at index `idx`. For Files this is the relative
    /// path alone — root identity is *not* part of the fuzzy match (the user disambiguates roots
    /// via the explorer's Roots mode, not the fuzzy filter). For Grep this is the preview but
    /// it's only consulted by the fuzzy matcher, which we skip for Grep.
    pub fn display_at(&self, idx: usize) -> &str {
        match self {
            PickerCandidates::Files(v) => &v[idx].relative_path,
            PickerCandidates::Buffers(v) => &v[idx].display,
            PickerCandidates::Grep(v) => &v[idx].preview,
            PickerCandidates::Explorer(e) => &e.entries[idx].name,
            PickerCandidates::ExplorerRoots(v) => &v[idx].basename,
            PickerCandidates::Projects(v) => &v[idx].name,
        }
    }

    /// Build the protocol-level `PickerItem` for candidate `idx`. `match_indices` is supplied by
    /// the fuzzy matcher for Files/Buffers/Explorer/Projects and ignored for Grep (the candidate
    /// already carries the ripgrep-computed match positions, which we use verbatim).
    pub fn make_item(&self, idx: usize, match_indices: Vec<u32>) -> PickerItem {
        match self {
            PickerCandidates::Files(v) => PickerItem::File {
                path_index: v[idx].path_index,
                relative_path: v[idx].relative_path.clone(),
                match_indices,
            },
            PickerCandidates::Buffers(v) => {
                let c = &v[idx];
                PickerItem::Buffer {
                    buffer_id: c.buffer_id,
                    display: c.display.clone(),
                    dirty: c.dirty,
                    match_indices,
                }
            }
            PickerCandidates::Grep(v) => {
                let c = &v[idx];
                PickerItem::GrepHit {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                    line: c.line,
                    col: c.col,
                    preview: c.preview.clone(),
                    match_indices: c.match_indices.clone(),
                }
            }
            PickerCandidates::Explorer(e) => {
                let entry = &e.entries[idx];
                PickerItem::DirEntry {
                    name: entry.name.clone(),
                    is_dir: entry.is_dir,
                    match_indices,
                }
            }
            PickerCandidates::ExplorerRoots(v) => PickerItem::Root {
                path_index: v[idx].path_index,
                match_indices,
            },
            PickerCandidates::Projects(v) => PickerItem::Project {
                name: v[idx].name.clone(),
                match_indices,
            },
        }
    }

    /// Find a candidate by the stable identity of a `PickerItem`. Returns the candidate index.
    /// Used by `view { center_on }` and `select` to round-trip an item to its candidate slot.
    pub fn position_of(&self, item: &PickerItem) -> Option<usize> {
        match (self, item) {
            (
                PickerCandidates::Files(v),
                PickerItem::File {
                    path_index,
                    relative_path,
                    ..
                },
            ) => v
                .iter()
                .position(|c| c.path_index == *path_index && c.relative_path == *relative_path),
            (PickerCandidates::Buffers(v), PickerItem::Buffer { buffer_id, .. }) => {
                v.iter().position(|c| c.buffer_id == *buffer_id)
            }
            (
                PickerCandidates::Grep(v),
                PickerItem::GrepHit {
                    path_index,
                    relative_path,
                    line,
                    col,
                    ..
                },
            ) => v.iter().position(|c| {
                c.path_index == *path_index
                    && c.relative_path == *relative_path
                    && c.line == *line
                    && c.col == *col
            }),
            (PickerCandidates::Explorer(e), PickerItem::DirEntry { name, .. }) => {
                e.entries.iter().position(|c| c.name == *name)
            }
            (PickerCandidates::ExplorerRoots(v), PickerItem::Root { path_index, .. }) => {
                v.iter().position(|c| c.path_index == *path_index)
            }
            (PickerCandidates::Projects(v), PickerItem::Project { name, .. }) => {
                v.iter().position(|c| c.name == *name)
            }
            _ => None,
        }
    }

    /// How the matcher should turn a non-empty query into a ranked subset for this candidate
    /// set. Centralises the per-variant decision so `rerank` and `build_window_items` can
    /// dispatch through one switch instead of scattered `matches!(..., Grep|Explorer)` checks.
    pub fn match_strategy(&self) -> MatchStrategy {
        match self {
            PickerCandidates::Files(_)
            | PickerCandidates::Buffers(_)
            | PickerCandidates::Projects(_) => MatchStrategy::Fuzzy,
            PickerCandidates::Explorer(_) | PickerCandidates::ExplorerRoots(_) => {
                MatchStrategy::PrefixSmartcase
            }
            // Grep's candidates *are* the matches (ripgrep already filtered + ordered them),
            // so query changes don't re-rank — they trigger a fresh walk elsewhere.
            PickerCandidates::Grep(_) => MatchStrategy::Preserved,
        }
    }

    /// Produce the per-kind result of `picker/select` for candidate `idx`. `None` when the item
    /// is not a "selectable" leaf — currently only the Explorer picker's directory entries,
    /// which the client should navigate into (via `picker/view`) instead of selecting.
    pub fn select_result(&self, idx: usize) -> Option<PickerSelectResult> {
        match self {
            PickerCandidates::Files(v) => Some(PickerSelectResult::File {
                path: v[idx].abs.clone(),
            }),
            PickerCandidates::Buffers(v) => Some(PickerSelectResult::Buffer {
                buffer_id: v[idx].buffer_id,
            }),
            PickerCandidates::Grep(v) => {
                let c = &v[idx];
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: LogicalPosition {
                        line: c.line,
                        col: c.col,
                    },
                })
            }
            PickerCandidates::Explorer(e) => {
                let entry = &e.entries[idx];
                if entry.is_dir {
                    None
                } else {
                    let abs = std::path::Path::new(&e.path)
                        .join(&entry.name)
                        .display()
                        .to_string();
                    Some(PickerSelectResult::File { path: abs })
                }
            }
            // Roots are always "navigate, don't select" — the client looks up the root's
            // absolute path from its project_paths and fires `picker/view` to enter it.
            PickerCandidates::ExplorerRoots(_) => None,
            PickerCandidates::Projects(v) => Some(PickerSelectResult::Project {
                name: v[idx].name.clone(),
            }),
        }
    }
}

/// Per-picker server state. Held under the global `ServerState` lock.
pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    pub generation: u64,
    /// Indices into `candidates` in match-score order (descending). On empty query, this is
    /// the candidate set's natural order — alphabetical for files, MRU for buffers.
    pub ranked: Vec<u32>,
    /// The candidate snapshot `ranked` was computed against. Pinned here so `select` and
    /// `center_on` resolve against the same set the client most recently saw — even if the
    /// underlying source (workspace index, buffer set) is later refreshed.
    pub candidates: PickerCandidates,
    /// `Some` while the client has the picker open and is receiving pushes. `None` after `hide`.
    pub subscribed: Option<SubscribedWindow>,
    /// Grep only: the query whose walk last completed (`ticking: false` push went out). When the
    /// next `picker/query` arrives with the same string, the candidates are still valid — skip
    /// the wipe + respawn and just re-emit the current window. Cleared whenever a new search
    /// starts; set by the streaming coordinator's final-push branch.
    pub last_completed_query: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SubscribedWindow {
    pub offset: u32,
    pub limit: u32,
}

impl PickerState {
    pub fn new(candidates: PickerCandidates) -> Self {
        let kind = candidates.kind();
        let ranked: Vec<u32> = (0..candidates.len() as u32).collect();
        Self {
            kind,
            query: String::new(),
            generation: 0,
            ranked,
            candidates,
            subscribed: None,
            last_completed_query: None,
        }
    }

    /// Recompute the ranked match list against the current candidates and query. Cheap for
    /// "small" workspaces (< ~50k files in benchmarks); revisit if we ever need to stream.
    pub fn rerank(&mut self, matcher: &mut Matcher) {
        self.ranked.clear();
        let strategy = self.candidates.match_strategy();
        // Two paths converge on "preserve natural order": Grep's strategy is always Preserved,
        // and the other strategies short-circuit to natural order on an empty query.
        if strategy == MatchStrategy::Preserved || self.query.is_empty() {
            self.ranked.extend(0..self.candidates.len() as u32);
            return;
        }
        match strategy {
            MatchStrategy::Fuzzy => {
                let pattern =
                    Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
                let mut buf = Vec::new();
                let n = self.candidates.len();
                let mut scored: Vec<(u32, u32)> = Vec::with_capacity(n);
                for i in 0..n {
                    let haystack = Utf32Str::new(self.candidates.display_at(i), &mut buf);
                    if let Some(score) = pattern.score(haystack, matcher) {
                        scored.push((score, i as u32));
                    }
                }
                // Higher score first; ties fall back to candidate order for determinism.
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                self.ranked.extend(scored.into_iter().map(|(_, i)| i));
            }
            MatchStrategy::PrefixSmartcase => {
                // Shell-tab-completion style: the typed query is a literal prefix of the entry
                // name. Natural candidate order preserved (dirs-then-files, alphabetical
                // within each, as the listing builder produced it).
                let (qc, case_insensitive) = smartcase_query(&self.query);
                let mut buf = String::new();
                for i in 0..self.candidates.len() {
                    let name = self.candidates.display_at(i);
                    let starts = if case_insensitive {
                        buf.clear();
                        buf.extend(name.chars().flat_map(char::to_lowercase));
                        buf.starts_with(qc.as_str())
                    } else {
                        name.starts_with(qc.as_str())
                    };
                    if starts {
                        self.ranked.push(i as u32);
                    }
                }
            }
            MatchStrategy::Preserved => unreachable!("handled above"),
        }
    }

    /// Locate a ranked index for `item` (used by `view { center_on }`). Returns `None` if the
    /// item is no longer present (file deleted, buffer closed, no longer matches the query, ...).
    pub fn rank_of(&self, item: &PickerItem) -> Option<u32> {
        let cand_idx = self.candidates.position_of(item)? as u32;
        self.ranked
            .iter()
            .position(|&ci| ci == cand_idx)
            .map(|p| p as u32)
    }

    /// Build the items + match indices for the subscribed window. Returns the slice items and
    /// the effective offset (clamped to ranked.len()).
    pub fn build_window_items(
        &self,
        offset: u32,
        limit: u32,
        matcher: &mut Matcher,
    ) -> (u32, Vec<PickerItem>) {
        let total = self.ranked.len() as u32;
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        // Match-indices source depends on the strategy: fuzzy → nucleo's `indices` helper;
        // prefix → the leading N chars of the name; preserved → none (Grep candidates carry
        // their own ripgrep-computed indices, applied inside `make_item`).
        let strategy = self.candidates.match_strategy();
        let query_active = !self.query.is_empty();
        let pattern = (query_active && strategy == MatchStrategy::Fuzzy)
            .then(|| Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart));
        let prefix_len: u32 = if query_active && strategy == MatchStrategy::PrefixSmartcase {
            self.query.chars().count() as u32
        } else {
            0
        };
        let mut buf = Vec::new();
        let mut items: Vec<PickerItem> = Vec::with_capacity((end - start) as usize);
        for &candidate_idx in &self.ranked[start as usize..end as usize] {
            let idx = candidate_idx as usize;
            let match_indices: Vec<u32> = if let Some(pat) = pattern.as_ref() {
                let haystack = Utf32Str::new(self.candidates.display_at(idx), &mut buf);
                let mut indices: Vec<u32> = Vec::new();
                pat.indices(haystack, matcher, &mut indices);
                indices.sort_unstable();
                indices.dedup();
                indices
            } else if prefix_len > 0 {
                let name_chars = self.candidates.display_at(idx).chars().count() as u32;
                (0..prefix_len.min(name_chars)).collect()
            } else {
                Vec::new()
            };
            items.push(self.candidates.make_item(idx, match_indices));
        }
        (start, items)
    }

    /// Total candidates the picker is matching against (whether matched or not).
    pub fn total_candidates(&self) -> u32 {
        self.candidates.len() as u32
    }
}

/// Construct a `PickerUpdateParams` for the current window. Mirrors `build_window_items` plus
/// the metadata fields. Caller is responsible for `generation` matching the latest query.
pub fn build_update(state: &PickerState, matcher: &mut Matcher) -> Option<PickerUpdateParams> {
    let window = state.subscribed?;
    let (offset, items) = state.build_window_items(window.offset, window.limit, matcher);
    Some(PickerUpdateParams {
        kind: state.kind,
        generation: state.generation,
        offset,
        items,
        total_matches: state.ranked.len() as u32,
        total_candidates: state.total_candidates(),
        ticking: false,
    })
}

/// Construct a `Matcher` with path-matching tuning. Called once and stored in `ServerState`;
/// callers borrow it mutably per picker operation.
pub fn make_matcher() -> Matcher {
    Matcher::new(Config::DEFAULT.match_paths())
}

/// Smartcase normalization for the Explorer picker's prefix matcher. Returns the query the
/// caller should compare against and whether comparisons need a lowercased haystack. The
/// query is lowercased iff it contains no uppercase letters — matching the convention nucleo
/// uses for the other pickers (`CaseMatching::Smart`).
fn smartcase_query(query: &str) -> (String, bool) {
    let has_upper = query.chars().any(|c| c.is_uppercase());
    if has_upper {
        (query.to_string(), false)
    } else {
        (query.chars().flat_map(char::to_lowercase).collect(), true)
    }
}

/// Resolve a `picker/select` item to its per-kind result. Returns `None` if the item is no
/// longer in the candidate set the picker last ranked against, *or* if the item exists but
/// isn't selectable (e.g. an Explorer directory entry — those navigate via `picker/view`).
pub fn resolve_select(state: &PickerState, item: &PickerItem) -> Option<PickerSelectResult> {
    let idx = state.candidates.position_of(item)?;
    state.candidates.select_result(idx)
}
