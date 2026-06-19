//! Picker state — the platform-free half of the picker (docs/client-core.md): query and
//! generation staleness, selection and identity, chip/filter state, display-row math. The
//! rendering half lives in the shell (`src/picker.rs`).
//!
use crate::chips::{self, Chip, ChipEditor, ChipEditorKind, ChipValue, DirListingState};
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
    /// The query value. Text editing (caret, insert, delete) is owned by each shell's input —
    /// native `text_input`/`<input>` in the rich clients, a shell-local editor in the TUI — which
    /// syncs the whole value via [`crate::update`]'s `picker_set_query`. The core keeps only the
    /// value plus the chip-row command gestures (`Left`/`Backspace` at the query start, etc.).
    pub query: String,
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
    /// Explorer: the committed *anchor* directory, echoed by `picker/view`. Navigation (Enter on a
    /// dir, Alt-h) moves it; typing a path query peeks relative to it without moving it (so
    /// backspace walks the peek back). The directory whose entries are actually shown is
    /// [`Self::explorer_listing_dir`] = this joined with the query's path part.
    pub directory: Option<String>,
    /// Explorer: the anchor's parent, when still inside the project boundary.
    pub directory_parent: Option<String>,
    /// Explorer: true when the query's peek directory (anchor + path part) doesn't exist — pushed
    /// by the server (the listing shows the peeked dir's *contents*, so the client can't tell on
    /// its own). Gates whether a trailing-slash query offers "+ Create directory".
    pub explorer_peek_missing: bool,
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
    /// The filter set the server is currently running results against — what was last sent on a
    /// `picker/query`. Lets the live-preview path (an open glob/dir editor folding its
    /// in-progress value into the filters) skip a redundant re-query when a keystroke leaves the
    /// effective filters unchanged, so focus moves and no-op edits don't blank + refetch.
    pub sent_filters: PickerFilters,
    /// Spinner animation frame, advanced once per applied push while `ticking` — so the throttled
    /// streaming-grep ticks (~16/s) drive the throbber without any client-side timer. See
    /// [`PickerState::spinner_glyph`].
    pub spinner_frame: u8,
}

/// Braille throbber frames for the "still searching" spinner (left of the picker's count).
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl PickerState {
    pub fn new(kind: PickerKind) -> Self {
        PickerState {
            kind,
            query: String::new(),
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
            explorer_peek_missing: false,
            chips: Vec::new(),
            chip_selected: None,
            chip_editor: None,
            sent_filters: PickerFilters::default(),
            spinner_frame: 0,
        }
    }

    /// The throbber glyph to show while a search is in progress (`ticking`), or `None` when settled.
    /// The frame advances per applied push (see [`apply_update`]), so it animates while results
    /// stream and stops the moment the search completes.
    pub fn spinner_glyph(&self) -> Option<&'static str> {
        self.ticking
            .then(|| SPINNER_FRAMES[self.spinner_frame as usize % SPINNER_FRAMES.len()])
    }

    /// The rendered chip row, derived from the stored list.
    pub fn chip_row(&self, project_paths: &[String]) -> Vec<Chip> {
        chips::derive_chips(&self.chips, project_paths)
    }

    /// The wire filter set the active chips fold into — built per send.
    pub fn wire_filters(&self) -> PickerFilters {
        chips::wire_filters(&self.chips)
    }

    /// The filter set to send *while a valued-chip editor is open*: the committed chips with the
    /// editor's in-progress glob/dir value folded in, so results update live as you type
    /// (docs/picker-filters.md). The in-progress value is exactly what `Enter` would commit
    /// ([`ChipEditor::preview_scope`] / [`chips::normalize_glob`]) — what-you-see-is-what-you-get.
    ///
    /// Returns `None` when the preview is *indeterminate* — a non-empty dir path whose suggestion
    /// listing is still loading — so the caller holds the current results rather than flapping
    /// them wider for a frame. An *invalid* in-progress value (a red segment) contributes nothing:
    /// results show as if the half-typed chip weren't there. With no editor open this is just the
    /// committed [`Self::wire_filters`].
    pub fn live_filters(&self, project_paths: &[String]) -> Option<PickerFilters> {
        let Some(ed) = &self.chip_editor else {
            return Some(self.wire_filters());
        };
        // Base = committed chips minus the one being edited; the in-progress value *replaces*
        // that chip's contribution rather than doubling it.
        let edit = ed.edit_index();
        let mut base: Vec<ChipValue> = self
            .chips
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != edit)
            .map(|(_, v)| v.clone())
            .collect();
        match ed.kind {
            ChipEditorKind::Glob { .. } => {
                if let Some(g) = chips::normalize_glob(&ed.input.text) {
                    base.push(ChipValue::Glob(g));
                }
            }
            ChipEditorKind::Dir { .. } => {
                // A non-empty path still listing: validity is unknown — hold, don't flap.
                if !ed.input.text.is_empty() && ed.listing_state == DirListingState::Pending {
                    return None;
                }
                if let Some(scope) = ed.preview_scope(project_paths) {
                    base.push(ChipValue::Dir(scope));
                }
            }
        }
        Some(chips::wire_filters(&base))
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

    /// True when the row at absolute index `abs` is a non-selectable *context* row — a filtered
    /// DocumentSymbols ancestor shown only for tree context. Only answerable within the fetched
    /// window (returns false otherwise, so out-of-window rows are treated as selectable).
    fn is_context_row(&self, abs: u32) -> bool {
        matches!(
            self.items.get(abs.saturating_sub(self.offset) as usize),
            Some(PickerItem::Symbol { context: true, .. })
        )
    }

    /// Nudge `selected` off a context row onto the nearest match — scanning `forward` first, then
    /// the other way. No-op unless the highlight is currently on a context row. A filtered symbol
    /// list always has at least one match alongside its ancestors, so this terminates on a match.
    fn skip_context_rows(&mut self, forward: bool, max: u32) {
        if !self.is_context_row(self.selected) {
            return;
        }
        for &rev in &[false, true] {
            let fwd = forward ^ rev; // primary pass, then the reverse
            let mut sel = self.selected;
            loop {
                if fwd {
                    if sel >= max {
                        break;
                    }
                    sel += 1;
                } else {
                    if sel == 0 {
                        break;
                    }
                    sel -= 1;
                }
                if !self.is_context_row(sel) {
                    self.selected = sel;
                    return;
                }
            }
        }
    }

    /// The synthetic "+ Create …" affordance: present when the (trimmed) query names something
    /// the listing doesn't already contain. Two pickers offer it:
    ///
    /// - **Explorer**: a file (or a directory, when the query ends with `/`) under the current
    ///   directory. Selecting it runs `explorer_create_from_query`.
    /// - **Projects**: a fresh project by that name. Selecting it runs `project_create_from_query`.
    ///   `is_dir` is irrelevant for projects (always `false`); names with path separators are
    ///   rejected (the server forbids them too).
    ///
    /// Returns `None` for any other kind, an empty/invalid name, or when a listed entry already
    /// matches it exactly — so the row appears the moment you type a novel name and vanishes again
    /// once the listing contains it.
    pub fn pending_create(&self) -> Option<PendingCreate> {
        match self.kind {
            PickerKind::Explorer => self.explorer_pending_create(),
            PickerKind::Projects => self.project_pending_create(),
            _ => None,
        }
    }

    /// The directory whose entries the Explorer is currently showing: the committed anchor
    /// ([`Self::directory`]) descended by the query's path part (everything up to the last `/`).
    /// Rows the user sees live here, so Enter-into-a-dir and delete resolve a leaf name against it
    /// (whereas "+ Create" joins the *whole* query to the anchor, creating intermediates). `None`
    /// only when there's no anchor yet (no Explorer view has landed).
    pub fn explorer_listing_dir(&self) -> Option<String> {
        let dir = self.directory.as_deref()?;
        let (path_part, _filter) = explorer_query_split(&self.query);
        if path_part.is_empty() {
            Some(dir.to_string())
        } else {
            Some(format!("{}/{}", dir.trim_end_matches('/'), path_part))
        }
    }

    /// Tab-completion ghost for the Explorer input: the longest common prefix shared by *all*
    /// currently-matched entries, beyond what the query's filter part already spells (so the ghost
    /// is what `Tab` would append). `None` for non-Explorer kinds, when not every match is in hand
    /// (the filtered listing overflows the fetched window, so a hidden entry could break the
    /// prefix), or when there's nothing left to add. An empty filter in a directory whose entries
    /// all share a prefix still suggests it.
    pub fn explorer_completion(&self) -> Option<String> {
        if self.kind != PickerKind::Explorer {
            return None;
        }
        // Only safe when the window holds every match — otherwise the "common" prefix is over a
        // subset and could run longer than the true one.
        if self.items.is_empty() || self.items.len() as u32 != self.total_matches {
            return None;
        }
        let filter_len = explorer_query_split(&self.query).1.chars().count();
        let mut names = self.items.iter().filter_map(|it| match it {
            PickerItem::DirEntry { name, .. } => Some(name.as_str()),
            _ => None,
        });
        let first = names.next()?;
        // Longest common prefix length (in chars) across all matched names.
        let mut lcp_len = first.chars().count();
        for name in names {
            let common = first
                .chars()
                .zip(name.chars())
                .take_while(|(a, b)| a == b)
                .count();
            lcp_len = lcp_len.min(common);
            if lcp_len <= filter_len {
                return None;
            }
        }
        (lcp_len > filter_len).then(|| {
            first
                .chars()
                .skip(filter_len)
                .take(lcp_len - filter_len)
                .collect()
        })
    }

    fn explorer_pending_create(&self) -> Option<PendingCreate> {
        let q = self.query.trim();
        let (base, is_dir) = match q.strip_suffix('/') {
            Some(stripped) => (stripped, true),
            None => (q, false),
        };
        if base.is_empty()
            || base
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return None;
        }
        // Suppress when the named thing already exists, keyed off the peek listing (the directory
        // the query descended into):
        //  - File (no trailing `/`): the listing *is* the leaf's parent (peek = anchor + path
        //    part), so suppress when the leaf is already there — you'd open it. `b/c` checks `c`
        //    against the entries of `b`.
        //  - Directory (trailing `/`): the query peeks *into* the named dir, so its existence can't
        //    be read off the listing (that's the dir's *contents*). The server tells us via
        //    `explorer_peek_missing`; offer create only when the peeked dir is actually missing.
        // Case-sensitive throughout: `Foo` and `foo` are distinct.
        let suppress = if is_dir {
            !self.explorer_peek_missing
        } else {
            let leaf = base.rsplit('/').next().unwrap_or(base);
            self.items
                .iter()
                .any(|it| matches!(it, PickerItem::DirEntry { name, .. } if name == leaf))
        };
        if suppress {
            return None;
        }
        Some(PendingCreate {
            name: base.to_string(),
            is_dir,
        })
    }

    fn project_pending_create(&self) -> Option<PendingCreate> {
        let name = self.query.trim();
        // Project names must be a single non-empty segment (the server stores them as a TOML file
        // stem and refuses path separators).
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return None;
        }
        // Suppress when a listed project already carries this exact name (Enter would activate it).
        // Case-sensitive, matching the file-stem identity.
        let exact = self
            .items
            .iter()
            .any(|it| matches!(it, PickerItem::Project { name: n, .. } if n == name));
        if exact {
            return None;
        }
        Some(PendingCreate {
            name: name.to_string(),
            is_dir: false,
        })
    }

    /// Absolute selection index the create row occupies — one past the final match.
    pub fn create_row_index(&self) -> Option<u32> {
        self.pending_create().map(|_| self.total_matches)
    }

    /// Is the synthetic create row the highlighted row?
    pub fn selected_is_create(&self) -> bool {
        self.create_row_index() == Some(self.selected)
    }

    /// Apply a `picker/update` push. Stale pushes (older generation, other window) are
    /// discarded per the protocol. Returns false when discarded.
    pub fn apply_update(&mut self, u: PickerUpdateParams) -> bool {
        if u.kind != self.kind || u.generation != self.generation || u.offset != self.offset {
            return false;
        }
        // `None` is a throttled count-only tick (streaming grep): keep the current window, update
        // the counts. `Some` replaces it (an empty vec is a genuinely empty result set).
        if let Some(items) = u.items {
            self.items = items;
        }
        self.total_matches = u.total_matches;
        self.total_candidates = u.total_candidates;
        self.ticking = u.ticking;
        self.explorer_peek_missing = u.explorer_peek_missing;
        // Advance the throbber each applied push while a search is still running.
        if u.ticking {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
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
        // The create row (Explorer) adds one selectable slot past the matches; keep the highlight
        // within `[0, total_matches]` when it's live, otherwise `[0, total_matches - 1]`.
        let rows = self.total_matches + self.create_row_index().is_some() as u32;
        if rows > 0 {
            self.selected = self.selected.min(rows - 1);
        } else {
            self.selected = 0;
        }
        // Filtered DocumentSymbols interleave non-selectable ancestor rows for context — never let
        // the highlight settle on one (e.g. when it lands on row 0 and that's an ancestor header).
        self.skip_context_rows(true, rows.saturating_sub(1));
        true
    }

    /// Move the highlight by `delta`, returning the new window offset to fetch when the
    /// highlight left the fetched window (the caller sends `picker/view`).
    pub fn move_selection(&mut self, delta: i64) -> Option<u32> {
        // The synthetic create row (Explorer) is one extra selectable row past the last match.
        let create = self.create_row_index();
        let rows = self.total_matches + create.is_some() as u32;
        if rows == 0 {
            return None;
        }
        let max = rows as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, max) as u32;
        // Skip over non-selectable context rows (filtered DocumentSymbols ancestors) in the move's
        // direction, so `j`/`k` land only on matches.
        self.skip_context_rows(delta >= 0, max as u32);
        // The create row is virtual — never in the fetched item window, so it can't force a
        // refetch; the move onto the row below it already brought the list's tail into view.
        if create == Some(self.selected) {
            self.reveal_on_update = Some(Reveal::Minimal);
            return None;
        }
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
            // Grep hits and Git changes both group by file: emit one header before each file's
            // first row.
            if let PickerItem::GrepHit {
                path_index,
                relative_path,
                ..
            }
            | PickerItem::GitChange {
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
        // The Explorer's "+ Create …" affordance trails the final match. Only emit it once the
        // window reaches the list's end (its absolute row, `total_matches`, sits just past the last
        // item) — for a mid-list window it isn't adjacent and would render in the wrong place.
        if let Some(pc) = self.pending_create() {
            if self.offset + self.items.len() as u32 >= self.total_matches {
                rows.push(DisplayRow::Create {
                    abs: self.total_matches,
                    name: pc.name,
                    is_dir: pc.is_dir,
                });
            }
        }
        rows
    }

    /// Display-row index where the rendered window's FIRST row sits in the whole virtual
    /// list. `display_offset` is the first *item*'s row; when the window leads with a group
    /// header, that header occupies the row just above (the server counted it there — or, for
    /// a mid-file window start, it stands in for the hit row the spacer would otherwise
    /// cover), so the window starts one row earlier.
    pub fn window_base(&self) -> u32 {
        let leads_with_header = self.items.first().is_some_and(|i| {
            matches!(i, PickerItem::GrepHit { .. } | PickerItem::GitChange { .. })
        });
        self.display_offset.saturating_sub(leads_with_header as u32)
    }

    /// The highlighted item's display-row index in the whole virtual list, when it's inside
    /// the fetched window.
    pub fn selected_display_row(&self) -> Option<u32> {
        let base = self.window_base();
        let rows = self.display_rows();
        rows.iter()
            .position(|r| match r {
                DisplayRow::Item { abs, .. } | DisplayRow::Create { abs, .. } => {
                    *abs == self.selected
                }
                DisplayRow::Header { .. } => false,
            })
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
    GitChange(u32, &'a str, u32),
    Diagnostic(u32, u32),
    DirEntry(&'a str),
    Root(u32),
    Project(&'a str),
    LspServer(&'a str, &'a str),
    Reference(&'a str, u32, u32),
    Symbol(&'a str, u32, u32),
}

/// Split an Explorer path-query into `(path_part, filter_part)` at the last `/`, mirroring the
/// server's `explorer_query_split`. The path part (no trailing slash) is the peek directory under
/// the anchor; the filter part prefix-matches its entries. No `/` → the whole query is the filter.
pub fn explorer_query_split(query: &str) -> (&str, &str) {
    match query.rfind('/') {
        Some(i) => (&query[..i], &query[i + 1..]),
        None => ("", query),
    }
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
        PickerItem::GitChange {
            path_index,
            relative_path,
            hunk_index,
            ..
        } => ItemKey::GitChange(*path_index, relative_path, *hunk_index),
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
        PickerItem::Symbol {
            path, line, col, ..
        } => ItemKey::Symbol(path, *line, *col),
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
    /// The Explorer's synthetic "+ Create …" action row (see [`PickerState::pending_create`]).
    /// `abs` is its selection index; selecting it creates `name` (a directory when `is_dir`).
    Create {
        abs: u32,
        name: String,
        is_dir: bool,
    },
}

/// The Explorer's pending create affordance — the name a "+ Create …" row would create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCreate {
    /// The leaf/relative name to create (no trailing `/`, validated non-empty).
    pub name: String,
    /// `true` when the query ended with `/` — create a directory rather than a file.
    pub is_dir: bool,
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
            items: Some(
                (0..n)
                    .map(|i| PickerItem::Project {
                        name: format!("p{i}"),
                        match_indices: vec![],
                    })
                    .collect(),
            ),
            total_matches: total,
            total_candidates: total,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
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
    fn count_only_update_keeps_items_and_advances_counts() {
        // A streaming grep: the window fills, then the server sends throttled count-only ticks
        // (`items: None`) as the candidate count climbs.
        let mut s = PickerState::new(PickerKind::Grep);
        assert!(s.apply_update(update(PickerKind::Grep, 0, 0, 5, 64)));
        assert_eq!(s.items.len(), 5);
        assert_eq!(s.total_matches, 64);
        // Count-only tick: `items: None` → keep the window, bump the counts.
        let mut tick = update(PickerKind::Grep, 0, 0, 0, 128);
        tick.items = None;
        tick.total_candidates = 9000;
        tick.ticking = true;
        assert!(s.apply_update(tick));
        assert_eq!(s.items.len(), 5, "count-only tick must not wipe the window");
        assert_eq!(s.total_matches, 128);
        assert_eq!(s.total_candidates, 9000);
        assert!(s.ticking);
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
            items: Some(vec![hit("a.rs", 1), hit("a.rs", 2), hit("b.rs", 1)]),
            // This window is the END of the result set (rows 13..18 of 18).
            total_matches: 13,
            total_candidates: 13,
            ticking: false,
            // The first item sits at display row 14; its group header occupies 13.
            grep_display_offset: Some(14),
            grep_total_display_rows: Some(18),
            center_on: None,
            explorer_peek_missing: false,
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
            items: Some(vec![
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
            ]),
            total_matches: 2,
            total_candidates: 2,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        assert_eq!(s.selected, 1);
        assert!(s.pending_center.is_none());
    }

    #[test]
    fn symbol_selection_skips_context_rows() {
        use aether_protocol::picker::SymbolKind;
        let sym = |name: &str, context: bool| PickerItem::Symbol {
            path: "/a".into(),
            line: 0,
            col: 0,
            name: name.into(),
            symbol_kind: SymbolKind::Struct,
            detail: String::new(),
            depth: 0,
            context,
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::DocumentSymbols);
        // [ctx Widget, match parse, ctx Token, match value] — ancestors interleaved with matches.
        let mut u = update(PickerKind::DocumentSymbols, 0, 0, 0, 4);
        u.items = Some(vec![
            sym("Widget", true),
            sym("parse", false),
            sym("Token", true),
            sym("value", false),
        ]);
        assert!(s.apply_update(u));
        // The leading context row (Widget) is skipped — the highlight lands on the first match.
        assert_eq!(s.selected, 1);
        // Down skips the context row (Token) onto the next match.
        s.move_selection(1);
        assert_eq!(s.selected, 3);
        // Up skips back over Token onto the previous match.
        s.move_selection(-1);
        assert_eq!(s.selected, 1);
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

    /// An Explorer window listing the given entry names (all files), with `total_matches` equal to
    /// the number of names (the whole directory fits the window).
    fn explorer_with(names: &[&str]) -> PickerState {
        let mut s = PickerState::new(PickerKind::Explorer);
        s.directory = Some("/proj/src".into());
        s.apply_update(PickerUpdateParams {
            kind: PickerKind::Explorer,
            generation: 0,
            offset: 0,
            items: Some(
                names
                    .iter()
                    .map(|n| PickerItem::DirEntry {
                        name: (*n).into(),
                        is_dir: false,
                        match_indices: vec![],
                        git_status: None,
                    })
                    .collect(),
            ),
            total_matches: names.len() as u32,
            total_candidates: names.len() as u32,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
            center_on: None,
            explorer_peek_missing: false,
        });
        s
    }

    #[test]
    fn pending_create_appears_for_a_novel_name_and_hides_on_exact_match() {
        let mut s = explorer_with(&["main.rs", "lib.rs"]);
        // No query: nothing to create.
        assert_eq!(s.pending_create(), None);
        // A name that isn't listed: offer to create a file.
        s.query = "new.rs".into();
        assert_eq!(
            s.pending_create(),
            Some(PendingCreate {
                name: "new.rs".into(),
                is_dir: false
            })
        );
        // A name that exactly matches an existing entry: no create offered (you'd open it).
        s.query = "lib.rs".into();
        assert_eq!(s.pending_create(), None);
        // Trailing slash peeks into a directory: when it exists (server says not missing) no create
        // is offered, regardless of whether its listing is empty…
        s.query = "sub/".into();
        s.explorer_peek_missing = false;
        assert_eq!(s.pending_create(), None);
        // …but when the peeked dir is missing, offer to create it.
        s.explorer_peek_missing = true;
        assert_eq!(
            s.pending_create(),
            Some(PendingCreate {
                name: "sub".into(),
                is_dir: true
            })
        );
        // Empty / dot segments are never creatable.
        for bad in ["", "   ", ".", "..", "a//b", "./x"] {
            s.query = bad.into();
            assert_eq!(s.pending_create(), None, "{bad:?} should not be creatable");
        }
        // Outside the Explorer, never offered.
        s.kind = PickerKind::Files;
        s.query = "new.rs".into();
        assert_eq!(s.pending_create(), None);
    }

    #[test]
    fn explorer_completion_suggests_common_prefix_beyond_the_query() {
        // A single match → complete the rest of its name.
        let mut s = explorer_with(&["crates"]);
        s.query = "cra".into();
        assert_eq!(s.explorer_completion().as_deref(), Some("tes"));

        // Several matches sharing a prefix, empty query → suggest the whole shared prefix.
        let mut s = explorer_with(&["aether-protocol", "aether-server", "aether-tui"]);
        s.query = "".into();
        assert_eq!(s.explorer_completion().as_deref(), Some("aether-"));
        // Once the query reaches the shared prefix, the entries diverge → nothing to add.
        s.query = "aether-".into();
        assert_eq!(s.explorer_completion(), None);
        // Partway in, still suggests the remainder up to the divergence.
        s.query = "aet".into();
        assert_eq!(s.explorer_completion().as_deref(), Some("her-"));
    }

    #[test]
    fn explorer_completion_holds_off_until_all_matches_are_in_hand() {
        // A windowed listing (more matches than rows shown) can't prove a common prefix.
        let mut s = explorer_with(&["aether-a", "aether-b"]);
        s.query = "".into();
        s.total_matches = 5; // two shown, five total → don't guess
        assert_eq!(s.explorer_completion(), None);
        // No matches at all → nothing.
        let mut s = explorer_with(&[]);
        s.query = "zzz".into();
        assert_eq!(s.explorer_completion(), None);
        // Not the Explorer → never.
        let mut s = explorer_with(&["aaa", "aab"]);
        s.kind = PickerKind::Files;
        assert_eq!(s.explorer_completion(), None);
    }

    #[test]
    fn explorer_completion_respects_the_query_path_part() {
        // The completion applies to the filter part (after the last `/`), not the whole query:
        // entries `alpha`/`alps` share `alp`, and with filter `al` the suffix is just `p`.
        let mut s = explorer_with(&["alpha", "alps"]);
        s.query = "src/al".into();
        assert_eq!(s.explorer_completion().as_deref(), Some("p"));
    }

    #[test]
    fn explorer_listing_dir_descends_by_query_path_part() {
        let mut s = explorer_with(&["main.rs"]);
        s.directory = Some("/proj/a".into());
        // No path part → the anchor itself.
        s.query = "ma".into();
        assert_eq!(s.explorer_listing_dir().as_deref(), Some("/proj/a"));
        // A path part descends; the filter part (after the last `/`) is not part of the dir.
        s.query = "b/ma".into();
        assert_eq!(s.explorer_listing_dir().as_deref(), Some("/proj/a/b"));
        // Trailing slash: the whole thing is the path part.
        s.query = "b/c/".into();
        assert_eq!(s.explorer_listing_dir().as_deref(), Some("/proj/a/b/c"));
    }

    #[test]
    fn pending_create_for_multi_segment_checks_leaf_against_peek_listing() {
        // Peeked into `b` (listing shows its entries); `b/c` where `c` is present → no create.
        let mut s = explorer_with(&["c", "d"]);
        s.query = "b/c".into();
        assert_eq!(s.pending_create(), None, "leaf `c` is in the peek listing");
        // `b/novel` → the leaf isn't listed, so offer to create the (multi-segment) file.
        s.query = "b/novel".into();
        assert_eq!(
            s.pending_create(),
            Some(PendingCreate {
                name: "b/novel".into(),
                is_dir: false
            })
        );
    }

    #[test]
    fn create_row_is_a_selectable_row_past_the_last_match() {
        let mut s = explorer_with(&["a.rs", "b.rs"]);
        s.query = "c.rs".into();
        assert_eq!(s.create_row_index(), Some(2)); // one past the two matches
                                                   // Arrow down walks onto the create row without forcing a refetch.
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 1);
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 2);
        assert!(s.selected_is_create());
        // It's the bottom row — can't move past it.
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn create_row_is_the_only_row_when_nothing_matches() {
        let mut s = explorer_with(&[]); // empty directory
        s.query = "first.rs".into();
        assert_eq!(s.create_row_index(), Some(0));
        // With zero matches the create row is selected by default and is its own bottom.
        assert!(s.selected_is_create());
        assert_eq!(s.move_selection(1), None);
        assert!(s.selected_is_create());
    }

    #[test]
    fn display_rows_appends_the_create_row_at_the_window_end() {
        let mut s = explorer_with(&["a.rs", "b.rs"]);
        s.query = "c.rs".into();
        let rows = s.display_rows();
        assert_eq!(rows.len(), 3);
        match &rows[2] {
            DisplayRow::Create { abs, name, is_dir } => {
                assert_eq!(*abs, 2);
                assert_eq!(name, "c.rs");
                assert!(!is_dir);
            }
            _ => panic!("expected a Create row last"),
        }
    }
}
