//! Picker state — the platform-free half of the picker (docs/client-core.md): query and
//! generation staleness, selection and identity, chip/filter state, display-row math. The
//! rendering half lives in the shell (`src/picker.rs`).
//!
use crate::chips::{self, Chip, ChipEditor, ChipEditorKind, ChipId, ChipValue, DirListingState};
use aether_protocol::picker::{
    ExpandedRun, GroupHeader, GroupSpan, PickerFilters, PickerItem, PickerKind, PickerUpdateParams,
};

/// Rows the panel shows at once.
pub const VISIBLE_ROWS: usize = 18;
/// Window size requested from the server (over-fetched so small moves don't refetch).
pub const FETCH_LIMIT: u32 = 90;

/// How to scroll the highlight into view when the next update lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// Scroll the minimum to bring the row inside the viewport (keyboard moves).
    Minimal,
    /// Align the row to the top unless already visible (grep file-jumps — context below).
    Top,
    /// Reveal the newly selected group's *run* (docs/picker-groups.md §9): scroll the minimum
    /// that brings the run's last row into view, capped so the run's header never leaves the
    /// top — a run taller than the pane shows the header at the very top (where it renders
    /// itself, so nothing hides under a sticky pin) with as many items as fit. Emitted by the
    /// group-select path (`Event::GroupSet`); shells resolve the run's rows from the state's
    /// `expanded_run`, applying only when it matches the selection (`header_row == selected`)
    /// so a pre-adoption fire against the *old* run is a no-op — the armed re-emit after the
    /// reshaped push lands does the real work.
    Run,
}

/// Which level of the two-level model (docs/picker-groups.md §9) the selection is on, for the
/// collapsible kinds. **Stored, not derived**: the row-space facts a derivation would read —
/// `selected` (moved by the `set_group` reply) and `expanded_run` (moved by the reshaping
/// push) — arrive on separate, order-independent messages, and a held `Alt-j` repeat can fire
/// in the gap between them. Deriving the level there reads a mismatched pair: the *new*
/// selection row can land inside the *stale* run interval, misclassify as item level, and
/// turn a group step into a local walk into the run. Only explicit gestures flip this bit
/// (step/select/ascend/query → `Group`; descend / a centred open landing on an entry →
/// `Item`); [`PickerState::selection_at_item_level`] still requires the run interval to agree,
/// so a stale bit can never move the selection outside the run either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerLevel {
    /// The selection is a group header; `Alt-j`/`Alt-k` step between groups.
    Group,
    /// The selection is inside the expanded run; `Alt-j`/`Alt-k` walk its items.
    Item,
}

/// Where a group-select gesture lands the selection within the newly selected run.
/// Client-side only: the `picker/set_group` reply carries the run's geometry
/// ([`aether_protocol::picker::ExpandedRun`]) and the client picks the row — the request's
/// completion closure carries this intent into `Event::GroupSet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupLanding {
    /// Group-level navigation (`Alt-j`/`Alt-k` on a header, a header click / re-select):
    /// land on the run's header, staying at group level.
    Header,
    /// An item-level spill over the run's *last* row (docs/picker-groups.md §9): enter the
    /// next group at its first item, staying at item level.
    RunStart,
    /// An item-level spill over the run's *first* row: enter the previous group at its last
    /// item, staying at item level.
    RunEnd,
}

pub struct PickerState {
    pub kind: PickerKind,
    /// Whether the active workspace declares any projects (`docs/projects.md`). Only the
    /// workspace-symbols picker reads it, to tell "your query matched nothing" from "nothing can
    /// answer here yet" — two very different things to show an empty list for. Stamped when the
    /// picker opens; a project added mid-picker is a re-open away.
    pub workspace_has_projects: bool,
    /// The query value. Text editing (caret, insert, delete) is owned by each shell's input —
    /// native `text_input`/`<input>` in the rich clients, a shell-local editor in the TUI — which
    /// syncs the whole value via [`crate::update`]'s `picker_set_query`. The core keeps only the
    /// value plus the chip-row command gestures (`Left`/`Backspace` at the query start, etc.).
    pub query: String,
    pub generation: u64,
    /// The fetched window starting at `offset` (absolute index into the match list).
    pub items: Vec<PickerItem>,
    /// The window's group runs (window-relative starts, server-pushed alongside `items` — the
    /// single source of group boundaries; see `GroupSpan`). Empty for the flat kinds.
    pub groups: Vec<GroupSpan>,
    /// Collapsible kinds (docs/picker-groups.md §9): the expanded run's absolute place in the
    /// row space — header row + item count, server-pushed alongside `items`. What the
    /// two-level navigation does its local math against, together with the [`Self::level`]
    /// bit. `None` for the other kinds and while the result set is empty.
    pub expanded_run: Option<ExpandedRun>,
    /// The two-level navigation level (collapsible kinds; see [`PickerLevel`] for why this is
    /// stored rather than derived). Fresh opens and query changes are group level; a centred
    /// open landing on an entry is item level.
    pub level: PickerLevel,
    /// True while a `picker/set_group` gesture (group step/select, item-level spill) awaits
    /// its outcome — the row space is about to reshape, so `Alt-j`/`Alt-k` repeats are
    /// swallowed rather than mis-routed against transient state (a repeat mid-reshape would
    /// step twice, skipping a group's items). Cleared when a real window is adopted
    /// ([`Self::apply_update`] with items — the reshaping push, whichever side of the reply
    /// it lands on), and by the no-op/error reply arms (no push follows those).
    pub group_gesture_in_flight: bool,
    pub offset: u32,
    /// Absolute index of the highlighted row.
    pub selected: u32,
    pub total_matches: u32,
    pub total_candidates: u32,
    pub ticking: bool,
    /// Display-row index of the fetched window's first row (grep: headers above included,
    /// from `display_offset`; other kinds: equals `offset`). Sizes the top spacer.
    pub display_offset: u32,
    /// Total display rows in the whole result set (grep: hits + group headers). Sizes the
    /// virtual-scroll spacers.
    pub total_display_rows: u32,
    /// Item to re-highlight when the first matching update arrives (`center_on` echo).
    /// Matched by identity ([`item_key`]) — the listed item carries live decoration
    /// (git status, match indices) the anchor doesn't.
    pub pending_center: Option<PickerItem>,
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
    /// Explorer: the anchor's parent, when still inside the workspace boundary.
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
    /// Single-flight guard for window refetches: true while a `picker/view` fired by
    /// [`crate::Session::picker_refetch`] is awaiting its reply. A *selection* move that leaves the
    /// window while this is set is coalesced — `selected` keeps advancing locally and the reply's
    /// trailing check chases it with one more fetch — so a fast keyboard scroll fires ~one refetch
    /// per round-trip instead of one per move. Reset when a fresh window supersedes the cycle
    /// (query change / dir nav) so it can't wedge.
    pub refetch_in_flight: bool,
    /// Whether the in-flight refetch should *chase the selection* when it lands (see the trailing
    /// check in the `PickerViewed` handler). True for selection-driven refetches (keyboard nav);
    /// **false for free pixel scroll** (iced / web scrollbar), where the view moves independently
    /// of the selection — chasing there would yank the window back to the selection and fight the
    /// scroll (a blank, oscillating scrollbar).
    pub refetch_chases_selection: bool,
    /// True once any real window (`items: Some`) has been adopted — before that, an empty
    /// `items` means "not loaded yet", not "no results". The hint facts read this so the
    /// workspace chooser's empty-list hint can't fire on the pre-load flash (docs/hints.md).
    pub loaded: bool,
    /// Jumplist only: whether the captured list is worth path-scoping (spans more than one file,
    /// with at least one in-root entry) — the `picker/view` echo of the server-computed flag.
    /// Gates the dir/glob chip chords via [`Self::filter_available`]; false until the first view
    /// result lands, so early chords are clean no-ops.
    pub path_filterable: bool,
}

/// Braille throbber frames for the "still searching" spinner (left of the picker's count).
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl PickerState {
    pub fn new(kind: PickerKind) -> Self {
        PickerState {
            kind,
            // Stamped by `open_picker`, which knows the session's projects.
            workspace_has_projects: false,
            query: String::new(),
            generation: 0,
            items: Vec::new(),
            groups: Vec::new(),
            expanded_run: None,
            level: PickerLevel::Group,
            group_gesture_in_flight: false,
            offset: 0,
            selected: 0,
            total_matches: 0,
            total_candidates: 0,
            ticking: true,
            display_offset: 0,
            total_display_rows: 0,
            pending_center: None,
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
            refetch_in_flight: false,
            refetch_chases_selection: false,
            loaded: false,
            path_filterable: false,
        }
    }

    /// Whether a filter chip is available *right now*: the static per-kind table
    /// ([`chips::filter_applies`]), plus the Jumplist's data gate — its dir/glob chips only
    /// apply when the capture spans in-root files (the server's `path_filterable` echo).
    pub fn filter_available(&self, id: ChipId) -> bool {
        chips::filter_applies(self.kind, id)
            && (self.kind != PickerKind::Jumplist || self.path_filterable)
    }

    /// Workspace rows this (Workspaces) picker is showing, or `None` when it isn't the
    /// Workspaces picker or hasn't adopted a window yet — before that, an empty item list means
    /// "not loaded", not "no workspaces". Feeds the hint facts (docs/hints.md): zero teaches
    /// creating a workspace, some teach opening one.
    pub fn listed_workspaces(&self) -> Option<u32> {
        (self.kind == PickerKind::Workspaces && self.loaded).then(|| {
            self.items
                .iter()
                .filter(|i| matches!(i, PickerItem::Workspace { .. }))
                .count() as u32
        })
    }

    /// The throbber glyph to show while a search is in progress (`ticking`), or `None` when settled.
    /// The frame advances per applied push (see [`apply_update`]), so it animates while results
    /// stream and stops the moment the search completes.
    pub fn spinner_glyph(&self) -> Option<&'static str> {
        self.ticking
            .then(|| SPINNER_FRAMES[self.spinner_frame as usize % SPINNER_FRAMES.len()])
    }

    /// The rendered chip row, derived from the stored list.
    pub fn chip_row(&self, workspace_paths: &[String]) -> Vec<Chip> {
        chips::derive_chips(&self.chips, workspace_paths)
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
    pub fn live_filters(&self, workspace_paths: &[String]) -> Option<PickerFilters> {
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
                if let Some(scope) = ed.preview_scope(workspace_paths) {
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
    /// - **Workspaces**: a fresh workspace by that name. Selecting it runs `workspace_create_from_query`.
    ///   `is_dir` is irrelevant for workspaces (always `false`); names with path separators are
    ///   rejected (the server forbids them too).
    ///
    /// Returns `None` for any other kind, an empty/invalid name, or when a listed entry already
    /// matches it exactly — so the row appears the moment you type a novel name and vanishes again
    /// once the listing contains it.
    pub fn pending_create(&self) -> Option<PendingCreate> {
        match self.kind {
            PickerKind::Explorer => self.explorer_pending_create(),
            PickerKind::Workspaces => self.workspace_pending_create(),
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

    fn workspace_pending_create(&self) -> Option<PendingCreate> {
        let name = self.query.trim();
        // Workspace names must be a single non-empty segment (the server stores them as a TOML file
        // stem and refuses path separators).
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return None;
        }
        // Suppress when a listed workspace already carries this exact name (Enter would activate it).
        // Case-sensitive, matching the file-stem identity.
        let exact = self
            .items
            .iter()
            .any(|it| matches!(it, PickerItem::Workspace { name: n, .. } if n == name));
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
        // the counts. `Some` replaces it (an empty vec is a genuinely empty result set). The
        // group spans describe `items`, so they're adopted and kept in lockstep with it.
        let has_items = u.items.is_some();
        if let Some(items) = u.items {
            self.items = items;
            self.groups = u.groups;
            // Like the spans, the expanded run describes the pushed result set — adopted in
            // lockstep with `items`, kept across count-only ticks.
            self.expanded_run = u.expanded_run;
            // A real window landed — any pending group gesture's reshape is now in hand (or
            // superseded), so `Alt-j`/`Alt-k` repeats may flow again.
            self.group_gesture_in_flight = false;
            // A real window landed: "no rows" now means genuinely empty, not not-yet-loaded.
            // The hint facts (docs/hints.md) key the chooser's create-vs-open hint on this.
            self.loaded = true;
        }
        // Adopt the push's counts + display geometry from a real window (`Some`) or a count tick
        // that's reporting actual progress (`total_matches > 0`). A count-only tick reporting
        // *nothing yet* (`items: None`, `total_matches == 0`) is the "search just started — keep
        // the previous window" signal the server sends on a grep/async query change: adopting its
        // zeros would collapse the geometry the shells size the viewport from (`total_display_rows`
        // → the iced list height, the web spacer, the TUI scrollbar) and flash the kept rows away
        // for a frame. Keep the prior counts until the first real batch (or a non-zero tick) lands.
        if has_items || u.total_matches > 0 {
            self.total_matches = u.total_matches;
            self.total_candidates = u.total_candidates;
            self.display_offset = u.display_offset.unwrap_or(u.offset);
            self.total_display_rows = u.total_display_rows.unwrap_or(u.total_matches);
        }
        self.ticking = u.ticking;
        self.explorer_peek_missing = u.explorer_peek_missing;
        // Advance the throbber each applied push while a search is still running.
        if u.ticking {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
        if let Some(center) = self.pending_center.take() {
            let key = item_key(&center);
            if let Some(pos) = self.items.iter().position(|i| item_key(i) == key) {
                self.selected = self.offset + pos as u32;
                // The centred row decides the level (docs/picker-groups.md §9): a cursor-
                // anchored open lands on an entry/hunk — item level; a header anchor stays
                // group level.
                if self.kind.collapsible() {
                    self.level = if matches!(self.items[pos], PickerItem::Group { .. }) {
                        PickerLevel::Group
                    } else {
                        PickerLevel::Item
                    };
                }
            } else {
                self.pending_center = Some(center); // not in this window yet
            }
        }
        // The create row (Explorer) adds one selectable slot past the matches; keep the highlight
        // within the selectable rows (the whole row space for the collapsible kinds — headers
        // are selectable rows there, so clamping to bare `total_matches` would yank a
        // header-row selection upward).
        let rows = self.selectable_rows() + self.create_row_index().is_some() as u32;
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

    /// The count of selectable rows the highlight moves over: matches for the flat /
    /// derived-header kinds; the whole *row space* — headers plus the expanded run's items —
    /// for the collapsible kinds (docs/picker-groups.md), whose `selected`/`offset` index
    /// window rows. `total_display_rows` is that row total (it falls back to `total_matches`
    /// on adoption for the flat kinds, so this is safe before the first grouped push).
    fn selectable_rows(&self) -> u32 {
        if self.kind.collapsible() {
            self.total_display_rows
        } else {
            self.total_matches
        }
    }

    /// The expanded run's item rows as an inclusive absolute-row interval, when a collapsible
    /// picker has one (docs/picker-groups.md §9). The selection is *item level* exactly when
    /// it sits inside this interval — every other row is a group header.
    pub fn expanded_item_rows(&self) -> Option<(u32, u32)> {
        let run = self.expanded_run?;
        (run.len > 0).then(|| (run.header_row + 1, run.header_row + run.len))
    }

    /// Two-level navigation level (collapsible kinds, docs/picker-groups.md §9): `true` when
    /// the selection is *effectively* at item level — the stored [`Self::level`] bit says so
    /// AND the selection sits inside the expanded run's rows. The conjunction is the point:
    /// the bit can't misroute a group step into the run when a held repeat fires between the
    /// `set_group` reply and its reshaping push (see [`PickerLevel`]), and the interval can't
    /// let a stale bit walk the selection outside the run. Meaningless for the
    /// non-collapsible kinds — callers gate on [`PickerKind::collapsible`] first.
    pub fn selection_at_item_level(&self) -> bool {
        self.level == PickerLevel::Item
            && self
                .expanded_item_rows()
                .is_some_and(|(first, last)| (first..=last).contains(&self.selected))
    }

    /// Move the highlight by `delta`, returning the new window offset to fetch when the
    /// highlight left the fetched window (the caller sends `picker/view`).
    ///
    /// For the collapsible kinds this is the *item-level* move of the two-level model
    /// (docs/picker-groups.md §9): the selection walks the expanded run's rows and stops hard
    /// at the run's ends — like the jumplist's `]`/`[` at its ends. Group-level moves are
    /// `picker/set_group { step }`, routed by the caller before it gets here; called at group
    /// level this is a no-op.
    pub fn move_selection(&mut self, delta: i64) -> Option<u32> {
        if self.kind.collapsible() {
            if !self.selection_at_item_level() {
                return None; // group level (or incoherent/empty): nothing moves locally
            }
            let Some((first, last)) = self.expanded_item_rows() else {
                return None;
            };
            self.selected = (self.selected as i64 + delta).clamp(first as i64, last as i64) as u32;
            let in_window = self.selected >= self.offset
                && self.selected < self.offset + self.items.len() as u32;
            if in_window {
                return None;
            }
            self.reveal_on_update = Some(Reveal::Minimal);
            return Some(self.selected.saturating_sub(FETCH_LIMIT / 2));
        }
        // The synthetic create row (Explorer) is one extra selectable row past the last match.
        let create = self.create_row_index();
        let rows = self.selectable_rows() + create.is_some() as u32;
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

    /// The fetched window as uniform display rows: one group-header row before each group run,
    /// straight from the server-pushed spans (the single source of group boundaries — no
    /// client-side key derivation), every display row the same height (the shell's `ROW_H`).
    /// A window that begins mid-group still leads with its group's header: the server repeats
    /// the split group's header at `start: 0`.
    ///
    /// The collapsible kinds (docs/picker-groups.md) skip the span interleave entirely: their
    /// headers arrive as real, selectable [`PickerItem::Group`] window rows, so items map 1:1
    /// to `Item` display rows and the spans only feed the sticky pin + the split-window lead.
    pub fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut rows = Vec::with_capacity(self.items.len() + self.groups.len() + 1);
        let mut spans = self.groups.iter().peekable();
        for (i, item) in self.items.iter().enumerate() {
            while let Some(span) = spans.next_if(|s| s.start as usize <= i) {
                if self.kind.collapsible() {
                    continue; // headers are the `Group` rows themselves
                }
                rows.push(match &span.header {
                    GroupHeader::File {
                        path_index,
                        relative_path,
                    } => DisplayRow::Header {
                        path_index: *path_index,
                        relative_path,
                    },
                    GroupHeader::Label { label } => DisplayRow::Section { label },
                });
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
        self.window_base_of(&self.display_rows())
    }

    /// [`Self::window_base`] from already-built rows — derives "leads with a header" straight from
    /// `display_rows` so the two can never disagree about which kinds emit headers. (They did once:
    /// workspace Diagnostics was added to `display_rows` but not to `window_base`'s old hardcoded
    /// variant list, so the window sat one row off.) Callers holding the rows pass them in to avoid
    /// rebuilding.
    fn window_base_of(&self, rows: &[DisplayRow]) -> u32 {
        let leads_with_header = matches!(
            rows.first(),
            Some(DisplayRow::Header { .. } | DisplayRow::Section { .. })
        );
        self.display_offset.saturating_sub(leads_with_header as u32)
    }

    /// The highlighted item's display-row index in the whole virtual list, when it's inside
    /// the fetched window.
    pub fn selected_display_row(&self) -> Option<u32> {
        let rows = self.display_rows();
        let base = self.window_base_of(&rows);
        rows.iter()
            .position(|r| match r {
                DisplayRow::Item { abs, .. } | DisplayRow::Create { abs, .. } => {
                    *abs == self.selected
                }
                DisplayRow::Header { .. } | DisplayRow::Section { .. } => false,
            })
            .map(|i| base + i as u32)
    }

    /// Inter-group gap accounting, for the pixel-based shells (iced / web). The grouped pickers
    /// render a small gap after each group except the last; the gap is *pixels outside the
    /// display-row unit* (display rows stay uniform — the virtual-scroll invariant), so the
    /// shells add `gap_px × count` to their spacer sizes and row positions. The TUI instead
    /// renders the gap as a real blank line, purely locally. One gap sits *before* every group
    /// header except the list's very first — equivalently, after each group but the last.
    ///
    /// Total gaps across the whole result set: `total groups − 1`. Total groups falls out of
    /// the display metrics (`total_display_rows − total_matches`), so this needs no extra wire
    /// data. Zero for flat kinds, empty results, and the collapsible kinds — their headers are
    /// uniform window rows (docs/picker-groups.md), not gap-separated decorations.
    pub fn total_gap_count(&self) -> u32 {
        if self.groups.is_empty() || self.kind.collapsible() {
            return 0;
        }
        self.total_display_rows
            .saturating_sub(self.total_matches)
            .saturating_sub(1)
    }

    /// Gaps fully above the fetched window — the groups that *ended* above it. Falls out of the
    /// window metrics: `window_base − offset` counts the headers strictly above the window
    /// (every grouped window leads with its own header), and each of those groups ended above
    /// (a group ending inside the window would BE the window's leading group). Zero for flat
    /// kinds.
    pub fn gaps_above_window(&self) -> u32 {
        if self.groups.is_empty() || self.kind.collapsible() {
            return 0;
        }
        self.window_base().saturating_sub(self.offset)
    }

    /// Gaps inside the window at or before window-relative display row `rel` (an index into
    /// [`Self::display_rows`]): one before each header row except the window's first display
    /// row. A header row's own gap counts toward its position (the gap sits above it).
    pub fn gaps_before_display_rel(&self, rel: u32) -> u32 {
        self.display_rows()
            .iter()
            .enumerate()
            .take(rel as usize + 1)
            .skip(1)
            .filter(|(_, r)| matches!(r, DisplayRow::Header { .. } | DisplayRow::Section { .. }))
            .count() as u32
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
        let rows = self.display_rows();
        let base = self.window_base_of(&rows);
        let window_end = base + rows.len() as u32;
        let needs = first_visible < base
            || (last_visible > window_end && window_end < self.total_display_rows);
        if !needs {
            return None;
        }
        let ratio = self.total_matches as f32 / self.total_display_rows as f32;
        let est_item = (first_visible as f32 * ratio) as u32;
        Some(est_item.saturating_sub(FETCH_LIMIT / 2))
    }

    /// The settled empty-state note for the rows area — the line to show when the result set is
    /// empty and not mid-search — or `None` when no note belongs: results exist, a search is still
    /// running (the shell shows its own "Searching…"), an *unqueried* Grep (no search has run yet,
    /// so a note would read as a failed one), or the Explorer's "+ Create …" row stands in.
    ///
    /// The single source of this wording across shells. A non-empty query that matched nothing is
    /// "No matches"; an empty query is the kind's "nothing here" line ("No diagnostics" / "No
    /// changes" / …) — because those kinds list their whole set without a query, so empty means
    /// genuinely none, not a failed search. References / symbols keep their own phrasing regardless.
    pub fn empty_note(&self) -> Option<&'static str> {
        if self.ticking || self.total_matches != 0 || self.pending_create().is_some() {
            return None;
        }
        if self.kind == PickerKind::Grep && self.query.is_empty() {
            return None;
        }
        Some(match self.kind {
            PickerKind::References => "No references found",
            PickerKind::DocumentSymbols => "No symbols found",
            // Two distinct states, and conflating them would be the difference between "your query
            // matched nothing" and "this feature can't work here yet" — see
            // `docs/workspace-symbols.md` § Scope.
            PickerKind::WorkspaceSymbols if !self.workspace_has_projects => {
                "No projects configured"
            }
            PickerKind::WorkspaceSymbols => "No symbols found",
            _ if !self.query.is_empty() => "No matches",
            PickerKind::Diagnostics | PickerKind::DiagnosticsWorkspace => "No diagnostics",
            PickerKind::GitChanges | PickerKind::GitChangesFile => "No changes",
            PickerKind::Keybindings => "No keybindings",
            // Empty list, empty query: advertise how to fill it (a non-empty query that matched
            // nothing already took the "No matches" arm above).
            PickerKind::Jumplist => "Nothing captured — Ctrl-j in a picker captures results",
            _ => "No results",
        })
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
    Workspace(&'a str),
    LspServer(&'a str, &'a str),
    Reference(&'a str, u32, u32),
    Symbol(&'a str, u32, u32),
    /// `(mode, keys, desc)` — a chord can be bound in several modes, and an Alt-variant can
    /// share a description, so all three disambiguate.
    Keybinding(&'a str, &'a str, &'a str),
    /// The captured entry's position in the jumplist — positional identity, stable for the
    /// picker's lifetime (a re-capture resets the picker).
    JumplistEntry(u32),
    /// A collapsible group's header row, keyed like the server's `group_key_at`: a `File`
    /// header is `(path_index, relative_path)`, a `Label` header `(u32::MAX, label)`.
    Group(u32, &'a str),
}

/// A Keybinding row's `match_indices` split per rendered segment. The wire indices are char
/// offsets into the row's composed haystack (`KeybindingEntry::haystack`, `{desc} [({mode}) ]
/// {keys}` — the mode is present only for Insert/Search rows, see
/// `KeybindingEntry::shows_mode`; the group is a section header, not row text); each field here
/// is the subset that falls inside that segment, rebased to the segment's own chars — ready to
/// feed a shell's per-span highlighter. Indices landing on the literal separators are dropped:
/// the shells style separators dim and unhighlighted. `mode` stays empty for rows whose mode is
/// elided.
#[derive(Debug, Default, PartialEq)]
pub struct KeybindingSegments {
    pub desc: Vec<u32>,
    pub mode: Vec<u32>,
    pub keys: Vec<u32>,
}

/// Split a [`PickerItem::Keybinding`]'s haystack-relative `match_indices` into per-segment
/// lists (see [`KeybindingSegments`]). The single source of the segment offsets — keep in
/// lockstep with `KeybindingEntry::haystack` (the web shell mirrors this in TypeScript).
pub fn keybinding_match_segments(
    desc: &str,
    mode: &str,
    keys: &str,
    match_indices: &[u32],
) -> KeybindingSegments {
    let (d, k) = (desc.chars().count() as u32, keys.chars().count() as u32);
    // Segment start offsets within the haystack: `{desc} ({mode}) {keys}` / `{desc} {keys}`.
    let m = if aether_protocol::picker::KeybindingEntry::shows_mode(mode) {
        mode.chars().count() as u32
    } else {
        0
    };
    let mode_at = d + 2; // only meaningful when `m > 0`
    let keys_at = if m > 0 { d + 2 + m + 2 } else { d + 1 };
    let mut out = KeybindingSegments::default();
    for &i in match_indices {
        if i < d {
            out.desc.push(i);
        } else if m > 0 && (mode_at..mode_at + m).contains(&i) {
            out.mode.push(i - mode_at);
        } else if (keys_at..keys_at + k).contains(&i) {
            out.keys.push(i - keys_at);
        }
    }
    out
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
        PickerItem::Workspace { name, .. } => ItemKey::Workspace(name),
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
        PickerItem::Keybinding {
            mode, keys, desc, ..
        } => ItemKey::Keybinding(mode, keys, desc),
        PickerItem::JumplistEntry { index, .. } => ItemKey::JumplistEntry(*index),
        PickerItem::Group { header, .. } => match header {
            GroupHeader::File {
                path_index,
                relative_path,
            } => ItemKey::Group(*path_index, relative_path),
            GroupHeader::Label { label } => ItemKey::Group(u32::MAX, label),
        },
    }
}

/// One uniform-height row of the rendered list.
pub enum DisplayRow<'a> {
    Header {
        path_index: u32,
        relative_path: &'a str,
    },
    /// A non-selectable section label: References' `Definition` / `References` split, and the
    /// Keybindings picker's per-group headings. Like [`DisplayRow::Header`] but the content is a
    /// label, not a file path.
    Section {
        label: &'a str,
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

    #[test]
    fn keybinding_match_segments_rebase_and_drop_separators() {
        // Elided mode (Any). Haystack: "Delete word back Ctrl-w"
        //                               0..............15  17..22
        let seg = keybinding_match_segments("Delete word back", "Any", "Ctrl-w", &[0, 16, 17, 22]);
        assert_eq!(seg.desc, vec![0]); // 'D' (16 lands on the separator space → dropped)
        assert_eq!(seg.mode, Vec::<u32>::new()); // elided — nothing can land in it
        assert_eq!(seg.keys, vec![0, 5]); // 'C', 'w'

        // Shown mode (Insert). Haystack: "Delete word back (Insert) Ctrl-w"
        //                                 0..............15  18...23  26..31
        let seg = keybinding_match_segments(
            "Delete word back",
            "Insert",
            "Ctrl-w",
            &[17, 18, 23, 26, 31],
        );
        assert_eq!(seg.mode, vec![0, 5]); // 'I', 't' (17 lands on the '(' → dropped)
        assert_eq!(seg.keys, vec![0, 5]); // 'C', 'w'

        // Round-trip sanity: the haystacks really are composed the way the offsets assume.
        let mut e = aether_protocol::picker::KeybindingEntry {
            group: "Editing".into(),
            desc: "Delete word back".into(),
            mode: "Any".into(),
            keys: "Ctrl-w".into(),
        };
        let hay: Vec<char> = e.haystack().chars().collect();
        assert_eq!(hay[0], 'D');
        assert_eq!(hay[17], 'C');
        assert_eq!(hay[22], 'w');
        e.mode = "Insert".into();
        let hay: Vec<char> = e.haystack().chars().collect();
        assert_eq!(hay[18], 'I');
        assert_eq!(hay[23], 't');
        assert_eq!(hay[26], 'C');
        assert_eq!(hay[31], 'w');
    }

    #[test]
    fn gap_accounting_counts_between_group_gaps() {
        // A derived-header kind (References-style Label sections): the END window of an
        // 18-display-row list (13 items + 5 sections), holding its last two sections.
        let reference = |path: &str, line: u32| PickerItem::Reference {
            path: path.into(),
            display_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            is_definition: false,
            match_indices: vec![],
        };
        let label_span = |start: u32, label: &str| GroupSpan {
            start,
            header: GroupHeader::Label {
                label: label.into(),
            },
            count: None,
            expanded: None,
        };
        let mut s = PickerState::new(PickerKind::References);
        s.offset = 10;
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::References,
            generation: 0,
            offset: 10,
            items: Some(vec![
                reference("a.rs", 1),
                reference("a.rs", 2),
                reference("b.rs", 1)
            ]),
            total_matches: 13,
            total_candidates: 13,
            ticking: false,
            groups: vec![label_span(0, "A"), label_span(2, "B")],
            display_offset: Some(14),
            total_display_rows: Some(18),
            expanded_run: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        // 18 display rows − 13 items = 5 sections → 4 between-group gaps overall.
        assert_eq!(s.total_gap_count(), 4);
        // window_base = 13, offset = 10 → 3 headers strictly above = 3 sections ended above.
        assert_eq!(s.gaps_above_window(), 3);
        // Window rows: [0]=hdr A, [1..2]=items, [3]=hdr B, [4]=item. The leading header
        // gets no gap; B's header (and everything after it) shifts by one.
        assert_eq!(s.gaps_before_display_rel(0), 0);
        assert_eq!(s.gaps_before_display_rel(2), 0);
        assert_eq!(s.gaps_before_display_rel(3), 1);
        assert_eq!(s.gaps_before_display_rel(4), 1);
        // Flat kinds have no gaps anywhere.
        let flat = PickerState::new(PickerKind::Files);
        assert_eq!(flat.total_gap_count(), 0);
        assert_eq!(flat.gaps_above_window(), 0);
        // Nor do the collapsible kinds — their headers are uniform window rows
        // (docs/picker-groups.md), not gap-separated decorations.
        let mut grep = PickerState::new(PickerKind::Grep);
        grep.groups = file_spans(&[(0, 0, "a.rs")]);
        grep.total_display_rows = 6;
        grep.total_matches = 4;
        assert_eq!(grep.total_gap_count(), 0);
        assert_eq!(grep.gaps_above_window(), 0);
    }

    #[test]
    fn keybinding_display_rows_emit_one_section_per_group_run() {
        let kb = |group: &str, desc: &str| PickerItem::Keybinding {
            group: group.into(),
            desc: desc.into(),
            mode: "Normal".into(),
            keys: "x".into(),
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::Keybindings);
        s.items = vec![
            kb("Motion", "Character left"),
            kb("Motion", "Character right"),
            kb("Edit", "Undo"),
        ];
        s.groups = vec![
            GroupSpan {
                start: 0,
                header: GroupHeader::Label {
                    label: "Motion".into(),
                },
                count: None,
                expanded: None,
            },
            GroupSpan {
                start: 2,
                header: GroupHeader::Label {
                    label: "Edit".into(),
                },
                count: None,
                expanded: None,
            },
        ];
        s.total_matches = 3;
        let rows = s.display_rows();
        let labels: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Section { label } => Some(*label),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["Motion", "Edit"]);
        // Section + 2 items + section + item.
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], DisplayRow::Section { .. }));
        assert!(matches!(rows[3], DisplayRow::Section { .. }));
    }

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
                    .map(|i| PickerItem::Workspace {
                        name: format!("p{i}"),
                        unsaved_buffers: 0,
                        match_indices: vec![],
                    })
                    .collect(),
            ),
            total_matches: total,
            total_candidates: total,
            ticking: false,
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
            expanded_run: None,
            center_on: None,
            explorer_peek_missing: false,
        }
    }

    /// Shorthand `File` group spans for the grouped-display fixtures.
    fn file_spans(spans: &[(u32, u32, &str)]) -> Vec<GroupSpan> {
        spans
            .iter()
            .map(|&(start, path_index, rel)| GroupSpan {
                start,
                header: GroupHeader::File {
                    path_index,
                    relative_path: rel.into(),
                },
                count: None,
                expanded: None,
            })
            .collect()
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
        // Collapsible grep (docs/picker-groups.md): headers arrive as real `Group` window rows
        // and the offset/selection space IS the display space — no interleave, no reconcile.
        let hit = |path: &str, line: u32| PickerItem::GrepHit {
            path_index: 0,
            relative_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        let group = |path: &str, count: u32, expanded: bool| PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: path.into(),
            },
            count,
            expanded,
        };
        let mut s = PickerState::new(PickerKind::Grep);
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Grep,
            generation: 0,
            offset: 0,
            // a.rs collapsed (2 hidden hits), b.rs expanded with its 2 hits inline.
            items: Some(vec![
                group("a.rs", 2, false),
                group("b.rs", 2, true),
                hit("b.rs", 1),
                hit("b.rs", 2)
            ]),
            total_matches: 4,
            total_candidates: 4,
            ticking: false,
            groups: file_spans(&[(0, 0, "a.rs"), (1, 0, "b.rs")]),
            display_offset: Some(0),
            total_display_rows: Some(4),
            expanded_run: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        // No synthesized header rows: the spans only feed the sticky pin; items map 1:1.
        let rows = s.display_rows();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| matches!(r, DisplayRow::Item { .. })));
        assert_eq!(s.window_base(), 0, "row space == display space");
        // Selection maps identically — header rows are selectable rows like any other.
        s.selected = 0;
        assert_eq!(s.selected_display_row(), Some(0));
        s.selected = 3;
        assert_eq!(s.selected_display_row(), Some(3));
    }

    #[test]
    fn workspace_diagnostics_count_file_headers_in_window_math() {
        // DiagnosticsWorkspace is collapsible (docs/picker-groups.md): its headers are real
        // `Group` window rows, so the window math is the identity — no interleave, and the
        // selection walks headers and items alike.
        let diag = |path: &str, line: u32| PickerItem::Diagnostic {
            path_index: 0,
            relative_path: path.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            severity: aether_protocol::viewport::DiagnosticSeverity::Error,
            message: "boom".into(),
            match_indices: vec![],
        };
        let group = |path: &str, count: u32, expanded: bool| PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: path.into(),
            },
            count,
            expanded,
        };
        let mut s = PickerState::new(PickerKind::DiagnosticsWorkspace);
        // Window rows: [0]=hdr a.rs (expanded), [1]=diag, [2]=diag, [3]=hdr b.rs (collapsed).
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::DiagnosticsWorkspace,
            generation: 0,
            offset: 0,
            items: Some(vec![
                group("src/a.rs", 2, true),
                diag("src/a.rs", 2),
                diag("src/a.rs", 9),
                group("src/b.rs", 1, false)
            ]),
            total_matches: 3,
            total_candidates: 3,
            ticking: false,
            groups: file_spans(&[(0, 0, "src/a.rs"), (3, 0, "src/b.rs")]),
            display_offset: Some(0),
            total_display_rows: Some(4), // 2 headers + the expanded run's 2 diagnostics
            expanded_run: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        // No synthesized headers — the Group rows are the headers.
        assert!(s
            .display_rows()
            .iter()
            .all(|r| matches!(r, DisplayRow::Item { .. })));
        assert_eq!(s.total_display_rows, 4);
        assert_eq!(s.window_base(), 0);
        // Selected-row math is the identity in row space.
        s.selected = 1;
        assert_eq!(s.selected_display_row(), Some(1));
        s.selected = 3;
        assert_eq!(s.selected_display_row(), Some(3));
        // Selection moves clamp to the row total (4 rows), not the 3-item match count.
        s.selected = 3;
        assert!(s.move_selection(5).is_none(), "already at the last row");
        assert_eq!(s.selected, 3);
    }

    #[test]
    fn item_level_needs_both_the_bit_and_the_run_interval() {
        let mut s = PickerState::new(PickerKind::Grep);
        s.expanded_run = Some(ExpandedRun {
            header_row: 1,
            len: 2,
        });
        s.total_display_rows = 4;
        // Inside the run's interval but the bit says Group — the held-Alt-j reply/push gap
        // (see `PickerLevel`): NOT item level, and local moves refuse the stale interval.
        s.selected = 2;
        assert!(!s.selection_at_item_level());
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 2, "no local walk at group level");
        // The bit flips (a descend gesture): both halves agree — item level, clamped moves.
        s.level = PickerLevel::Item;
        assert!(s.selection_at_item_level());
        let _ = s.move_selection(1);
        assert_eq!(s.selected, 3);
        // Bit says Item but the selection sits on a header row (a post-re-rank clamp):
        // not item level either — the interval keeps a stale bit honest.
        s.selected = 0;
        assert!(!s.selection_at_item_level());
    }

    #[test]
    fn empty_note_wording_by_kind_and_query() {
        // A settled (not ticking), result-less picker with the given query.
        let settled = |kind: PickerKind, query: &str| {
            let mut s = PickerState::new(kind);
            s.ticking = false;
            s.query = query.into();
            s
        };
        // Empty query → the kind's "nothing here" line (not "No matches").
        assert_eq!(
            settled(PickerKind::Diagnostics, "").empty_note(),
            Some("No diagnostics")
        );
        assert_eq!(
            settled(PickerKind::DiagnosticsWorkspace, "").empty_note(),
            Some("No diagnostics")
        );
        assert_eq!(
            settled(PickerKind::GitChanges, "").empty_note(),
            Some("No changes")
        );
        assert_eq!(
            settled(PickerKind::GitChangesFile, "").empty_note(),
            Some("No changes")
        );
        assert_eq!(
            settled(PickerKind::Files, "").empty_note(),
            Some("No results")
        );
        assert_eq!(
            settled(PickerKind::References, "").empty_note(),
            Some("No references found")
        );
        assert_eq!(
            settled(PickerKind::DocumentSymbols, "").empty_note(),
            Some("No symbols found")
        );
        // A query that filtered everything out → "No matches" (the async kinds keep their phrasing).
        assert_eq!(
            settled(PickerKind::Diagnostics, "foo").empty_note(),
            Some("No matches")
        );
        assert_eq!(
            settled(PickerKind::Files, "foo").empty_note(),
            Some("No matches")
        );
        assert_eq!(
            settled(PickerKind::References, "foo").empty_note(),
            Some("No references found")
        );
        // An unqueried Grep hasn't searched → no note; a queried empty Grep → "No matches".
        assert_eq!(settled(PickerKind::Grep, "").empty_note(), None);
        assert_eq!(
            settled(PickerKind::Grep, "foo").empty_note(),
            Some("No matches")
        );
        // Still searching, or rows present → no note.
        let mut ticking = settled(PickerKind::Diagnostics, "");
        ticking.ticking = true;
        assert_eq!(ticking.empty_note(), None);
        let mut has_rows = settled(PickerKind::Diagnostics, "");
        has_rows.total_matches = 3;
        assert_eq!(has_rows.empty_note(), None);
    }

    #[test]
    fn git_changes_file_renders_headerless() {
        let hunk = |line: u32| PickerItem::GitChange {
            path_index: 0,
            relative_path: "src/main.rs".into(),
            hunk_index: line,
            line,
            stage: aether_protocol::viewport::DiffStage::Unstaged,
            added: 1,
            removed: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        let items = vec![hunk(1), hunk(5)];
        // Workspace GitChanges is collapsible: its headers arrive as `Group` window rows, and
        // the spans must NOT be interleaved on top of them (that would double the header)...
        let mut workspace = PickerState::new(PickerKind::GitChanges);
        workspace.items = std::iter::once(PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "src/main.rs".into(),
            },
            count: 2,
            expanded: true,
        })
        .chain(items.clone())
        .collect();
        workspace.groups = file_spans(&[(0, 0, "src/main.rs")]);
        let rows = workspace.display_rows();
        assert_eq!(
            rows.len(),
            3,
            "the Group row + its two hunks, nothing added"
        );
        assert!(rows.iter().all(|r| matches!(r, DisplayRow::Item { .. })));
        // ...and the buffer-locked GitChangesFile is a single file with no header at all — the
        // server sends no spans and no Group rows, so the same items render flat.
        let mut file = PickerState::new(PickerKind::GitChangesFile);
        file.items = items;
        assert!(file
            .display_rows()
            .iter()
            .all(|r| matches!(r, DisplayRow::Item { .. })));
    }

    #[test]
    fn references_display_rows_split_into_definition_and_references_sections() {
        let reference = |path: &str, line: u32, is_definition: bool| PickerItem::Reference {
            path: path.into(),
            display_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            is_definition,
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::References);
        // Definition-first, then two uses — and the server's matching display-row geometry: 3 items
        // + 2 section headers = 5 display rows, the first item one row down under the Definition
        // header.
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::References,
            generation: 0,
            offset: 0,
            items: Some(vec![
                reference("lib.rs", 0, true),
                reference("a.rs", 5, false),
                reference("b.rs", 9, false),
            ]),
            total_matches: 3,
            total_candidates: 3,
            ticking: false,
            groups: vec![
                GroupSpan {
                    start: 0,
                    header: GroupHeader::Label {
                        label: "Definition".into(),
                    },
                    count: None,
                    expanded: None,
                },
                GroupSpan {
                    start: 1,
                    header: GroupHeader::Label {
                        label: "References".into(),
                    },
                    count: None,
                    expanded: None,
                },
            ],
            display_offset: Some(1),
            total_display_rows: Some(5),
            expanded_run: None,
            center_on: None,
            explorer_peek_missing: false,
        }));
        let rows = s.display_rows();
        // Section("Definition"), the def item, Section("References"), then the two uses.
        assert!(matches!(
            rows[0],
            DisplayRow::Section {
                label: "Definition"
            }
        ));
        assert!(matches!(rows[1], DisplayRow::Item { abs: 0, .. }));
        assert!(matches!(
            rows[2],
            DisplayRow::Section {
                label: "References"
            }
        ));
        assert!(matches!(rows[3], DisplayRow::Item { abs: 1, .. }));
        assert!(matches!(rows[4], DisplayRow::Item { abs: 2, .. }));
        // Section headers are non-selectable; the display-row index accounts for the headers above.
        assert_eq!(
            s.window_base(),
            0,
            "the window leads with the Definition header"
        );
        s.selected = 0;
        assert_eq!(
            s.selected_display_row(),
            Some(1),
            "def row sits below its header"
        );
        s.selected = 1;
        assert_eq!(
            s.selected_display_row(),
            Some(3),
            "first use is below both headers"
        );
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
            expanded_run: None,
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
            display_path: String::new(),
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
        s.pending_center = Some(PickerItem::Workspace {
            name: "p7".into(),
            unsaved_buffers: 0,
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
            groups: Vec::new(),
            display_offset: None,
            total_display_rows: None,
            expanded_run: None,
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
