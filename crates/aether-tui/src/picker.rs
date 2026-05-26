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
        true
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
    File(&'a str),
    Buffer(aether_protocol::BufferId),
    Grep { path: &'a str, line: u32, col: u32 },
    DirEntry(&'a str),
    Project(&'a str),
}

pub fn item_key(item: &PickerItem) -> ItemKey<'_> {
    match item {
        PickerItem::File { path, .. } => ItemKey::File(path.as_str()),
        PickerItem::Buffer { buffer_id, .. } => ItemKey::Buffer(*buffer_id),
        PickerItem::GrepHit {
            path, line, col, ..
        } => ItemKey::Grep {
            path: path.as_str(),
            line: *line,
            col: *col,
        },
        PickerItem::DirEntry { name, .. } => ItemKey::DirEntry(name.as_str()),
        PickerItem::Project { name, .. } => ItemKey::Project(name.as_str()),
    }
}
