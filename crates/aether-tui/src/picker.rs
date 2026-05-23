//! Client-side picker state. The server owns the candidate cache, query, and ranked list; the
//! client owns the highlighted row plus a small persisted slot (`last_selected`) used to restore
//! the highlight on `Space Alt-f` via `view { center_on }`.

use crate::app::Mode;
use aether_protocol::picker::{PickerItem, PickerKind};
use std::collections::HashMap;

/// In-flight picker UI state. `open` toggles the modal; when `false` all the other fields are
/// dormant carry-over from a prior session (we don't bother zeroing them — `Space f` resets the
/// server, and the next push will repopulate items).
#[derive(Debug, Default)]
pub struct PickerState {
    pub open: bool,
    pub kind: Option<PickerKind>,
    /// Mode the user was in when the picker was opened. `Esc` returns here. `Enter` always
    /// lands in `Normal` (via the selected file's buffer), regardless. Defaults to `Normal` —
    /// the field is only consulted when `open` is true.
    pub return_mode: Mode,
    pub query: String,
    /// Generation we minted on the most recent `picker/query`. Pushes carrying a different
    /// generation came from a stale query and must be ignored.
    pub generation: u64,
    /// Subscribed window. `offset` is what we last sent to the server; `limit` is how many rows
    /// the picker pane can display.
    pub offset: u32,
    pub limit: u32,
    /// Latest pushed slice. `items.len() <= limit`.
    pub items: Vec<PickerItem>,
    pub total_matches: u32,
    pub total_candidates: u32,
    pub ticking: bool,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
    /// When non-None, the item we're trying to re-anchor on after resume. Cleared once located
    /// in the pushed items (or once the user navigates, whichever comes first) — see
    /// `apply_update`.
    pub resume_target: Option<PickerItem>,
    /// Per-kind last-selected item, persisted across hide/show so `Space Alt-f` can resume the
    /// highlight (not just the listing). Lives outside `kind`-scoped fields above because it
    /// survives reset.
    pub last_selected: HashMap<PickerKind, PickerItem>,
}

impl PickerState {
    /// Apply a push from the server. Returns `true` if the push was for the current
    /// `(generation, offset)` and the UI should redraw.
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
        if offset != self.offset {
            return false;
        }
        self.items = items;
        self.total_matches = total_matches;
        self.total_candidates = total_candidates;
        self.ticking = ticking;

        // Resume anchoring: if we still owe the user a re-snap to last selection, try to find it
        // in the freshly-pushed items. If it's not present (matcher still ticking, or it's no
        // longer matched) we leave `resume_target` set so a later push can re-snap.
        if let Some(target) = self.resume_target.clone() {
            if let Some(idx) = self.items.iter().position(|i| item_key(i) == item_key(&target)) {
                self.selected = idx;
                self.resume_target = None;
            } else if self.selected >= self.items.len() {
                self.selected = 0;
            }
        } else if self.selected >= self.items.len() {
            // After a query change the result set may have shrunk past our last index. Clamp.
            self.selected = self.items.len().saturating_sub(1);
            if self.items.is_empty() {
                self.selected = 0;
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
/// pushed window after re-rank or resume.
pub fn item_key(item: &PickerItem) -> &str {
    match item {
        PickerItem::File { path, .. } => path.as_str(),
    }
}
