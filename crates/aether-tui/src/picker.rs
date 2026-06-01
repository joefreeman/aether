//! Client-side picker state. The server owns the candidate cache, query, and ranked list; the
//! client owns the highlighted row plus a small persisted slot (`last_selected`) used to restore
//! the highlight on reopen via `view { center_on }`.

use aether_protocol::picker::{PickerItem, PickerKind};
use std::collections::HashMap;

/// In-flight picker UI state. `open` toggles the overlay; when `false` all the other fields are
/// dormant carry-over from a prior session (we don't bother zeroing them — `Space f` resets the
/// server, and the next push will repopulate items). Opening/closing the picker doesn't change
/// which screen is underneath, so there's no "return mode" bookkeeping to do.
///
/// Cache layout: `items` is over-fetched — we ask the server for several pane-heights' worth so
/// most scrolls stay client-side. `visible_start` slides through that cache (without RPCs) to
/// pick the slice the renderer actually draws; `selected` is an index into `items` clamped to
/// keep the highlight inside the visible slice. We only round-trip when the visible window
/// approaches the cache edge — see `picker_move_selection` for the refetch trigger.
#[derive(Debug, Default)]
pub struct PickerState {
    pub open: bool,
    pub kind: Option<PickerKind>,
    pub query: crate::text_input::TextInput,
    /// Generation we minted on the most recent `picker/query`. Pushes carrying a different
    /// generation came from a stale query and must be ignored.
    pub generation: u64,
    /// Absolute index into the result set of `items[0]` (what we last asked the server for).
    pub offset: u32,
    /// How many items we asked the server for. Usually `pane_rows * PICKER_OVER_FETCH` — the
    /// over-fetch is what makes most scrolls local. The server may return fewer if we're near
    /// the end of the result set.
    pub limit: u32,
    /// Display rows in the picker pane. Used by the renderer (slice size) and the move handler
    /// (PageUp/Down delta, edge-of-cache prefetch threshold). Distinct from `limit` since
    /// `limit > pane_rows` under over-fetch.
    pub pane_rows: u32,
    /// Latest pushed slice. `items.len() <= limit`.
    pub items: Vec<PickerItem>,
    /// First index in `items` rendered by the picker pane. Slides forward / backward in response
    /// to selection moves to keep `selected` on-screen, all without an RPC. Refetch happens only
    /// when this approaches the edge of `items`.
    pub visible_start: usize,
    pub total_matches: u32,
    pub total_candidates: u32,
    pub ticking: bool,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
    /// When non-None, the item we're trying to re-anchor on after resume. Cleared once located
    /// in the pushed items (or once the user navigates, whichever comes first) — see
    /// `apply_update`.
    pub resume_target: Option<PickerItem>,
    /// Index offset of the highlight within the cache at the time of the last hide/select
    /// (`selected - visible_start`). When the resume target is found, `apply_update` positions
    /// `visible_start` so the highlight lands at the same row it was at when the picker closed.
    /// Lifecycle mirrors `resume_target`.
    pub resume_row_offset: Option<usize>,
    /// Per-kind last-selected item and its index offset within the cache, persisted across
    /// hide/show so reopening a picker can resume both the highlight and the scroll position.
    /// Lives outside `kind`-scoped fields above because it survives reset.
    pub last_selected: HashMap<PickerKind, (PickerItem, usize)>,
    /// Coalesced refetch target. `picker_move_selection` writes into this when the visible
    /// window approaches the cache edge; `flush_pending_picker_scroll` (once per draw cycle)
    /// fires a single `picker/view`. `apply_update` reconciles by accepting either `self.offset`
    /// or `pending_offset` and shifting `visible_start` / `selected` so the user's spot is
    /// preserved across the cache swap.
    pub pending_offset: Option<u32>,
    /// Explorer only. The canonical absolute path of the directory the picker is currently
    /// listing. Set by `open_picker(Explorer)` / `picker_navigate_to_dir` from the
    /// `PickerViewResult::directory_path` the server returns. Persisted across hide/show so the
    /// next `Space e` resumes in the same directory; `None` outside the Explorer picker.
    pub explorer_dir: Option<String>,
    /// Explorer only. The parent of `explorer_dir`, or `None` when the picker is at (or above)
    /// a project root (Alt-h is then a no-op). Carried alongside `explorer_dir` for the same
    /// reasons.
    pub explorer_parent: Option<String>,
    /// Projects-picker only. When `Some(idx)`, `items[idx]` is a *synthetic* row added
    /// client-side to offer "create a new project named <query>" — it isn't part of the
    /// server's candidate set. Selecting it routes through `project/create` instead of
    /// `picker/select`. `None` when no synthetic row is present (kind isn't Projects, query is
    /// empty, or an existing project matches the query exactly).
    pub synthetic_create_idx: Option<usize>,
}

impl PickerState {
    /// Apply a push from the server. Returns `true` if the push was for the current
    /// `(generation, offset)` and the UI should redraw. Accepts pushes whose offset equals
    /// either the currently-applied offset OR the pending offset (the result of an in-flight
    /// refetch); in the latter case it shifts `visible_start` and `selected` so the user's
    /// position in the result set is preserved across the cache swap.
    pub fn apply_update(
        &mut self,
        kind: PickerKind,
        generation: u64,
        offset: u32,
        items: Vec<PickerItem>,
        total_matches: u32,
        total_candidates: u32,
        ticking: bool,
    ) -> bool {
        if Some(kind) != self.kind {
            return false;
        }
        if generation != self.generation {
            return false;
        }
        let shift: i64 = if offset == self.offset {
            0
        } else if Some(offset) == self.pending_offset {
            let s = offset as i64 - self.offset as i64;
            self.offset = offset;
            self.pending_offset = None;
            s
        } else {
            return false;
        };
        self.items = items;
        // The server's push never contains our client-side synthetic row, so any cached
        // `synthetic_create_idx` is stale relative to the new `items` Vec. Drop it before the
        // recompute below decides whether to re-add — otherwise the strip-by-index logic could
        // remove a real entry at the same position.
        self.synthetic_create_idx = None;
        self.total_matches = total_matches;
        self.total_candidates = total_candidates;
        self.ticking = ticking;

        if shift != 0 {
            // Cache moved by `shift` in absolute coordinates → existing indices into the old
            // cache shift the other way to land on the same absolute items.
            self.visible_start = (self.visible_start as i64 - shift).max(0) as usize;
            self.selected = (self.selected as i64 - shift).max(0) as usize;
        }
        if self.items.is_empty() {
            self.selected = 0;
            self.visible_start = 0;
        } else {
            if self.selected >= self.items.len() {
                self.selected = self.items.len() - 1;
            }
            if self.visible_start >= self.items.len() {
                self.visible_start = self.items.len() - 1;
            }
        }

        // Resume anchoring: if we still owe the user a re-snap to last selection, try to find it
        // in the freshly-pushed items. If it's not present (matcher still ticking, or it's no
        // longer matched) we leave `resume_target` set so a later push can re-snap. When found,
        // also restore `visible_start` so the highlight lands at the same row-within-pane it was
        // at when the picker closed.
        if let Some(target) = self.resume_target.clone() {
            if let Some(idx) = self
                .items
                .iter()
                .position(|i| item_key(i) == item_key(&target))
            {
                self.selected = idx;
                if let Some(off) = self.resume_row_offset {
                    self.visible_start = idx.saturating_sub(off);
                }
                self.resume_target = None;
                self.resume_row_offset = None;
            }
        }
        self.recompute_synthetic_create_row();
        true
    }

    /// Recompute the synthetic "+ create" row. Called after any change to `items` or `query`.
    /// Appends a row labeled `+ create "<query>"` to `items` when the query is non-empty and
    /// doesn't exactly match an existing entry — Projects use it to create new projects, and
    /// Explorer uses it to create a new file at the current directory. Idempotent: strips any
    /// prior synthetic row first.
    pub fn recompute_synthetic_create_row(&mut self) {
        // Strip prior synthetic row if present.
        if let Some(idx) = self.synthetic_create_idx.take() {
            if idx < self.items.len() {
                self.items.remove(idx);
            }
        }
        let q = self.query.text.trim();
        if q.is_empty() {
            return;
        }
        let synthetic = match self.kind {
            Some(PickerKind::Projects) => {
                let already_exists = self.items.iter().any(|item| match item {
                    PickerItem::Project { name, .. } => name == q,
                    _ => false,
                });
                if already_exists {
                    return;
                }
                PickerItem::Project {
                    name: format!("+ create \"{q}\""),
                    match_indices: Vec::new(),
                }
            }
            Some(PickerKind::Explorer) => {
                // Don't offer "+ create" while a real entry exactly matches the typed name —
                // that's an existing file/dir the user can just Enter on. Slashes in `q` are
                // also rejected: file creation here is single-segment within the current dir
                // (mkdir-p flows live in save-as, not here).
                if q.contains('/') {
                    return;
                }
                let already_exists = self.items.iter().any(|item| match item {
                    PickerItem::DirEntry { name, .. } => name == q,
                    _ => false,
                });
                if already_exists {
                    return;
                }
                PickerItem::DirEntry {
                    name: format!("+ create \"{q}\""),
                    // Synthetic is always a leaf-file affordance: routing through buffer/open
                    // with create_if_missing creates the file, not a directory.
                    is_dir: false,
                    match_indices: Vec::new(),
                }
            }
            _ => return,
        };
        let idx = self.items.len();
        self.items.push(synthetic);
        self.synthetic_create_idx = Some(idx);
    }

    /// True if the highlighted row is the synthetic "create" row (the Projects picker's
    /// "create new project" affordance). The selector uses this to route to `project/create`
    /// instead of the normal `picker/select` flow.
    pub fn highlighted_is_synthetic_create(&self) -> bool {
        Some(self.selected) == self.synthetic_create_idx
    }

    /// The item currently under the highlight, if any.
    pub fn highlighted(&self) -> Option<&PickerItem> {
        self.items.get(self.selected)
    }
}

/// Stable identity for a picker item — used to find a previously-selected item in a freshly
/// pushed window after re-rank or resume. For files this is the path; for buffers it's the id
/// (stable across rename/Save-As, where the display string changes); for grep hits it's the
/// triple (path, line, col), which keeps a specific match identifiable across resume even if
/// the line text drifts after editing. For explorer entries it's the leaf name — valid only
/// inside one directory listing, which is exactly the lifetime resume needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ItemKey<'a> {
    File {
        path_index: u32,
        relative_path: &'a str,
    },
    Buffer(aether_protocol::BufferId),
    Grep {
        path_index: u32,
        relative_path: &'a str,
        line: u32,
        col: u32,
    },
    DirEntry(&'a str),
    Project(&'a str),
    Root { path_index: u32 },
}

pub fn item_key(item: &PickerItem) -> ItemKey<'_> {
    match item {
        PickerItem::File {
            path_index,
            relative_path,
            ..
        } => ItemKey::File {
            path_index: *path_index,
            relative_path: relative_path.as_str(),
        },
        PickerItem::Buffer { buffer_id, .. } => ItemKey::Buffer(*buffer_id),
        PickerItem::GrepHit {
            path_index,
            relative_path,
            line,
            col,
            ..
        } => ItemKey::Grep {
            path_index: *path_index,
            relative_path: relative_path.as_str(),
            line: *line,
            col: *col,
        },
        PickerItem::DirEntry { name, .. } => ItemKey::DirEntry(name.as_str()),
        PickerItem::Project { name, .. } => ItemKey::Project(name.as_str()),
        PickerItem::Root { path_index, .. } => ItemKey::Root {
            path_index: *path_index,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_input::TextInput;

    fn dir_entry(name: &str, is_dir: bool) -> PickerItem {
        PickerItem::DirEntry {
            name: name.into(),
            is_dir,
            match_indices: Vec::new(),
        }
    }

    fn empty_state(kind: PickerKind, query: &str) -> PickerState {
        let mut s = PickerState::default();
        s.open = true;
        s.kind = Some(kind);
        s.query = TextInput::new(query);
        s
    }

    #[test]
    fn explorer_synthetic_appears_when_query_has_no_exact_match() {
        let mut s = empty_state(PickerKind::Explorer, "newfile.rs");
        s.items = vec![dir_entry("README.md", false), dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 3);
        let last = s.items.last().unwrap();
        match last {
            PickerItem::DirEntry { name, is_dir, .. } => {
                assert_eq!(name, "+ create \"newfile.rs\"");
                assert!(!is_dir, "synthetic create row is always a file affordance");
            }
            other => panic!("expected DirEntry, got {other:?}"),
        }
        assert_eq!(s.synthetic_create_idx, Some(2));
    }

    #[test]
    fn explorer_synthetic_suppressed_when_exact_match_exists() {
        let mut s = empty_state(PickerKind::Explorer, "README.md");
        s.items = vec![dir_entry("README.md", false)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1, "no synthetic when entry already exists");
        assert!(s.synthetic_create_idx.is_none());
    }

    #[test]
    fn explorer_synthetic_suppressed_when_query_contains_slash() {
        // Cross-directory creation lives in save-as, not here — the picker is single-segment.
        let mut s = empty_state(PickerKind::Explorer, "subdir/newfile.rs");
        s.items = vec![dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1);
        assert!(s.synthetic_create_idx.is_none());
    }

    #[test]
    fn explorer_synthetic_suppressed_when_query_empty() {
        let mut s = empty_state(PickerKind::Explorer, "");
        s.items = vec![dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1);
        assert!(s.synthetic_create_idx.is_none());
    }

    #[test]
    fn recompute_strips_stale_synthetic_before_re_adding() {
        // Round 1: synthetic added.
        let mut s = empty_state(PickerKind::Explorer, "newfile.rs");
        s.items = vec![dir_entry("README.md", false)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 2);
        // Round 2: query changes, recompute. Old synthetic must be stripped before the new one
        // is appended — otherwise `items` grows unboundedly.
        s.query = TextInput::new("other.rs");
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 2);
        match s.items.last().unwrap() {
            PickerItem::DirEntry { name, .. } => assert_eq!(name, "+ create \"other.rs\""),
            other => panic!("expected DirEntry, got {other:?}"),
        }
    }

    #[test]
    fn apply_update_invalidates_stale_synthetic_idx() {
        // The pre-existing bug: after a wholesale items replacement (server push), the cached
        // synthetic_create_idx points into the old items vec. If a real item lands at that
        // position in the new vec, the strip-by-index logic would silently remove it.
        let mut s = empty_state(PickerKind::Explorer, "ne");
        s.items = vec![dir_entry("README.md", false)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.synthetic_create_idx, Some(1));
        // Simulate a fresh server push: send the same generation, offset, etc., with new
        // entries. `apply_update` must clear synthetic_create_idx so the recompute treats the
        // items as synthetic-free.
        s.generation = 7;
        let ok = s.apply_update(
            PickerKind::Explorer,
            7,
            0,
            vec![
                dir_entry("a.rs", false),
                dir_entry("b.rs", false),
                dir_entry("c.rs", false),
            ],
            3,
            3,
            false,
        );
        assert!(ok);
        // Items should be [a.rs, b.rs, c.rs, "+ create"] — the synthetic re-added without
        // having removed `b.rs` (which would happen if the stale idx=1 was used).
        assert_eq!(s.items.len(), 4);
        let names: Vec<&str> = s
            .items
            .iter()
            .map(|i| match i {
                PickerItem::DirEntry { name, .. } => name.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(names, vec!["a.rs", "b.rs", "c.rs", "+ create \"ne\""]);
    }
}
