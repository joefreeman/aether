//! Client-side picker state. The server owns the candidate cache, query, and ranked list; the
//! client owns the highlighted row plus a small persisted slot (`last_selected`) used to restore
//! the highlight on reopen via `view { center_on }`.

use crate::scroll::ScrollState;
use aether_protocol::directory::DirectoryEntry;
use aether_protocol::lsp::{LspProgress, LspStatus};
use aether_protocol::picker::{
    CaseMode, PickerFilters, PickerItem, PickerKind, PickerUpdateParams, ScopedPath,
};
use aether_protocol::BufferId;
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
/// Identity of the client's active entry in a freshly-opened Buffers / Projects picker — what
/// the initial highlight should step over. See `PickerState::default_skip`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultSkip {
    Buffer(BufferId),
    Project(String),
}

impl DefaultSkip {
    /// True when `item` is the active entry this skip names.
    fn matches(&self, item: &PickerItem) -> bool {
        match (self, item) {
            (DefaultSkip::Buffer(id), PickerItem::Buffer { buffer_id, .. }) => buffer_id == id,
            (DefaultSkip::Project(name), PickerItem::Project { name: n, .. }) => n == name,
            _ => false,
        }
    }
}

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
    /// Total display rows the whole result set occupies, when that differs from `total_matches`.
    /// Server-reported; in practice grep-only (hits + one header per file group — the wire field
    /// is `grep_total_display_rows`), `None` for the other kinds. Sizes the collapsed picker box.
    pub total_display_rows: Option<u32>,
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
    /// When set (Buffers / Projects open), the first push with items moves the highlight to the
    /// first item that *isn't* this client's active buffer/project — the thing you'd flip to.
    /// An identity check, not "skip row 0": the list is shared MRU (Buffers) or name-ordered
    /// (Projects), so another client's activity can put any item at the top. Cleared once
    /// applied, or by a query change (the user is steering somewhere else).
    pub default_skip: Option<DefaultSkip>,
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
    /// When set, a delete is awaiting `[y/N]` confirmation: the target row renders the prompt and
    /// key handling is restricted to confirm/cancel (mirroring the settings overlay's
    /// `pending_delete`). Cleared on open/hide and on resolve. Covers project deletion (Projects
    /// picker) and file/directory deletion (Files / Explorer pickers).
    pub pending_delete: Option<PendingDelete>,
    /// LSP-servers picker only. When set, the picker body shows this server's status/error detail
    /// (a drill-down entered with `Enter`) instead of the list; `Esc` clears it back to the list.
    /// A snapshot taken at `Enter` time — it doesn't live-update.
    pub lsp_detail: Option<LspServerDetail>,
    /// The filter set in effect, stored as the ordered chip list — the client's *single*
    /// source of truth, in insertion order (see `docs/picker-filters.md`). The wire format
    /// (the normalized, unordered `PickerFilters`) is derived on demand by
    /// [`PickerState::wire_filters`] and converted back by [`PickerState::adopt_filters`] on
    /// open/resume — the order itself never crosses the wire, so a resumed picker comes back
    /// in canonical order and true insertion order is session-ephemeral, like `chip_selected`.
    pub chips: Vec<ChipValue>,
    /// Index into the chip row — which, the row being the stored list itself, is also an index
    /// into [`PickerState::chips`]. While set, editing keys act on the chip (Enter edits,
    /// Backspace/Delete removes, Left/Right move) instead of the query/results. Entered via
    /// Left/Backspace at query cursor 0.
    pub chip_selected: Option<usize>,
    /// Below-input editor line for valued chips (glob / dir). While set, an extra row is
    /// revealed under the query (chips + query stay visible) and it owns all key events:
    /// Enter commits a chip, Esc cancels, Alt-h/l move between its fields, Alt-j/k cycle the
    /// root field's candidates.
    pub chip_editor: Option<ChipEditor>,
}

/// Which filter a chip stands for — the handle used to edit/remove it. `Dir` and `Glob` carry
/// their index into [`PickerState::chips`] (the repeatable chips; the rendered row is the
/// stored list, so row index = storage index). There's no root chip: scoping to a whole root
/// is a `Dir` chip with an empty relative path (a directory always implies its root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipId {
    Dir(usize),
    Glob(usize),
    Case,
    Word,
    Lit,
    Ignored,
    Hidden,
    Changed,
}

/// One chip, by value — the element of the client's ordered filter state. Everything the
/// chip row renders (and the wire `PickerFilters` folds up) lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipValue {
    Dir(ScopedPath),
    Glob(String),
    /// `Sensitive` or `Insensitive` — `Smart` (the default) is "no chip".
    Case(CaseMode),
    Word,
    Lit,
    /// Gitignored-file visibility. `hide` records the per-kind direction at creation time
    /// (the Explorer hides, Grep includes — see docs §1.2), so the wire conversion needs no
    /// kind context.
    Ignored {
        hide: bool,
    },
    /// Hidden-file visibility; same `hide` convention as `Ignored`.
    Hidden {
        hide: bool,
    },
    Changed,
}

impl ChipValue {
    /// True when `other` is the same *kind* of chip — what flag toggling and the dedup rules
    /// match on (a `Case(Sensitive)` and `Case(Insensitive)` are the same chip mid-cycle).
    fn same_kind(&self, other: &ChipValue) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// Normalize a committed glob. `None` means "don't keep a chip": empty input, or a degenerate
/// match-everything glob (`*`, `**`, also negated — `!*` would exclude *everything*, never
/// wanted). A glob starting with `.` (or `!.`) that contains no other glob syntax is treated
/// as an extension shorthand: `.rs` → `*.rs`, so the common case is three keystrokes.
pub fn normalize_glob(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (neg, body) = match trimmed.strip_prefix('!') {
        Some(b) => ("!", b),
        None => ("", trimmed),
    };
    if body.is_empty() || body == "*" || body == "**" {
        return None;
    }
    let extension_shorthand = body.starts_with('.') && !body.contains(['*', '?', '[', '/']);
    Some(if extension_shorthand {
        format!("{neg}*{body}")
    } else {
        format!("{neg}{body}")
    })
}

/// One rendered filter chip. Derived from `filters` on demand (never stored) so the chip row
/// can't drift from the filter state; canonical order is scope first, flags after.
#[derive(Debug, Clone)]
pub struct Chip {
    pub id: ChipId,
    pub label: String,
}

/// The editor line for a valued chip, revealed below the picker's input row. The dir editor
/// reads as a single `dir:` field: in multi-root projects a root segment (an inline typeahead —
/// type a prefix, Alt-j/k cycle the matches) leads, separated by `:` from the root-relative
/// path; single-root projects show only the path. The path segment carries directory-only
/// ghost suggestions in the save-as idiom, cached in `listing`. The glob editor is one field.
#[derive(Debug)]
pub struct ChipEditor {
    pub kind: ChipEditorKind,
    /// Which field has focus. Always `Path` for glob and single-root dir editors.
    pub field: ChipEditorField,
    /// The glob text / the root-relative directory path.
    pub input: crate::text_input::TextInput,
    /// Dir, multi-root: the prefix filter typed into the root field.
    pub root_filter: crate::text_input::TextInput,
    /// Dir, multi-root: highlight within [`root_candidates`]' matches for the current filter.
    /// Reset to the first match whenever the filter text changes; Alt-j/k cycle it (wrapping).
    pub root_selected: usize,
    /// Dir: the root the editor opened with — the fallback when the filter matches nothing.
    pub root_index: u32,
    /// Dir: cached `directory/list` entries (subdirectories only — files never complete a dir
    /// scope) for the dir portion of `input`. Powers the path field's ghost suggestions.
    pub listing: Vec<DirectoryEntry>,
    /// Dir: the absolute path `listing` was last synced against — the staleness key
    /// [`ChipEditor::sync_dir_listing`] compares to decide whether a refetch is due.
    pub listing_dir_abs: String,
    /// Dir: where `listing` stands relative to `listing_dir_abs` — the input to
    /// [`ChipEditor::path_valid`]: only a `Loaded` listing can vouch for the typed path.
    pub listing_state: DirListingState,
    /// Dir: position within the filtered match set producing the current path ghost. Reset on
    /// any path edit; Alt-j/k step it (clamped, like the save-as prompt).
    pub suggestion_idx: usize,
}

/// Lifecycle of the dir editor's suggestion listing. Distinguishing `Failed` from an empty
/// `Loaded` listing is what lets validity treat "the dir portion doesn't exist" differently
/// from "the dir exists but has no subdirectories".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirListingState {
    /// A fetch for `listing_dir_abs` is owed or in flight — validity is unknown, commits wait.
    Pending,
    /// `listing` reflects `listing_dir_abs`; the directory exists.
    Loaded,
    /// The fetch failed — the dir portion doesn't exist (or sits outside the project boundary).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipEditorKind {
    /// `edit: Some(i)` re-opens glob `i` pre-filled; `None` adds a new glob on commit.
    Glob { edit: Option<usize> },
    /// Same shape for the (equally repeatable) dir scopes: `Some(i)` edits entry `i` of
    /// `filters.directories`, `None` adds a new one on commit.
    Dir { edit: Option<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipEditorField {
    Root,
    Path,
}

impl ChipEditor {
    /// A glob editor: one field, no suggestion machinery.
    pub fn glob(prefill: String, edit: Option<usize>) -> Self {
        ChipEditor {
            kind: ChipEditorKind::Glob { edit },
            field: ChipEditorField::Path,
            input: crate::text_input::TextInput::new(prefill),
            root_filter: crate::text_input::TextInput::default(),
            root_selected: 0,
            root_index: 0,
            listing: Vec::new(),
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Pending,
            suggestion_idx: 0,
        }
    }

    /// A dir editor. `edit: Some(i)` re-opens dir scope `i` pre-filled; `None` adds a new one
    /// on commit. `listing_dir_abs` starts empty, so the caller's first
    /// [`ChipEditor::sync_dir_listing`] always reports a refetch is due.
    pub fn dir(path: String, field: ChipEditorField, root_index: u32, edit: Option<usize>) -> Self {
        ChipEditor {
            kind: ChipEditorKind::Dir { edit },
            field,
            input: crate::text_input::TextInput::new(path),
            root_filter: crate::text_input::TextInput::default(),
            // Empty filter → candidates are all roots in order, so the opening root's index
            // doubles as its position among them.
            root_selected: root_index as usize,
            root_index,
            listing: Vec::new(),
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Pending,
            suggestion_idx: 0,
        }
    }

    /// True for dir editors of either flavour (fresh add / editing an existing scope).
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, ChipEditorKind::Dir { .. })
    }

    /// The root the editor would commit: the highlighted candidate for the current filter,
    /// falling back to the root it opened with when the filter matches nothing. `labels` are
    /// the project's (disambiguated) root labels.
    pub fn chosen_root(&self, labels: &[String]) -> u32 {
        let candidates = root_candidates(labels, &self.root_filter.text);
        match candidates.get(self.root_selected.min(candidates.len().saturating_sub(1))) {
            Some(&i) => i as u32,
            None => self.root_index,
        }
    }

    /// The root field's ghost completion: the current match's root index and the part of its
    /// label beyond the typed prefix (rendered gray after the cursor, save-as style). `None`
    /// when nothing matches the typed prefix — the visible cue that a commit would fall back
    /// to the root the editor opened with.
    pub fn root_ghost(&self, labels: &[String]) -> Option<(usize, String)> {
        let candidates = root_candidates(labels, &self.root_filter.text);
        let &idx = candidates.get(self.root_selected.min(candidates.len().saturating_sub(1)))?;
        let typed_chars = self.root_filter.text.chars().count();
        let suffix: String = labels[idx].chars().skip(typed_chars).collect();
        Some((idx, suffix))
    }

    /// True when the root field holds a complete root label (the current match's ghost suffix
    /// is empty). This is what lets a typed `:` act as the root/path separator: complete value
    /// → confirm and move on; incomplete → swallowed.
    pub fn root_complete(&self, labels: &[String]) -> bool {
        self.root_ghost(labels)
            .is_some_and(|(_, suffix)| suffix.is_empty())
    }

    /// Confirm the root field (adopting the ghost completion) and move focus into the path.
    /// Shared by Tab, Alt-l, and `:`-on-a-complete-value. An *invalid* root refuses: focus
    /// stays on the (red) root field — the path can't be entered under a root that won't
    /// commit. Returns `true` when the listing went stale (the chosen root changed the dir
    /// the path resolves under) and the caller should refetch.
    pub fn commit_root_field(&mut self, labels: &[String], project_paths: &[String]) -> bool {
        let Some((idx, _)) = self.root_ghost(labels) else {
            return false; // no candidate ⇔ root_invalid — stay put
        };
        self.root_filter = crate::text_input::TextInput::new(labels[idx].clone());
        // The full label may still prefix-match several roots ("beta" vs "beta-api") —
        // keep the highlight on the adopted one.
        self.root_selected = root_candidates(labels, &self.root_filter.text)
            .iter()
            .position(|&c| c == idx)
            .unwrap_or(0);
        self.field = ChipEditorField::Path;
        self.sync_dir_listing(project_paths)
    }

    /// The absolute directory the path field's suggestions should list: the dir portion of the
    /// typed path (up to and including the last `/`), resolved under the chosen root. `None`
    /// for glob editors — and under an *invalid* root: fetching beneath the fallback root
    /// would surface suggestions that read as silently defaulting to it.
    pub fn dir_listing_path(&self, project_paths: &[String]) -> Option<String> {
        if !self.is_dir() {
            return None;
        }
        let root = if project_paths.len() > 1 {
            let labels = crate::labels::root_labels(project_paths);
            if self.root_invalid(&labels) {
                return None;
            }
            self.chosen_root(&labels)
        } else {
            0
        };
        Some(crate::save_prompt::join_root_relative(
            project_paths,
            root,
            crate::save_prompt::dir_of_input(&self.input.text),
        ))
    }

    /// Store a `directory/list` response, keeping only subdirectories — a file never completes
    /// a directory scope.
    pub fn set_dir_listing(&mut self, entries: Vec<DirectoryEntry>) {
        self.listing = entries.into_iter().filter(|e| e.is_dir).collect();
        self.listing_state = DirListingState::Loaded;
        self.suggestion_idx = 0;
    }

    /// Record that the `directory/list` fetch failed: the dir portion names a directory that
    /// doesn't exist (or one outside the project boundary). The path renders invalid and the
    /// commit gate refuses it until the next path change re-syncs.
    pub fn set_dir_listing_failed(&mut self) {
        self.listing.clear();
        self.listing_state = DirListingState::Failed;
        self.suggestion_idx = 0;
    }

    /// Reconcile the listing key with the current (root, dir-portion) pair. Returns `true`
    /// when they diverged — the listing was cleared and the caller should fire a fresh
    /// `directory/list` for [`ChipEditor::dir_listing_path`]. Call after any transition that
    /// can move the dir the path resolves under: path edits, root changes, segment pops.
    pub fn sync_dir_listing(&mut self, project_paths: &[String]) -> bool {
        let Some(abs) = self.dir_listing_path(project_paths) else {
            return false;
        };
        if abs == self.listing_dir_abs {
            return false;
        }
        self.listing_dir_abs = abs;
        self.listing.clear();
        self.listing_state = DirListingState::Pending;
        self.suggestion_idx = 0;
        true
    }

    /// True when the root field would refuse a commit: a non-empty filter that prefix-matches
    /// no root label. (An empty filter matches every root, so a fresh `Alt-d` → `Enter` still
    /// commits a whole-root scope.) The invalid field renders red in place of the old
    /// "(no match)" cue.
    pub fn root_invalid(&self, labels: &[String]) -> bool {
        root_candidates(labels, &self.root_filter.text).is_empty()
    }

    /// True when the path field holds a committable value: empty (whole-root scope / clear), or
    /// a path whose dir portion is vouched for by a `Loaded` listing and whose leaf is either
    /// empty (trailing `/`) or prefixes at least one listed subdirectory — a partial leaf
    /// commits as its highlighted completion (see [`ChipEditor::committed_path`]). A `Pending`
    /// listing can't vouch, so a commit racing the fetch waits rather than letting an
    /// unvalidated path through. Always true for glob editors.
    pub fn path_valid(&self) -> bool {
        if !self.is_dir() || self.input.text.is_empty() {
            return true;
        }
        if self.listing_state != DirListingState::Loaded {
            return false;
        }
        let leaf = crate::save_prompt::partial_of_input(&self.input.text);
        leaf.is_empty() || !crate::save_prompt::matching_indices(&self.listing, leaf).is_empty()
    }

    /// The path a commit should adopt: the typed text, with a partially typed leaf completed
    /// to the highlighted suggestion — Enter on a prefix selects the completion, mirroring the
    /// root segment, and the ghost shows exactly what will commit. The text comes back as
    /// typed when the leaf is empty, nothing matches, or the listing can't vouch.
    pub fn committed_path(&self) -> String {
        if !self.is_dir() || self.listing_state != DirListingState::Loaded {
            return self.input.text.clone();
        }
        let dir = crate::save_prompt::dir_of_input(&self.input.text);
        let leaf = crate::save_prompt::partial_of_input(&self.input.text);
        if leaf.is_empty() {
            return self.input.text.clone();
        }
        let matches = crate::save_prompt::matching_indices(&self.listing, leaf);
        match matches
            .get(self.suggestion_idx)
            .and_then(|&i| self.listing.get(i))
        {
            Some(entry) => format!("{dir}{}", entry.name),
            None => self.input.text.clone(),
        }
    }

    /// True when the path is *definitely* wrong — the red-worthy condition: the dir portion
    /// failed to list, or the loaded listing holds no directory the leaf even prefixes. The
    /// complement of [`ChipEditor::path_valid`] except under a `Pending` listing, which is
    /// neither committable nor flagged (unknown ≠ invalid; no red flash mid-fetch).
    pub fn path_invalid(&self) -> bool {
        if !self.is_dir() || self.input.text.is_empty() {
            return false;
        }
        match self.listing_state {
            DirListingState::Pending => false,
            DirListingState::Failed => true,
            DirListingState::Loaded => {
                let leaf = crate::save_prompt::partial_of_input(&self.input.text);
                !leaf.is_empty()
                    && crate::save_prompt::matching_indices(&self.listing, leaf).is_empty()
            }
        }
    }

    /// The path field's ghost: the rest of the current directory match beyond the partial leaf,
    /// plus the `/` that opens the next segment. Visible only with the cursor at the end of the
    /// input (matching the save-as prompt's rule).
    pub fn path_ghost(&self) -> Option<String> {
        if !self.is_dir() || self.input.cursor != self.input.text.len() {
            return None;
        }
        let partial = crate::save_prompt::partial_of_input(&self.input.text);
        let matches = crate::save_prompt::matching_indices(&self.listing, partial);
        let pick = *matches.get(self.suggestion_idx)?;
        let entry = self.listing.get(pick)?;
        let mut suffix: String = entry.name.chars().skip(partial.chars().count()).collect();
        suffix.push('/');
        Some(suffix)
    }

    /// Step the path ghost through the filtered matches (Alt-j/k), clamped at both ends like
    /// the save-as prompt.
    pub fn cycle_path_suggestion(&mut self, down: bool) {
        let partial = crate::save_prompt::partial_of_input(&self.input.text);
        let n = crate::save_prompt::matching_indices(&self.listing, partial).len();
        if n == 0 {
            return;
        }
        let sel = self.suggestion_idx.min(n - 1);
        self.suggestion_idx = if down {
            (sel + 1).min(n - 1)
        } else {
            sel.saturating_sub(1)
        };
    }

    /// Tab / Alt-l in the path field: absorb the ghost into the input. The suffix always ends
    /// in `/` (suggestions are directories), so the dir portion grew — returns `true` so the
    /// caller refetches and the next segment's suggestion appears. No-op without a ghost.
    pub fn accept_path_suggestion(&mut self, project_paths: &[String]) -> bool {
        let Some(suffix) = self.path_ghost() else {
            return false;
        };
        for c in suffix.chars() {
            self.input.insert_char(c);
        }
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Alt-Backspace in a non-empty path field: drop the rightmost segment, fish-style (the
    /// save-as gesture). Returns `true` when the dir portion shrank and a refetch is due.
    pub fn pop_path_segment(&mut self, project_paths: &[String]) -> bool {
        let popped = crate::save_prompt::pop_segment(&self.input.text);
        self.input.set(popped);
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Bookkeeping after a free-form edit to the path field (typed char, backspace, cursor
    /// moves don't matter but are harmless): reset the suggestion highlight and report whether
    /// the dir portion moved.
    pub fn path_edited(&mut self, project_paths: &[String]) -> bool {
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }
}

/// Indices of the root labels matching `filter` as a smartcase prefix (everything, on an empty
/// filter) — the root field's typeahead candidates, in root order. Matches the Explorer's
/// prefix-matching convention: case-insensitive unless the filter contains an uppercase letter.
pub fn root_candidates(labels: &[String], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..labels.len()).collect();
    }
    let sensitive = filter.chars().any(|c| c.is_uppercase());
    let needle: String = if sensitive {
        filter.to_string()
    } else {
        filter.chars().flat_map(char::to_lowercase).collect()
    };
    labels
        .iter()
        .enumerate()
        .filter(|(_, label)| {
            if sensitive {
                label.starts_with(needle.as_str())
            } else {
                label
                    .chars()
                    .flat_map(char::to_lowercase)
                    .collect::<String>()
                    .starts_with(needle.as_str())
            }
        })
        .map(|(i, _)| i)
        .collect()
}

/// Drill-down detail for one LSP server, shown in place of the LSP-servers list. Built client-side
/// from the highlighted picker row (which already carries the server's `status`, incl. a crash
/// message), so no extra server round-trip is needed. `status` and `progress` are refreshed live
/// from `lsp/status_changed` while the detail is open (matched by language + workspace root), with
/// `scroll` preserved across updates.
#[derive(Debug)]
pub struct LspServerDetail {
    pub name: String,
    pub language: String,
    pub workspace_root: String,
    pub status: LspStatus,
    /// Active `$/progress` operations, refreshed live while the detail is open.
    pub progress: Vec<LspProgress>,
    /// Scroll position of the (possibly long) detail body. Interior-mutable: the renderer records
    /// the geometry, the key handler reads it back to clamp (see [`ScrollState`]).
    pub scroll: ScrollState,
}

/// A staged delete awaiting `[y/N]` confirmation in the picker. The `item` it targets is matched
/// by [`item_key`] (not index) when rendering, so a background re-rank can't smear the prompt onto
/// the wrong row.
#[derive(Debug, Clone)]
pub struct PendingDelete {
    pub action: PendingDeleteAction,
    /// The picker row the prompt renders over.
    pub item: PickerItem,
    /// Noun for the prompt — `"project"`, `"file"`, or `"directory"`.
    pub noun: &'static str,
    /// Display name shown inside the quotes in the prompt.
    pub name: String,
}

/// What a confirmed picker delete actually does.
#[derive(Debug, Clone)]
pub enum PendingDeleteAction {
    /// `project/delete { name }`.
    Project(String),
    /// `path/delete { path }` — the absolute path of a file or directory.
    Path(String),
}

impl PickerState {
    /// Render the chip row: the stored list, verbatim — insertion order *is* the storage
    /// order, so row index = storage index. Labels are compact: the dir chip is just the path
    /// with a trailing `/` (the slash implies "directory"; multi-root labels lead with the
    /// root's basename), and the flags are two-or-three-char abbreviations (only `wd`
    /// underlines — it reads as a stray token otherwise). The ignored/hidden chips render `+`
    /// (include — Grep) or `-` (hide — Explorer) per the direction stored in the value.
    pub fn chips(&self, project_paths: &[String]) -> Vec<Chip> {
        self.chips
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let (id, label) = match v {
                    ChipValue::Dir(d) => {
                        // Multi-root scopes read like the status bar: `{root label}:
                        // {path}/`, with the same disambiguated root labels. An empty
                        // relative path is a whole-root scope — just the label.
                        let label = if project_paths.len() > 1 {
                            let labels = crate::labels::root_labels(project_paths);
                            let root_label = labels
                                .get(d.path_index as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("?");
                            if d.relative_path.is_empty() {
                                root_label.to_string()
                            } else {
                                format!("{root_label}: {}/", d.relative_path)
                            }
                        } else {
                            format!("{}/", d.relative_path)
                        };
                        (ChipId::Dir(i), label)
                    }
                    ChipValue::Glob(g) => (ChipId::Glob(i), g.clone()),
                    ChipValue::Case(CaseMode::Insensitive) => (ChipId::Case, "aa".into()),
                    ChipValue::Case(_) => (ChipId::Case, "Aa".into()),
                    ChipValue::Word => (ChipId::Word, "wd".into()),
                    ChipValue::Lit => (ChipId::Lit, "lit".into()),
                    ChipValue::Ignored { hide } => {
                        (ChipId::Ignored, if *hide { "-ig" } else { "+ig" }.into())
                    }
                    ChipValue::Hidden { hide } => {
                        (ChipId::Hidden, if *hide { "-." } else { "+." }.into())
                    }
                    ChipValue::Changed => (ChipId::Changed, "Δ".into()),
                };
                Chip { id, label }
            })
            .collect()
    }

    /// Fold the chip list into the wire format — the normalized, unordered `PickerFilters`
    /// sent with every `picker/query`/`picker/view`.
    pub fn wire_filters(&self) -> PickerFilters {
        let mut f = PickerFilters::default();
        for v in &self.chips {
            match v {
                ChipValue::Dir(d) => f.directories.push(d.clone()),
                ChipValue::Glob(g) => f.globs.push(g.clone()),
                ChipValue::Case(mode) => f.case = *mode,
                ChipValue::Word => f.whole_word = true,
                ChipValue::Lit => f.fixed_string = true,
                ChipValue::Ignored { hide: true } => f.hide_ignored = true,
                ChipValue::Ignored { hide: false } => f.include_ignored = true,
                ChipValue::Hidden { hide: true } => f.hide_hidden = true,
                ChipValue::Hidden { hide: false } => f.include_hidden = true,
                ChipValue::Changed => f.changed_only = true,
            }
        }
        f
    }

    /// Adopt a wire filter set (open/resume — `PickerViewResult::filters`), replacing the chip
    /// list. The wire carries no order, so restored chips come back in canonical order (dirs,
    /// globs, flags); everything added afterwards appends behind them — insertion order is
    /// session-ephemeral.
    pub fn adopt_filters(&mut self, f: &PickerFilters) {
        let mut chips: Vec<ChipValue> = Vec::new();
        chips.extend(f.directories.iter().cloned().map(ChipValue::Dir));
        chips.extend(f.globs.iter().cloned().map(ChipValue::Glob));
        if f.case != CaseMode::Smart {
            chips.push(ChipValue::Case(f.case));
        }
        if f.whole_word {
            chips.push(ChipValue::Word);
        }
        if f.fixed_string {
            chips.push(ChipValue::Lit);
        }
        if f.include_ignored || f.hide_ignored {
            chips.push(ChipValue::Ignored {
                hide: f.hide_ignored,
            });
        }
        if f.include_hidden || f.hide_hidden {
            chips.push(ChipValue::Hidden {
                hide: f.hide_hidden,
            });
        }
        if f.changed_only {
            chips.push(ChipValue::Changed);
        }
        self.chips = chips;
    }

    /// The dir scope behind chip `i`, when chip `i` is a dir — the editor's pre-fill.
    pub fn dir_value(&self, i: usize) -> Option<&ScopedPath> {
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

    /// Toggle/cycle the filter a flag chip stands for: booleans flip (appearing appends,
    /// disappearing drops out); `case` cycles smart → sensitive → insensitive → smart *in
    /// place* while the chip stays visible. The ignored/hidden chips record the per-kind
    /// direction (`explorer` hides; everything else includes) in the value at creation time.
    /// Returns `false` for the valued chips (dir, glob — those go through their editors).
    pub fn apply_chip_toggle(&mut self, id: ChipId, explorer: bool) -> bool {
        let value = match id {
            ChipId::Case => {
                // Cycle in place: absent → Sensitive (append), Sensitive → Insensitive
                // (stays put), Insensitive → absent.
                let pos = self
                    .chips
                    .iter()
                    .position(|v| matches!(v, ChipValue::Case(_)));
                match pos {
                    None => self.chips.push(ChipValue::Case(CaseMode::Sensitive)),
                    Some(i) => match self.chips[i] {
                        ChipValue::Case(CaseMode::Sensitive) => {
                            self.chips[i] = ChipValue::Case(CaseMode::Insensitive);
                        }
                        _ => {
                            self.chips.remove(i);
                        }
                    },
                }
                return true;
            }
            ChipId::Word => ChipValue::Word,
            ChipId::Lit => ChipValue::Lit,
            ChipId::Ignored => ChipValue::Ignored { hide: explorer },
            ChipId::Hidden => ChipValue::Hidden { hide: explorer },
            ChipId::Changed => ChipValue::Changed,
            ChipId::Dir(_) | ChipId::Glob(_) => return false,
        };
        match self.chips.iter().position(|v| v.same_kind(&value)) {
            Some(i) => {
                self.chips.remove(i);
            }
            None => self.chips.push(value),
        }
        true
    }

    /// Apply a glob editor commit: `None` clears the glob being edited (or cancels when it
    /// wasn't editing one); duplicates collapse — committing an existing glob is a no-op (the
    /// chip already says it), editing into one drops the edited entry; an in-place edit keeps
    /// its position in the row. `edit` indexes the chip list. Returns whether the filters
    /// changed (the caller follows up with the filter-change RPC).
    pub fn commit_glob_edit(&mut self, normalized: Option<String>, edit: Option<usize>) -> bool {
        let edit = edit.filter(|&i| matches!(self.chips.get(i), Some(ChipValue::Glob(_))));
        let Some(g) = normalized else {
            return match edit {
                Some(i) => {
                    self.chips.remove(i);
                    true
                }
                None => false, // empty/useless new glob — treat as cancel
            };
        };
        let value = ChipValue::Glob(g);
        match edit {
            Some(i) => {
                if self
                    .chips
                    .iter()
                    .enumerate()
                    .any(|(j, v)| j != i && *v == value)
                {
                    self.chips.remove(i);
                } else {
                    self.chips[i] = value;
                }
                true
            }
            None => {
                if self.chips.contains(&value) {
                    false // already present — the chip says it; nothing to change
                } else {
                    self.chips.push(value);
                    true
                }
            }
        }
    }

    /// Apply a dir editor commit — same shape as [`PickerState::commit_glob_edit`]: `None`
    /// clears the scope being edited (or cancels when adding), duplicates collapse, in-place
    /// edits keep their position. `edit` indexes the chip list. Returns whether the filters
    /// changed.
    pub fn commit_dir_edit(&mut self, value: Option<ScopedPath>, edit: Option<usize>) -> bool {
        let edit = edit.filter(|&i| matches!(self.chips.get(i), Some(ChipValue::Dir(_))));
        let Some(d) = value else {
            return match edit {
                Some(i) => {
                    self.chips.remove(i);
                    true
                }
                None => false, // empty new scope in a single-root project — cancel
            };
        };
        let value = ChipValue::Dir(d);
        match edit {
            Some(i) => {
                if self
                    .chips
                    .iter()
                    .enumerate()
                    .any(|(j, v)| j != i && *v == value)
                {
                    self.chips.remove(i);
                } else {
                    self.chips[i] = value;
                }
                true
            }
            None => {
                if self.chips.contains(&value) {
                    false
                } else {
                    self.chips.push(value);
                    true
                }
            }
        }
    }

    /// Remove the chip — it disappears from the row and from the next `wire_filters()` fold.
    /// The caller follows up with a filter-change RPC.
    pub fn remove_chip(&mut self, id: ChipId) {
        match id {
            ChipId::Dir(i) | ChipId::Glob(i) => {
                if i < self.chips.len() {
                    self.chips.remove(i);
                }
            }
            ChipId::Case => self.chips.retain(|v| !matches!(v, ChipValue::Case(_))),
            ChipId::Word => self.chips.retain(|v| *v != ChipValue::Word),
            ChipId::Lit => self.chips.retain(|v| *v != ChipValue::Lit),
            ChipId::Ignored => self
                .chips
                .retain(|v| !matches!(v, ChipValue::Ignored { .. })),
            ChipId::Hidden => self
                .chips
                .retain(|v| !matches!(v, ChipValue::Hidden { .. })),
            ChipId::Changed => self.chips.retain(|v| *v != ChipValue::Changed),
        }
    }

    /// Apply a push from the server. Returns `true` if the push was for the current
    /// `(generation, offset)` and the UI should redraw. Accepts pushes whose offset equals
    /// either the currently-applied offset OR the pending offset (the result of an in-flight
    /// refetch); in the latter case it shifts `visible_start` and `selected` so the user's
    /// position in the result set is preserved across the cache swap.
    pub fn apply_update(&mut self, p: PickerUpdateParams) -> bool {
        if Some(p.kind) != self.kind {
            return false;
        }
        if p.generation != self.generation {
            return false;
        }
        let shift: i64 = if p.offset == self.offset {
            0
        } else if Some(p.offset) == self.pending_offset {
            let s = p.offset as i64 - self.offset as i64;
            self.offset = p.offset;
            self.pending_offset = None;
            s
        } else {
            return false;
        };
        self.items = p.items;
        // The server's push never contains our client-side synthetic row, so any cached
        // `synthetic_create_idx` is stale relative to the new `items` Vec. Drop it before the
        // recompute below decides whether to re-add — otherwise the strip-by-index logic could
        // remove a real entry at the same position.
        self.synthetic_create_idx = None;
        self.total_matches = p.total_matches;
        self.total_candidates = p.total_candidates;
        self.ticking = p.ticking;
        self.total_display_rows = p.grep_total_display_rows;

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

        // Fresh-open default highlight: the first item that isn't the client's active
        // buffer/project. Falls back to 0 when every item is the active one (e.g. a single
        // open buffer). One-shot — the first push with items decides and clears it.
        if self.resume_target.is_none() && !self.items.is_empty() {
            if let Some(skip) = self.default_skip.take() {
                self.selected = self
                    .items
                    .iter()
                    .position(|item| !skip.matches(item))
                    .unwrap_or(0);
            }
        }
        self.recompute_synthetic_create_row();
        true
    }

    /// Recompute the synthetic "Create …" row. Called after any change to `items` or `query`.
    /// Appends a row labeled `Create <kind> "<query>"` to `items` when the query is non-empty
    /// and doesn't exactly match an existing entry — Projects use it to create new projects,
    /// and Explorer uses it to create a new file (or directory, when the query ends with `/`)
    /// at the current directory. The italic styling at render time is what signals "this is
    /// an action, not an entry"; we don't need a leading `+` decoration to convey the same.
    /// Idempotent: strips any prior synthetic row first.
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
                    name: format!("Create project \"{q}\""),
                    match_indices: Vec::new(),
                }
            }
            Some(PickerKind::Explorer) => {
                // A trailing `/` switches the synthetic from "Create file …" to "Create
                // directory …". We strip it once to get the base name. Multiple-segment
                // paths (`foo/bar/`, `foo/bar.rs`) are allowed — the server's
                // `directory/create` and `buffer/open { create_if_missing }` both mkdir-p the
                // intermediate dirs, so the synthetic just hands them the full relative path.
                let (base, dir_intent) = match q.strip_suffix('/') {
                    Some(stripped) => (stripped, true),
                    None => (q, false),
                };
                if base.is_empty() {
                    return;
                }
                // Validate each segment: no empty (rules out `foo//bar`), no `.` or `..`
                // (rules out `./`, `..`, `foo/./bar`, `foo/../bar` — `.` and `..` aren't
                // legal filenames and the server's boundary check would reject the traversal
                // anyway). Catches `.` and `./` as the most common confusions.
                let segments_valid = base
                    .split('/')
                    .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
                if !segments_valid {
                    return;
                }
                // Single-segment names: the items vec covers the current dir, so we can check
                // whether the typed name already exists and suppress the synthetic. Multi-
                // segment names refer to paths outside the current dir's listing — we trust
                // the server (mkdir-p is idempotent, and `buffer/open` opens an existing file
                // when `create_if_missing` finds one).
                if !base.contains('/') {
                    let already_exists = self.items.iter().any(|item| match item {
                        PickerItem::DirEntry { name, .. } => name == base,
                        _ => false,
                    });
                    if already_exists {
                        return;
                    }
                }
                let label = if dir_intent {
                    // The word "directory" already signals what's getting created; no need
                    // for a trailing `/` to disambiguate. Keeping `is_dir: false` below so
                    // the row picks up the same neutral white styling as the file variant —
                    // it's an *action* affordance, not a real entry.
                    format!("Create directory \"{base}\"")
                } else {
                    format!("Create file \"{base}\"")
                };
                PickerItem::DirEntry {
                    name: label,
                    // Always `false` — see comment above. The selector routes via
                    // `synthetic_create_idx` + the trailing-slash check in the query, so this
                    // flag never reaches the navigate-into-dir path in `select_picker_item`.
                    is_dir: false,
                    match_indices: Vec::new(),
                    // A synthetic action affordance, not a filesystem entry — never coloured.
                    git_status: None,
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
    Root {
        path_index: u32,
    },
    Diagnostic {
        line: u32,
        col: u32,
    },
    /// A reference location, identified by `(path, line, col)` — stable across resume even if the
    /// preview line text drifts after editing (mirrors the Grep key's rationale).
    Reference {
        path: &'a str,
        line: u32,
        col: u32,
    },
    /// An LSP server, identified by its `(language, workspace_root)` key — stable across the
    /// status changes that drive the picker's live re-pushes.
    LspServer {
        language: &'a str,
        workspace_root: &'a str,
    },
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
        PickerItem::Diagnostic { line, col, .. } => ItemKey::Diagnostic {
            line: *line,
            col: *col,
        },
        PickerItem::Reference {
            path, line, col, ..
        } => ItemKey::Reference {
            path: path.as_str(),
            line: *line,
            col: *col,
        },
        PickerItem::LspServer {
            language,
            workspace_root,
            ..
        } => ItemKey::LspServer {
            language: language.as_str(),
            workspace_root: workspace_root.as_str(),
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
            git_status: None,
        }
    }

    fn empty_state(kind: PickerKind, query: &str) -> PickerState {
        PickerState {
            open: true,
            kind: Some(kind),
            query: TextInput::new(query),
            ..PickerState::default()
        }
    }

    fn buffer_item(id: u64) -> PickerItem {
        PickerItem::Buffer {
            buffer_id: id,
            display: format!("buf{id}"),
            status: Default::default(),
            path_index: None,
            relative_path: None,
            match_indices: Vec::new(),
            transient: false,
        }
    }

    fn project_item(name: &str) -> PickerItem {
        PickerItem::Project {
            name: name.into(),
            match_indices: Vec::new(),
        }
    }

    /// Push `items` into `s` the way a server update would (generation/offset 0, not ticking).
    fn push_items(s: &mut PickerState, kind: PickerKind, items: Vec<PickerItem>) {
        let n = items.len() as u32;
        assert!(s.apply_update(PickerUpdateParams {
            kind,
            generation: 0,
            offset: 0,
            items,
            total_matches: n,
            total_candidates: n,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }));
    }

    #[test]
    fn buffers_default_highlight_skips_active_buffer_at_top() {
        let mut s = empty_state(PickerKind::Buffers, "");
        s.default_skip = Some(DefaultSkip::Buffer(7));
        push_items(
            &mut s,
            PickerKind::Buffers,
            vec![buffer_item(7), buffer_item(3), buffer_item(9)],
        );
        assert_eq!(
            s.selected, 1,
            "active buffer leads the MRU → flip target is row 1"
        );
        assert!(s.default_skip.is_none(), "one-shot: cleared once applied");
    }

    #[test]
    fn buffers_default_highlight_when_another_client_owns_the_top_row() {
        let mut s = empty_state(PickerKind::Buffers, "");
        s.default_skip = Some(DefaultSkip::Buffer(7));
        // Another client touched buffer 3 last, pushing our active buffer (7) off the top.
        push_items(
            &mut s,
            PickerKind::Buffers,
            vec![buffer_item(3), buffer_item(7), buffer_item(9)],
        );
        assert_eq!(s.selected, 0, "row 0 already isn't the active buffer");
    }

    #[test]
    fn buffers_default_highlight_with_only_the_active_buffer() {
        let mut s = empty_state(PickerKind::Buffers, "");
        s.default_skip = Some(DefaultSkip::Buffer(7));
        push_items(&mut s, PickerKind::Buffers, vec![buffer_item(7)]);
        assert_eq!(s.selected, 0, "nothing to flip to → fall back to the top");
        assert!(s.default_skip.is_none());
    }

    #[test]
    fn buffers_default_skip_survives_an_empty_push() {
        let mut s = empty_state(PickerKind::Buffers, "");
        s.default_skip = Some(DefaultSkip::Buffer(7));
        push_items(&mut s, PickerKind::Buffers, vec![]);
        assert!(
            s.default_skip.is_some(),
            "no items yet — keep waiting for a push that has some"
        );
        push_items(
            &mut s,
            PickerKind::Buffers,
            vec![buffer_item(7), buffer_item(3)],
        );
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn projects_default_highlight_skips_active_project() {
        let mut s = empty_state(PickerKind::Projects, "");
        s.default_skip = Some(DefaultSkip::Project("beta".into()));
        push_items(
            &mut s,
            PickerKind::Projects,
            vec![
                project_item("alpha"),
                project_item("beta"),
                project_item("gamma"),
            ],
        );
        assert_eq!(
            s.selected, 0,
            "alpha isn't the active project — no need to skip"
        );

        let mut s = empty_state(PickerKind::Projects, "");
        s.default_skip = Some(DefaultSkip::Project("alpha".into()));
        push_items(
            &mut s,
            PickerKind::Projects,
            vec![
                project_item("alpha"),
                project_item("beta"),
                project_item("gamma"),
            ],
        );
        assert_eq!(s.selected, 1, "step over the active project at the top");
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
                assert_eq!(name, "Create file \"newfile.rs\"");
                assert!(
                    !is_dir,
                    "synthetic row is always rendered as a neutral action"
                );
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
    fn explorer_synthetic_allows_multi_segment_paths() {
        // Multi-segment names are allowed; the server's mkdir-p creates intermediate dirs at
        // commit time. The items existence check is skipped (it only covers the current dir).
        let mut s = empty_state(PickerKind::Explorer, "subdir/newfile.rs");
        s.items = vec![dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 2);
        match s.items.last().unwrap() {
            PickerItem::DirEntry { name, .. } => {
                assert_eq!(name, "Create file \"subdir/newfile.rs\"");
            }
            other => panic!("expected DirEntry, got {other:?}"),
        }
    }

    #[test]
    fn explorer_synthetic_allows_multi_segment_dirs() {
        let mut s = empty_state(PickerKind::Explorer, "subdir/inner/");
        s.items = Vec::new();
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1);
        match s.items.last().unwrap() {
            PickerItem::DirEntry { name, .. } => {
                assert_eq!(name, "Create directory \"subdir/inner\"");
            }
            other => panic!("expected DirEntry, got {other:?}"),
        }
    }

    #[test]
    fn explorer_synthetic_suppressed_for_dot_segments() {
        // `.`, `./`, `..`, `../`, `foo/./bar`, `foo/../bar`, `foo//bar` — none of these are
        // legal filenames; the synthetic shouldn't tempt the user into trying.
        for query in [
            ".",
            "./",
            "..",
            "../",
            "foo/./bar",
            "foo/../bar",
            "foo//bar",
        ] {
            let mut s = empty_state(PickerKind::Explorer, query);
            s.items = Vec::new();
            s.recompute_synthetic_create_row();
            assert!(
                s.synthetic_create_idx.is_none(),
                "synthetic should be suppressed for {query:?}"
            );
        }
    }

    #[test]
    fn explorer_synthetic_switches_to_dir_intent_on_trailing_slash() {
        let mut s = empty_state(PickerKind::Explorer, "newdir/");
        s.items = vec![dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 2);
        match s.items.last().unwrap() {
            PickerItem::DirEntry { name, is_dir, .. } => {
                assert_eq!(name, "Create directory \"newdir\"");
                assert!(
                    !is_dir,
                    "synthetic stays `is_dir: false` so it inherits the neutral file styling"
                );
            }
            other => panic!("expected DirEntry, got {other:?}"),
        }
    }

    #[test]
    fn explorer_synthetic_dir_suppressed_when_name_already_exists() {
        // An existing file blocks a dir-create with the same base name (filesystem would
        // refuse anyway). Same in reverse: an existing dir blocks file creation.
        let mut s = empty_state(PickerKind::Explorer, "src/");
        s.items = vec![dir_entry("src", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1);
        assert!(s.synthetic_create_idx.is_none());
    }

    #[test]
    fn explorer_synthetic_dir_suppressed_against_existing_dir_entry() {
        // Regression: when the user types `newdir/`, the client strips the trailing slash
        // before filtering server-side — so the existing `newdir` entry stays in items, and
        // the recompute must spot it and suppress the synthetic. (Before the fix, the slash
        // made it to the server's prefix filter, hid `newdir` from items, and the synthetic
        // was offered for a directory that already existed.)
        let mut s = empty_state(PickerKind::Explorer, "newdir/");
        s.items = vec![dir_entry("newdir", true)];
        s.recompute_synthetic_create_row();
        assert_eq!(s.items.len(), 1, "no synthetic when the dir already exists");
        assert!(s.synthetic_create_idx.is_none());
    }

    #[test]
    fn explorer_synthetic_dir_suppressed_when_only_slash() {
        // "/" alone strips to empty — nothing to create.
        let mut s = empty_state(PickerKind::Explorer, "/");
        s.items = Vec::new();
        s.recompute_synthetic_create_row();
        assert!(s.items.is_empty());
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
            PickerItem::DirEntry { name, .. } => assert_eq!(name, "Create file \"other.rs\""),
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
        let ok = s.apply_update(PickerUpdateParams {
            kind: PickerKind::Explorer,
            generation: 7,
            offset: 0,
            items: vec![
                dir_entry("a.rs", false),
                dir_entry("b.rs", false),
                dir_entry("c.rs", false),
            ],
            total_matches: 3,
            total_candidates: 3,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        });
        assert!(ok);
        // Items should be [a.rs, b.rs, c.rs, "Create file …"] — the synthetic re-added
        // without having removed `b.rs` (which would happen if the stale idx=1 was used).
        assert_eq!(s.items.len(), 4);
        let names: Vec<&str> = s
            .items
            .iter()
            .map(|i| match i {
                PickerItem::DirEntry { name, .. } => name.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(names, vec!["a.rs", "b.rs", "c.rs", "Create file \"ne\""]);
    }

    #[test]
    fn adopted_filters_derive_chips_in_canonical_order() {
        use aether_protocol::picker::{CaseMode, PickerFilters, ScopedPath};
        // The wire carries no order, so adoption (open/resume) lays chips out canonically:
        // dirs, globs (each in declaration order), flags after. Multi-root dir labels lead
        // with the root's basename. Round-tripping back to the wire preserves the set.
        let mut s = empty_state(PickerKind::Grep, "");
        let wire = PickerFilters {
            changed_only: true,
            whole_word: true,
            globs: vec!["*.rs".into(), "!*_test.rs".into()],
            case: CaseMode::Insensitive,
            include_hidden: true,
            directories: vec![
                ScopedPath {
                    path_index: 1,
                    relative_path: "src/app".into(),
                },
                ScopedPath {
                    path_index: 0,
                    relative_path: "docs".into(),
                },
            ],
            ..Default::default()
        };
        s.adopt_filters(&wire);
        let roots = vec!["/proj/alpha".to_string(), "/proj/beta".to_string()];
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            vec![
                "beta: src/app/",
                "alpha: docs/",
                "*.rs",
                "!*_test.rs",
                "aa",
                "wd",
                "+.",
                "Δ"
            ]
        );
        assert_eq!(s.wire_filters(), wire, "wire → chips → wire round-trips");
        // A whole-root scope is a dir chip with an empty relative path.
        s.adopt_filters(&PickerFilters {
            directories: vec![ScopedPath {
                path_index: 1,
                relative_path: String::new(),
            }],
            ..Default::default()
        });
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels[0], "beta");
    }

    #[test]
    fn chips_follow_insertion_order() {
        use aether_protocol::picker::ScopedPath;
        let roots = vec!["/proj".to_string()];
        let mut s = empty_state(PickerKind::Grep, "");
        // Add in an order canonical sorting would reshuffle: flag, glob, dir, flag.
        assert!(s.apply_chip_toggle(ChipId::Word, false));
        assert!(s.commit_glob_edit(Some("*.rs".into()), None));
        assert!(s.commit_dir_edit(
            Some(ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
            }),
            None,
        ));
        assert!(s.apply_chip_toggle(ChipId::Changed, false));
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["wd", "*.rs", "src/", "Δ"]);
        // Removing a middle chip keeps the rest in place; re-adding appends (a new insertion).
        let glob_id = s.chips(&roots)[1].id;
        s.remove_chip(glob_id);
        assert!(s.commit_glob_edit(Some("*.rs".into()), None));
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["wd", "src/", "Δ", "*.rs"]);
        // Editing a chip in place keeps its position (`edit` indexes the chip row — the dir
        // sits at row 1).
        assert!(s.commit_dir_edit(
            Some(ScopedPath {
                path_index: 0,
                relative_path: "docs".into(),
            }),
            Some(1),
        ));
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["wd", "docs/", "Δ", "*.rs"]);
        // Case cycling keeps the chip's position while it stays visible.
        assert!(s.apply_chip_toggle(ChipId::Case, false)); // smart → sensitive: appears, appends
        assert!(s.apply_chip_toggle(ChipId::Case, false)); // sensitive → insensitive: stays put
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["wd", "docs/", "Δ", "*.rs", "aa"]);
    }

    #[test]
    fn additions_append_behind_adopted_chips() {
        use aether_protocol::picker::{PickerFilters, ScopedPath};
        let roots = vec!["/proj".to_string()];
        let mut s = empty_state(PickerKind::Grep, "");
        s.adopt_filters(&PickerFilters {
            changed_only: true,
            globs: vec!["*.rs".into()],
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
            }],
            ..Default::default()
        });
        assert!(s.apply_chip_toggle(ChipId::Word, false));
        let labels: Vec<String> = s.chips(&roots).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["src/", "*.rs", "Δ", "wd"]);
    }

    #[test]
    fn commit_glob_edit_collapses_duplicates() {
        let mut s = empty_state(PickerKind::Grep, "");
        assert!(s.commit_glob_edit(Some("*.rs".into()), None));
        // Committing the same glob again is a no-op — the chip already says it.
        assert!(!s.commit_glob_edit(Some("*.rs".into()), None));
        assert_eq!(s.wire_filters().globs, vec!["*.rs".to_string()]);
        // Editing another glob *into* an existing one drops the edited entry.
        assert!(s.commit_glob_edit(Some("*.md".into()), None));
        assert!(s.commit_glob_edit(Some("*.rs".into()), Some(1)));
        assert_eq!(s.wire_filters().globs, vec!["*.rs".to_string()]);
    }

    #[test]
    fn chips_ignored_hidden_direction_follows_kind() {
        // The toggle records the per-kind direction in the value: the Explorer hides
        // (its listing shows ignored/hidden by default), everything else includes — and the
        // wire fold lands on the matching field.
        let mut s = empty_state(PickerKind::Explorer, "");
        assert!(s.apply_chip_toggle(ChipId::Ignored, true));
        assert!(s.apply_chip_toggle(ChipId::Hidden, true));
        let labels: Vec<String> = s.chips(&[]).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["-ig", "-."]);
        assert!(s.wire_filters().hide_ignored);
        assert!(!s.wire_filters().include_ignored);
        let mut g = empty_state(PickerKind::Grep, "");
        assert!(g.apply_chip_toggle(ChipId::Ignored, false));
        let labels: Vec<String> = g.chips(&[]).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, vec!["+ig"]);
        assert!(g.wire_filters().include_ignored);
    }

    #[test]
    fn root_candidates_filter_by_smartcase_prefix() {
        let labels: Vec<String> = ["beta", "beta-api", "Backend", "core"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Empty filter: every root, in order.
        assert_eq!(root_candidates(&labels, ""), vec![0, 1, 2, 3]);
        // Lowercase filter is case-insensitive (smartcase): matches Backend too.
        assert_eq!(root_candidates(&labels, "b"), vec![0, 1, 2]);
        assert_eq!(root_candidates(&labels, "beta-"), vec![1]);
        // An uppercase letter flips to case-sensitive.
        assert_eq!(root_candidates(&labels, "B"), vec![2]);
        assert!(root_candidates(&labels, "zzz").is_empty());
    }

    #[test]
    fn chip_editor_chosen_root_follows_filter_and_falls_back() {
        let labels: Vec<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 1, None);
        // Empty filter: candidates are all roots; selection picks by position.
        ed.root_selected = 2;
        assert_eq!(ed.chosen_root(&labels), 2);
        // Filter narrows; selection indexes the *matches*.
        ed.root_filter = TextInput::new("g");
        ed.root_selected = 0;
        assert_eq!(ed.chosen_root(&labels), 2);
        // No match: fall back to the root the editor opened with.
        ed.root_filter = TextInput::new("zzz");
        assert_eq!(ed.chosen_root(&labels), 1);
    }

    #[test]
    fn normalize_glob_extension_shorthand_and_degenerates() {
        // Extension shorthand: a leading dot with no other glob syntax grows a `*`.
        assert_eq!(normalize_glob(".rs").as_deref(), Some("*.rs"));
        assert_eq!(normalize_glob("!.rs").as_deref(), Some("!*.rs"));
        // Real globs and paths pass through untouched.
        assert_eq!(normalize_glob("*.rs").as_deref(), Some("*.rs"));
        assert_eq!(normalize_glob("src/**").as_deref(), Some("src/**"));
        assert_eq!(normalize_glob(".config/*").as_deref(), Some(".config/*"));
        assert_eq!(normalize_glob("!*.md").as_deref(), Some("!*.md"));
        // Degenerate match-everything globs (and empties) are dropped — `!*` would exclude
        // everything, never wanted.
        assert_eq!(normalize_glob(""), None);
        assert_eq!(normalize_glob("  "), None);
        assert_eq!(normalize_glob("*"), None);
        assert_eq!(normalize_glob("**"), None);
        assert_eq!(normalize_glob("!*"), None);
        assert_eq!(normalize_glob("!**"), None);
        assert_eq!(normalize_glob("!"), None);
    }

    #[test]
    fn chip_editor_root_ghost_is_match_suffix() {
        let labels: Vec<String> = ["alpha", "beta", "beta-api"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 0, None);
        ed.root_filter = TextInput::new("be");
        ed.root_selected = 0;
        // First match for "be" is beta: ghost completes the rest of the label.
        assert_eq!(ed.root_ghost(&labels), Some((1, "ta".to_string())));
        // Cycling to the next match swaps the ghost.
        ed.root_selected = 1;
        assert_eq!(ed.root_ghost(&labels), Some((2, "ta-api".to_string())));
        // No match: no ghost (the commit falls back to root_index).
        ed.root_filter = TextInput::new("zzz");
        assert_eq!(ed.root_ghost(&labels), None);
    }

    #[test]
    fn remove_chip_resets_the_named_filter() {
        use aether_protocol::picker::{CaseMode, PickerFilters};
        let mut s = empty_state(PickerKind::Grep, "");
        s.adopt_filters(&PickerFilters {
            case: CaseMode::Sensitive,
            whole_word: true,
            globs: vec!["*.rs".into(), "!*.md".into()],
            ..Default::default()
        });
        // Canonical adoption order: globs first, then flags — chip 0 is "*.rs".
        s.remove_chip(ChipId::Glob(0));
        assert_eq!(s.wire_filters().globs, vec!["!*.md".to_string()]);
        s.remove_chip(ChipId::Case);
        assert_eq!(s.wire_filters().case, CaseMode::Smart);
        s.remove_chip(ChipId::Word);
        assert!(!s.wire_filters().whole_word);
        // Out-of-range glob removal is a no-op, not a panic (the chip row may have re-derived).
        s.remove_chip(ChipId::Glob(7));
        assert_eq!(s.wire_filters().globs, vec!["!*.md".to_string()]);
        s.remove_chip(ChipId::Glob(0));
        assert!(s.chips.is_empty());
        assert!(s.wire_filters().is_default());
    }

    fn listing_entry(name: &str, is_dir: bool) -> aether_protocol::directory::DirectoryEntry {
        aether_protocol::directory::DirectoryEntry {
            name: name.into(),
            is_dir,
        }
    }

    #[test]
    fn dir_editor_path_ghost_suggests_directories_only() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        assert!(
            ed.sync_dir_listing(&paths),
            "fresh editor owes a listing fetch"
        );
        assert_eq!(ed.listing_dir_abs, "/proj");
        ed.set_dir_listing(vec![
            listing_entry("README.md", false),
            listing_entry("src", true),
            listing_entry("tests", true),
        ]);
        // Files are dropped at ingest; the ghost completes the first directory.
        assert_eq!(ed.path_ghost().as_deref(), Some("src/"));
        ed.input = TextInput::new("t");
        assert!(
            !ed.path_edited(&paths),
            "leaf edit doesn't move the dir portion"
        );
        assert_eq!(ed.path_ghost().as_deref(), Some("ests/"));
        // No match → no ghost.
        ed.input = TextInput::new("zzz");
        ed.path_edited(&paths);
        assert_eq!(ed.path_ghost(), None);
    }

    #[test]
    fn dir_editor_path_ghost_hidden_when_cursor_not_at_end() {
        let mut ed = ChipEditor::dir("s".into(), ChipEditorField::Path, 0, None);
        ed.set_dir_listing(vec![listing_entry("src", true)]);
        assert_eq!(ed.path_ghost().as_deref(), Some("rc/"));
        ed.input.move_left();
        assert_eq!(ed.path_ghost(), None);
    }

    #[test]
    fn dir_editor_glob_kind_never_offers_path_ghosts() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::glob(String::new(), None);
        assert_eq!(ed.dir_listing_path(&paths), None);
        assert!(!ed.sync_dir_listing(&paths));
        ed.set_dir_listing(vec![listing_entry("src", true)]);
        assert_eq!(ed.path_ghost(), None);
    }

    #[test]
    fn dir_editor_accept_suggestion_appends_slash_and_requests_next_segment() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        ed.sync_dir_listing(&paths);
        ed.set_dir_listing(vec![listing_entry("src", true)]);
        assert!(
            ed.accept_path_suggestion(&paths),
            "the absorbed segment grew the dir portion — refetch is due"
        );
        assert_eq!(ed.input.text, "src/");
        assert_eq!(ed.listing_dir_abs, "/proj/src");
        assert!(
            ed.listing.is_empty(),
            "stale listing cleared until the fetch lands"
        );
        // Without a ghost (empty listing) accepting is a no-op.
        assert!(!ed.accept_path_suggestion(&paths));
        assert_eq!(ed.input.text, "src/");
    }

    #[test]
    fn dir_editor_cycle_path_suggestion_clamps() {
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        ed.set_dir_listing(vec![
            listing_entry("a", true),
            listing_entry("b", true),
            listing_entry("c", true),
        ]);
        assert_eq!(ed.path_ghost().as_deref(), Some("a/"));
        ed.cycle_path_suggestion(true);
        ed.cycle_path_suggestion(true);
        assert_eq!(ed.path_ghost().as_deref(), Some("c/"));
        ed.cycle_path_suggestion(true);
        assert_eq!(
            ed.path_ghost().as_deref(),
            Some("c/"),
            "no wrap at the bottom"
        );
        ed.cycle_path_suggestion(false);
        assert_eq!(ed.path_ghost().as_deref(), Some("b/"));
    }

    #[test]
    fn dir_editor_pop_path_segment_is_fish_style() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir("src/app/picker".into(), ChipEditorField::Path, 0, None);
        ed.sync_dir_listing(&paths);
        assert_eq!(ed.listing_dir_abs, "/proj/src/app");
        assert!(
            !ed.pop_path_segment(&paths),
            "dropping the leaf keeps the dir portion"
        );
        assert_eq!(ed.input.text, "src/app/");
        assert!(
            ed.pop_path_segment(&paths),
            "dropping a dir segment moves it"
        );
        assert_eq!(ed.input.text, "src/");
        assert_eq!(ed.listing_dir_abs, "/proj/src");
        ed.pop_path_segment(&paths);
        assert_eq!(ed.input.text, "");
        assert_eq!(ed.listing_dir_abs, "/proj");
    }

    #[test]
    fn dir_editor_root_complete_only_on_full_label() {
        let labels: Vec<String> = ["beta", "beta-api"].iter().map(|s| s.to_string()).collect();
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 0, None);
        ed.root_filter = TextInput::new("be");
        assert!(!ed.root_complete(&labels));
        ed.root_filter = TextInput::new("beta");
        assert!(
            ed.root_complete(&labels),
            "exact label, even though beta-api also matches"
        );
        ed.root_filter = TextInput::new("zzz");
        assert!(!ed.root_complete(&labels), "no match is never complete");
    }

    #[test]
    fn dir_editor_path_valid_requires_every_segment_to_exist() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        ed.sync_dir_listing(&paths);
        // Empty path: always committable (whole-root scope / clear), even before the fetch.
        assert!(ed.path_valid());
        // Non-empty path with the listing still pending: validity unknown — not committable.
        ed.input = TextInput::new("src");
        assert_eq!(ed.listing_state, DirListingState::Pending);
        assert!(!ed.path_valid());
        ed.set_dir_listing(vec![
            listing_entry("src", true),
            listing_entry("src2", true),
            listing_entry("README.md", false),
        ]);
        // Exact directory match: valid. A bare prefix is valid too — it commits as its
        // highlighted completion (`committed_path`).
        assert!(ed.path_valid());
        ed.input = TextInput::new("sr");
        ed.path_edited(&paths);
        assert!(ed.path_valid());
        assert_eq!(ed.committed_path(), "src");
        // A file name never validates a dir scope (files are dropped at ingest).
        ed.input = TextInput::new("README.md");
        ed.path_edited(&paths);
        assert!(!ed.path_valid());
        // Trailing slash = empty leaf: valid once the new dir portion's listing loads.
        ed.input = TextInput::new("src/");
        assert!(ed.path_edited(&paths), "dir portion moved — refetch due");
        assert!(!ed.path_valid(), "pending again until the fetch lands");
        ed.set_dir_listing(Vec::new());
        assert!(ed.path_valid(), "empty leaf in an existing (empty) dir");
        // A failed fetch marks the whole dir portion nonexistent.
        ed.input = TextInput::new("zzz/app");
        ed.path_edited(&paths);
        ed.set_dir_listing_failed();
        assert!(!ed.path_valid());
        // Glob editors never gate on path validity.
        let glob = ChipEditor::glob("whatever".into(), None);
        assert!(glob.path_valid());
    }

    #[test]
    fn dir_editor_path_invalid_only_when_definitely_wrong() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        ed.sync_dir_listing(&paths);
        // Pending: validity unknown — not committable, but not flagged red either.
        ed.input = TextInput::new("s");
        assert_eq!(ed.listing_state, DirListingState::Pending);
        assert!(!ed.path_invalid());
        assert!(!ed.path_valid());
        ed.set_dir_listing(vec![listing_entry("src", true)]);
        // A prefix of an existing dir: mid-segment, ghost visible — not red, and committable
        // (Enter selects the completion).
        assert!(!ed.path_invalid());
        assert!(ed.path_valid());
        ed.input = TextInput::new("src");
        ed.path_edited(&paths);
        assert!(!ed.path_invalid());
        assert!(ed.path_valid());
        // Nothing even prefixes the leaf: red.
        ed.input = TextInput::new("zzz");
        ed.path_edited(&paths);
        assert!(ed.path_invalid());
        // A failed dir portion is red regardless of the leaf.
        ed.input = TextInput::new("zzz/foo");
        ed.path_edited(&paths);
        ed.set_dir_listing_failed();
        assert!(ed.path_invalid());
        assert!(!ed.path_valid());
    }

    #[test]
    fn dir_editor_enter_commits_the_completed_path() {
        let paths = vec!["/proj".to_string()];
        let mut ed = ChipEditor::dir("src/ap".into(), ChipEditorField::Path, 0, None);
        ed.sync_dir_listing(&paths);
        ed.set_dir_listing(vec![
            listing_entry("app", true),
            listing_entry("apple", true),
        ]);
        // The partial leaf commits as the highlighted completion — exactly what the ghost
        // shows.
        assert!(ed.path_valid());
        assert_eq!(ed.committed_path(), "src/app");
        // Cycling the ghost changes what commits — still WYSIWYG.
        ed.cycle_path_suggestion(true);
        assert_eq!(ed.path_ghost().as_deref(), Some("ple/"));
        assert_eq!(ed.committed_path(), "src/apple");
        // Empty leaf and unmatched leaf come back as typed.
        ed.input = TextInput::new("src/");
        ed.path_edited(&paths);
        assert_eq!(ed.committed_path(), "src/");
        ed.input = TextInput::new("src/zzz");
        ed.path_edited(&paths);
        assert!(!ed.path_valid());
        assert_eq!(ed.committed_path(), "src/zzz");
        // Glob editors pass the text through untouched.
        let glob = ChipEditor::glob("*.rs".into(), None);
        assert_eq!(glob.committed_path(), "*.rs");
    }

    #[test]
    fn dir_editor_root_invalid_only_when_nothing_matches() {
        let labels: Vec<String> = ["alpha", "beta"].iter().map(|s| s.to_string()).collect();
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 0, None);
        // Empty filter matches every root — a fresh Alt-d → Enter still commits.
        assert!(!ed.root_invalid(&labels));
        ed.root_filter = TextInput::new("be");
        assert!(!ed.root_invalid(&labels));
        ed.root_filter = TextInput::new("zzz");
        assert!(ed.root_invalid(&labels));
    }

    #[test]
    fn dir_editor_invalid_root_blocks_path_entry_and_suggestions() {
        let paths = vec!["/proj/alpha".to_string(), "/proj/beta".to_string()];
        let labels = crate::labels::root_labels(&paths);
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 0, None);
        ed.root_filter = TextInput::new("zzz");
        assert!(ed.root_invalid(&labels));
        // Tab / Alt-l refuse to advance — the path can't be entered under a root that won't
        // commit.
        assert!(!ed.commit_root_field(&labels, &paths));
        assert_eq!(ed.field, ChipEditorField::Root);
        // And there's no listing target: suggestions fetched beneath the fallback root would
        // read as silently defaulting to it.
        assert_eq!(ed.dir_listing_path(&paths), None);
        assert!(!ed.sync_dir_listing(&paths));
        // Fixing the filter restores both.
        ed.root_filter = TextInput::new("b");
        ed.root_selected = 0;
        assert!(ed.commit_root_field(&labels, &paths));
        assert_eq!(ed.field, ChipEditorField::Path);
        assert_eq!(ed.listing_dir_abs, "/proj/beta");
    }

    #[test]
    fn dir_editor_commit_root_field_adopts_ghost_and_moves_to_path() {
        let paths = vec!["/proj/alpha".to_string(), "/proj/beta".to_string()];
        let labels = crate::labels::root_labels(&paths);
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Root, 0, None);
        ed.root_filter = TextInput::new("b");
        ed.root_selected = 0;
        assert!(
            ed.commit_root_field(&labels, &paths),
            "root chosen — listing fetch due"
        );
        assert_eq!(ed.field, ChipEditorField::Path);
        assert_eq!(ed.root_filter.text, "beta");
        assert_eq!(ed.chosen_root(&labels), 1);
        assert_eq!(ed.listing_dir_abs, "/proj/beta");
        // Re-confirming the same root finds the listing already in sync.
        ed.field = ChipEditorField::Root;
        assert!(!ed.commit_root_field(&labels, &paths));
    }
}
