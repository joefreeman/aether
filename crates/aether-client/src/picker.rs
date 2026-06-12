//! Picker state — the platform-free half of the picker (docs/client-core.md): query and
//! generation staleness, selection and identity, chip/filter state, display-row math. The
//! rendering half lives in the shell (`src/picker.rs`).
//!
use crate::chips::{self, Chip, ChipEditor, ChipValue};
use aether_protocol::picker::{PickerFilters, PickerItem, PickerKind, PickerUpdateParams};

/// Rows the panel shows at once.
pub const VISIBLE_ROWS: usize = 18;
/// Window size requested from the server (over-fetched so small moves don't refetch).
pub const FETCH_LIMIT: u32 = 90;

/// The active buffer/project a fresh Buffers/Projects open should skip over when defaulting
/// its highlight (see [`PickerState::default_skip`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultSkip {
    Buffer(aether_protocol::BufferId),
    Project(String),
}

impl DefaultSkip {
    fn matches(&self, item: &PickerItem) -> bool {
        match (self, item) {
            (DefaultSkip::Buffer(id), PickerItem::Buffer { buffer_id, .. }) => id == buffer_id,
            (DefaultSkip::Project(active), PickerItem::Project { name, .. }) => active == name,
            _ => false,
        }
    }
}

/// How to scroll the highlight into view when the next update lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// Scroll the minimum to bring the row inside the viewport (keyboard moves).
    Minimal,
    /// Align the row to the top unless already visible (grep file-jumps — context below).
    Top,
}

pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    /// Byte cursor within `query`.
    pub cursor: usize,
    pub generation: u64,
    /// The fetched window starting at `offset` (absolute index into the match list).
    pub items: Vec<PickerItem>,
    pub offset: u32,
    /// Absolute index of the highlighted row.
    pub selected: u32,
    pub total_matches: u32,
    pub total_candidates: u32,
    pub ticking: bool,
    /// Display-row index of the fetched window's first row (grep: headers above included,
    /// from `grep_display_offset`; other kinds: equals `offset`). Sizes the top spacer.
    pub display_offset: u32,
    /// Total display rows in the whole result set (grep: hits + group headers). Sizes the
    /// virtual-scroll spacers.
    pub total_display_rows: u32,
    /// Item to re-highlight when the first matching update arrives (`center_on` echo).
    /// Matched by identity ([`item_key`]) — the listed item carries live decoration
    /// (git status, match indices) the anchor doesn't.
    pub pending_center: Option<PickerItem>,
    /// Fresh-open default highlight: land on the first item that *isn't* the client's active
    /// buffer/project, so Enter is a quick flip to the previous one. By identity, not "row 1"
    /// — the buffers MRU is shared across clients, so another client's activity can put any
    /// buffer at the top. One-shot: the first push with items decides and clears it.
    pub default_skip: Option<DefaultSkip>,
    /// Scroll the highlight into view when the next update lands (set by keyboard moves that
    /// forced a refetch and by centred opens — scroll-driven refetches must NOT yank the view).
    pub reveal_on_update: Option<Reveal>,
    /// The row under the pointer (underlined, web's hover affordance).
    pub hovered: Option<u32>,
    /// Explorer: the canonical directory being listed, echoed by `picker/view`.
    pub directory: Option<String>,
    /// Explorer: the listed directory's parent, when still inside the project boundary.
    pub directory_parent: Option<String>,
    /// The filter set in effect, stored as the ordered chip list — the client's single source
    /// of truth (docs/picker-filters.md). The wire `PickerFilters` is derived per send and
    /// converted back on open/resume; insertion order is session-ephemeral.
    pub chips: Vec<ChipValue>,
    /// Index into the chip row. While set, editing keys act on the chip (Enter edits,
    /// Backspace/Delete removes, Left/Right move). Entered via Left/Backspace at query
    /// cursor 0.
    pub chip_selected: Option<usize>,
    /// Below-input editor line for valued chips (glob / dir); owns all keys while open.
    pub chip_editor: Option<ChipEditor>,
}

impl PickerState {
    pub fn new(kind: PickerKind) -> Self {
        PickerState {
            kind,
            query: String::new(),
            cursor: 0,
            generation: 0,
            items: Vec::new(),
            offset: 0,
            selected: 0,
            total_matches: 0,
            total_candidates: 0,
            ticking: true,
            display_offset: 0,
            total_display_rows: 0,
            pending_center: None,
            default_skip: None,
            reveal_on_update: None,
            hovered: None,
            directory: None,
            directory_parent: None,
            chips: Vec::new(),
            chip_selected: None,
            chip_editor: None,
        }
    }

    /// The rendered chip row, derived from the stored list.
    pub fn chip_row(&self, project_paths: &[String]) -> Vec<Chip> {
        chips::derive_chips(&self.chips, project_paths)
    }

    /// The wire filter set the active chips fold into — built per send.
    pub fn wire_filters(&self) -> PickerFilters {
        chips::wire_filters(&self.chips)
    }

    /// Adopt a wire filter set (open/resume), replacing the chip list.
    pub fn adopt_filters(&mut self, f: &PickerFilters) {
        self.chips = chips::adopt_filters(f);
        self.chip_selected = None;
        self.chip_editor = None;
    }

    /// The dir scope behind chip `i`, when chip `i` is a dir — the editor's pre-fill.
    pub fn dir_value(&self, i: usize) -> Option<&aether_protocol::picker::ScopedPath> {
        match self.chips.get(i) {
            Some(ChipValue::Dir(d)) => Some(d),
            _ => None,
        }
    }

    /// The glob behind chip `i`, when chip `i` is a glob — the editor's pre-fill.
    pub fn glob_value(&self, i: usize) -> Option<&str> {
        match self.chips.get(i) {
            Some(ChipValue::Glob(g)) => Some(g.as_str()),
            _ => None,
        }
    }

    /// The highlighted item, when it's inside the fetched window.
    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.items
            .get(self.selected.saturating_sub(self.offset) as usize)
    }

    /// Apply a `picker/update` push. Stale pushes (older generation, other window) are
    /// discarded per the protocol. Returns false when discarded.
    pub fn apply_update(&mut self, u: PickerUpdateParams) -> bool {
        if u.kind != self.kind || u.generation != self.generation || u.offset != self.offset {
            return false;
        }
        self.items = u.items;
        self.total_matches = u.total_matches;
        self.total_candidates = u.total_candidates;
        self.ticking = u.ticking;
        self.display_offset = u.grep_display_offset.unwrap_or(u.offset);
        self.total_display_rows = u.grep_total_display_rows.unwrap_or(u.total_matches);
        if let Some(center) = self.pending_center.take() {
            let key = item_key(&center);
            if let Some(pos) = self.items.iter().position(|i| item_key(i) == key) {
                self.selected = self.offset + pos as u32;
            } else {
                self.pending_center = Some(center); // not in this window yet
            }
        } else if !self.items.is_empty() {
            if let Some(skip) = self.default_skip.take() {
                let pos = self
                    .items
                    .iter()
                    .position(|i| !skip.matches(i))
                    .unwrap_or(0); // every item is the active one (single open buffer)
                self.selected = self.offset + pos as u32;
            }
        }
        if self.total_matches > 0 {
            self.selected = self.selected.min(self.total_matches - 1);
        } else {
            self.selected = 0;
        }
        true
    }

    /// Move the highlight by `delta`, returning the new window offset to fetch when the
    /// highlight left the fetched window (the caller sends `picker/view`).
    pub fn move_selection(&mut self, delta: i64) -> Option<u32> {
        if self.total_matches == 0 {
            return None;
        }
        let max = self.total_matches as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, max) as u32;
        let in_window =
            self.selected >= self.offset && self.selected < self.offset + self.items.len() as u32;
        if in_window {
            return None;
        }
        self.reveal_on_update = Some(Reveal::Minimal);
        Some(self.selected.saturating_sub(FETCH_LIMIT / 2))
    }

    /// The fetched window as uniform display rows: group headers interleaved before each
    /// file's first hit (grep), every display row the same height (the shell's `ROW_H`).
    pub fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut rows = Vec::with_capacity(self.items.len() + 8);
        let mut last_file: Option<(u32, &str)> = None;
        for (i, item) in self.items.iter().enumerate() {
            if let PickerItem::GrepHit {
                path_index,
                relative_path,
                ..
            } = item
            {
                let f = (*path_index, relative_path.as_str());
                if last_file != Some(f) {
                    last_file = Some(f);
                    rows.push(DisplayRow::Header {
                        path_index: *path_index,
                        relative_path,
                    });
                }
            }
            rows.push(DisplayRow::Item {
                abs: self.offset + i as u32,
                item,
            });
        }
        rows
    }

    /// Display-row index where the rendered window's FIRST row sits in the whole virtual
    /// list. `display_offset` is the first *item*'s row; when the window leads with a group
    /// header, that header occupies the row just above (the server counted it there — or, for
    /// a mid-file window start, it stands in for the hit row the spacer would otherwise
    /// cover), so the window starts one row earlier.
    pub fn window_base(&self) -> u32 {
        let leads_with_header = self
            .items
            .first()
            .is_some_and(|i| matches!(i, PickerItem::GrepHit { .. }));
        self.display_offset.saturating_sub(leads_with_header as u32)
    }

    /// The highlighted item's display-row index in the whole virtual list, when it's inside
    /// the fetched window.
    pub fn selected_display_row(&self) -> Option<u32> {
        let base = self.window_base();
        let rows = self.display_rows();
        rows.iter()
            .position(|r| matches!(r, DisplayRow::Item { abs, .. } if *abs == self.selected))
            .map(|i| base + i as u32)
    }

    /// After a scroll that puts display row `first_visible` at the top of the list view:
    /// does the view need a re-fetched window? Returns the estimated item offset to request.
    /// Display rows ≈ items (headers are a minority), so the estimate maps display rows back
    /// to items proportionally; the server clamps. (The shell converts its scroll offset to
    /// a row index — the core doesn't know row heights.)
    pub fn scrolled_refetch(&self, first_visible: u32) -> Option<u32> {
        if self.items.is_empty() || self.total_display_rows == 0 {
            return None; // nothing fetched yet / refetch already in flight
        }
        let last_visible = first_visible + VISIBLE_ROWS as u32;
        let base = self.window_base();
        let window_rows = self.display_rows().len() as u32;
        let window_end = base + window_rows;
        let needs = first_visible < base
            || (last_visible > window_end && window_end < self.total_display_rows);
        if !needs {
            return None;
        }
        let ratio = self.total_matches as f32 / self.total_display_rows as f32;
        let est_item = (first_visible as f32 * ratio) as u32;
        Some(est_item.saturating_sub(FETCH_LIMIT / 2))
    }
}

/// Stable identity of a picker item, so centering anchors match the *live* listed item (which
/// carries decoration — git status, match indices — the anchor doesn't). Mirrors the TUI's
/// `item_key` / the web's `itemKey`.
#[derive(PartialEq)]
pub enum ItemKey<'a> {
    File(u32, &'a str),
    Buffer(aether_protocol::BufferId),
    Grep(u32, &'a str, u32, u32),
    Diagnostic(u32, u32),
    DirEntry(&'a str),
    Root(u32),
    Project(&'a str),
    LspServer(&'a str, &'a str),
    Reference(&'a str, u32, u32),
}

pub fn item_key(item: &PickerItem) -> ItemKey<'_> {
    match item {
        PickerItem::File {
            path_index,
            relative_path,
            ..
        } => ItemKey::File(*path_index, relative_path),
        PickerItem::Buffer { buffer_id, .. } => ItemKey::Buffer(*buffer_id),
        PickerItem::GrepHit {
            path_index,
            relative_path,
            line,
            col,
            ..
        } => ItemKey::Grep(*path_index, relative_path, *line, *col),
        PickerItem::Diagnostic { line, col, .. } => ItemKey::Diagnostic(*line, *col),
        PickerItem::DirEntry { name, .. } => ItemKey::DirEntry(name),
        PickerItem::Root { path_index, .. } => ItemKey::Root(*path_index),
        PickerItem::Project { name, .. } => ItemKey::Project(name),
        PickerItem::LspServer {
            language,
            workspace_root,
            ..
        } => ItemKey::LspServer(language, workspace_root),
        PickerItem::Reference {
            path, line, col, ..
        } => ItemKey::Reference(path, *line, *col),
    }
}

/// One uniform-height row of the rendered list.
pub enum DisplayRow<'a> {
    Header {
        path_index: u32,
        relative_path: &'a str,
    },
    Item {
        abs: u32,
        item: &'a PickerItem,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::git::GitStatus;

    fn update(
        kind: PickerKind,
        generation: u64,
        offset: u32,
        n: usize,
        total: u32,
    ) -> PickerUpdateParams {
        PickerUpdateParams {
            kind,
            generation,
            offset,
            items: (0..n)
                .map(|i| PickerItem::Project {
                    name: format!("p{i}"),
                    match_indices: vec![],
                })
                .collect(),
            total_matches: total,
            total_candidates: total,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }
    }

    #[test]
    fn updates_filter_stale_generations_and_windows() {
        let mut s = PickerState::new(PickerKind::Files);
        assert!(s.apply_update(update(PickerKind::Files, 0, 0, 5, 5)));
        assert_eq!(s.items.len(), 5);
        // Older generation / different window / different kind are discarded.
        s.generation = 2;
        assert!(!s.apply_update(update(PickerKind::Files, 1, 0, 9, 9)));
        assert!(!s.apply_update(update(PickerKind::Files, 2, 50, 9, 9)));
        assert!(!s.apply_update(update(PickerKind::Buffers, 2, 0, 9, 9)));
        assert_eq!(s.items.len(), 5);
    }

    #[test]
    fn selection_clamps_and_requests_refetch_outside_window() {
        let mut s = PickerState::new(PickerKind::Files);
        assert!(s.apply_update(update(PickerKind::Files, 0, 0, 90, 500)));
        // Moves within the fetched window need no refetch.
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 1);
        assert_eq!(s.move_selection(-10), None); // clamps at 0
        assert_eq!(s.selected, 0);
        // Jumping past the window requests a re-centred offset.
        s.selected = 89;
        let refetch = s.move_selection(1);
        assert_eq!(s.selected, 90);
        assert_eq!(refetch, Some(90 - FETCH_LIMIT / 2));
        // And the end clamps to the last match.
        s.selected = 499;
        assert!(s.move_selection(5).is_some());
        assert_eq!(s.selected, 499);
    }

    #[test]
    fn grep_display_rows_align_with_server_offsets() {
        let hit = |path: &str, line: u32| PickerItem::GrepHit {
            path_index: 0,
            relative_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::Grep);
        s.offset = 10;
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Grep,
            generation: 0,
            offset: 10,
            items: vec![hit("a.rs", 1), hit("a.rs", 2), hit("b.rs", 1)],
            // This window is the END of the result set (rows 13..18 of 18).
            total_matches: 13,
            total_candidates: 13,
            ticking: false,
            // The first item sits at display row 14; its group header occupies 13.
            grep_display_offset: Some(14),
            grep_total_display_rows: Some(18),
        }));
        // Window rows: [13]=hdr a.rs, [14]=hit, [15]=hit, [16]=hdr b.rs, [17]=hit.
        s.selected = 10;
        assert_eq!(s.selected_display_row(), Some(14));
        s.selected = 12;
        assert_eq!(s.selected_display_row(), Some(17));
        // Viewing the window's range needs no refetch (nothing exists below it); scrolling
        // above the fetched window does.
        assert_eq!(s.scrolled_refetch(13), None);
        assert!(s.scrolled_refetch(5).is_some());
    }

    #[test]
    fn default_skip_lands_on_first_non_active_item() {
        // Projects open: the highlight defaults past the active project — by identity, not
        // "row 1" (the active one needn't be first). One-shot: later pushes leave the
        // user's selection alone, and unwrap_or(0) covers "every item is the active one".
        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("aether".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 1, 1)));
        // Single item p0 ≠ "aether" → selected 0 and the skip is spent.
        assert_eq!(s.selected, 0);
        assert!(s.default_skip.is_none());

        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("p0".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 3, 3)));
        assert_eq!(s.selected, 1, "skips the active project at row 0");

        // Only the active project listed → fall back to row 0.
        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("p0".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 1, 1)));
        assert_eq!(s.selected, 0);
        assert!(s.default_skip.is_none());
    }

    #[test]
    fn pending_center_matches_by_identity_not_equality() {
        // The explorer's parent-ascend anchor is a bare DirEntry (no git status, no match
        // indices); the listed entry carries live decoration. Identity matching (by name)
        // must still land the highlight on it.
        let mut s = PickerState::new(PickerKind::Explorer);
        s.pending_center = Some(PickerItem::DirEntry {
            name: "src".into(),
            is_dir: true,
            match_indices: vec![],
            git_status: None,
        });
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Explorer,
            generation: 0,
            offset: 0,
            items: vec![
                PickerItem::DirEntry {
                    name: "docs".into(),
                    is_dir: true,
                    match_indices: vec![],
                    git_status: None,
                },
                PickerItem::DirEntry {
                    name: "src".into(),
                    is_dir: true,
                    match_indices: vec![],
                    git_status: Some(GitStatus::Modified), // decoration the anchor lacks
                },
            ],
            total_matches: 2,
            total_candidates: 2,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }));
        assert_eq!(s.selected, 1);
        assert!(s.pending_center.is_none());
    }

    #[test]
    fn pending_center_resolves_when_its_window_arrives() {
        let mut s = PickerState::new(PickerKind::Grep);
        s.pending_center = Some(PickerItem::Project {
            name: "p7".into(),
            match_indices: vec![],
        });
        assert!(s.apply_update(update(PickerKind::Grep, 0, 0, 10, 10)));
        assert_eq!(s.selected, 7);
        assert!(s.pending_center.is_none());
    }
}
