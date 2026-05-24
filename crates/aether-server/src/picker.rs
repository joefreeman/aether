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
use aether_protocol::BufferId;
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
}

impl PickerCandidates {
    pub fn len(&self) -> usize {
        match self {
            PickerCandidates::Files(v) => v.len(),
            PickerCandidates::Buffers(v) => v.len(),
        }
    }

    pub fn kind(&self) -> PickerKind {
        match self {
            PickerCandidates::Files(_) => PickerKind::Files,
            PickerCandidates::Buffers(_) => PickerKind::Buffers,
        }
    }

    /// Haystack string used for fuzzy matching at index `idx`.
    pub fn display_at(&self, idx: usize) -> &str {
        match self {
            PickerCandidates::Files(v) => &v[idx].display,
            PickerCandidates::Buffers(v) => &v[idx].display,
        }
    }

    /// Build the protocol-level `PickerItem` for candidate `idx` with the given match indices.
    pub fn make_item(&self, idx: usize, match_indices: Vec<u32>) -> PickerItem {
        match self {
            PickerCandidates::Files(v) => PickerItem::File {
                path: v[idx].display.clone(),
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
        }
    }

    /// Find a candidate by the stable identity of a `PickerItem`. Returns the candidate index.
    /// Used by `view { center_on }` and `select` to round-trip an item to its candidate slot.
    pub fn position_of(&self, item: &PickerItem) -> Option<usize> {
        match (self, item) {
            (PickerCandidates::Files(v), PickerItem::File { path, .. }) => {
                v.iter().position(|c| c.display == *path)
            }
            (PickerCandidates::Buffers(v), PickerItem::Buffer { buffer_id, .. }) => {
                v.iter().position(|c| c.buffer_id == *buffer_id)
            }
            _ => None,
        }
    }

    /// Produce the per-kind result of `picker/select` for candidate `idx`.
    pub fn select_result(&self, idx: usize) -> PickerSelectResult {
        match self {
            PickerCandidates::Files(v) => PickerSelectResult::File {
                path: v[idx].abs.clone(),
            },
            PickerCandidates::Buffers(v) => PickerSelectResult::Buffer {
                buffer_id: v[idx].buffer_id,
            },
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
        }
    }

    /// Recompute the ranked match list against the current candidates and query. Cheap for
    /// "small" workspaces (< ~50k files in benchmarks); revisit if we ever need to stream.
    pub fn rerank(&mut self, matcher: &mut Matcher) {
        self.ranked.clear();
        if self.query.is_empty() {
            // Empty query → preserve the candidate set's natural order. For Files that's
            // alphabetical (set by the walker); for Buffers it's MRU (set by the rebuild).
            self.ranked.extend(0..self.candidates.len() as u32);
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let n = self.candidates.len();
        let mut scored: Vec<(u32, u32)> = Vec::with_capacity(n);
        for i in 0..n {
            let haystack = Utf32Str::new(self.candidates.display_at(i), &mut buf);
            if let Some(score) = pattern.score(haystack, matcher) {
                scored.push((score, i as u32));
            }
        }
        // Higher score first; on ties, fall back to candidate order so results are deterministic.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.ranked.extend(scored.into_iter().map(|(_, i)| i));
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
        let pattern = if !self.query.is_empty() {
            Some(Pattern::parse(
                &self.query,
                CaseMatching::Smart,
                Normalization::Smart,
            ))
        } else {
            None
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

/// Resolve a `picker/select` item to its per-kind result. Returns `None` if the item is no
/// longer in the candidate set the picker last ranked against.
pub fn resolve_select(state: &PickerState, item: &PickerItem) -> Option<PickerSelectResult> {
    let idx = state.candidates.position_of(item)?;
    Some(state.candidates.select_result(idx))
}
