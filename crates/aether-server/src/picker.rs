//! Server-side picker state. One `PickerState` per `(ClientId, PickerKind)`; the server owns
//! the query, the ranked match list, and the subscribed window. The client owns the highlighted
//! row (it persists the last item locally and uses `view { center_on }` to restore on resume).
//!
//! Matching uses `nucleo_matcher` directly — sort once on query change, slice the window on
//! demand. No background ticking; for v1 the walk is the only slow step and that lives in
//! `WorkspaceIndex`. When the workspace grows enough to warrant streaming results during the
//! walk, switch to `nucleo::Nucleo` and a per-picker tick task.

use crate::workspace_index::CachedFile;
use aether_protocol::picker::{PickerItem, PickerKind, PickerUpdateParams};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::sync::Arc;

/// Per-picker server state. Held under the global `ServerState` lock.
pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    pub generation: u64,
    /// Indices into `candidates` in match-score order (descending). On empty query, this is
    /// `0..candidates.len()` in the candidates' own (alphabetical) order.
    pub ranked: Vec<u32>,
    /// The candidate snapshot `ranked` was computed against. Pinned here so `select` and
    /// `center_on` resolve against the same set the client most recently saw — even if the
    /// workspace index is later refreshed (file watcher, manual reload).
    pub candidates: Arc<Vec<CachedFile>>,
    /// `Some` while the client has the picker open and is receiving pushes. `None` after `hide`.
    pub subscribed: Option<SubscribedWindow>,
}

#[derive(Debug, Clone, Copy)]
pub struct SubscribedWindow {
    pub offset: u32,
    pub limit: u32,
}

impl PickerState {
    pub fn new(kind: PickerKind, candidates: Arc<Vec<CachedFile>>) -> Self {
        let ranked: Vec<u32> = (0..candidates.len() as u32).collect();
        Self { kind, query: String::new(), generation: 0, ranked, candidates, subscribed: None }
    }

    /// Wipe query and re-rank against the current candidate set. Used by `view { reset: true }`.
    pub fn reset(&mut self, candidates: Arc<Vec<CachedFile>>, matcher: &mut Matcher) {
        self.query.clear();
        self.generation = 0;
        self.candidates = candidates;
        self.rerank(matcher);
    }

    /// Recompute the ranked match list against the current candidates and query. Cheap for
    /// "small" workspaces (< ~50k files in benchmarks); revisit if we ever need to stream.
    pub fn rerank(&mut self, matcher: &mut Matcher) {
        self.ranked.clear();
        if self.query.is_empty() {
            self.ranked.extend(0..self.candidates.len() as u32);
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, u32)> = Vec::with_capacity(self.candidates.len());
        for (i, cand) in self.candidates.iter().enumerate() {
            let haystack = Utf32Str::new(&cand.display, &mut buf);
            if let Some(score) = pattern.score(haystack, matcher) {
                scored.push((score, i as u32));
            }
        }
        // Higher score first; on ties, fall back to candidate order so results are deterministic.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.ranked.extend(scored.into_iter().map(|(_, i)| i));
    }

    /// Locate a ranked index for `item` (used by `view { center_on }`). Returns `None` if the
    /// item is no longer present (file deleted, no longer matches the query, ...). Files match
    /// by `abs` path, which is stable across re-walks.
    pub fn rank_of(&self, item: &PickerItem) -> Option<u32> {
        match item {
            PickerItem::File { path: target_display, .. } => {
                // Match on display (project-relative) — that's what the client persisted. It's
                // sufficient because two distinct candidates can't share a display under our
                // walking rules (root-prefixed for multi-root, root-relative for single).
                self.ranked.iter().position(|&ci| {
                    self.candidates[ci as usize].display.as_str() == target_display.as_str()
                }).map(|p| p as u32)
            }
        }
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
            Some(Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart))
        } else {
            None
        };
        let mut buf = Vec::new();
        let mut items: Vec<PickerItem> = Vec::with_capacity((end - start) as usize);
        for &candidate_idx in &self.ranked[start as usize..end as usize] {
            let cand = &self.candidates[candidate_idx as usize];
            let match_indices: Vec<u32> = if let Some(pat) = pattern.as_ref() {
                let haystack = Utf32Str::new(&cand.display, &mut buf);
                let mut indices: Vec<u32> = Vec::new();
                pat.indices(haystack, matcher, &mut indices);
                indices.sort_unstable();
                indices.dedup();
                indices
            } else {
                Vec::new()
            };
            items.push(PickerItem::File { path: cand.display.clone(), match_indices });
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
pub fn build_update(
    state: &PickerState,
    matcher: &mut Matcher,
) -> Option<PickerUpdateParams> {
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

/// Convenience for `picker/select { Files }`: look up the canonical absolute path corresponding
/// to a `PickerItem::File`. Returns `None` if the item is no longer in the candidate set.
pub fn resolve_file_abs(state: &PickerState, item: &PickerItem) -> Option<String> {
    let PickerItem::File { path: display, .. } = item;
    state
        .candidates
        .iter()
        .find(|c| c.display == *display)
        .map(|c| c.abs.clone())
}

