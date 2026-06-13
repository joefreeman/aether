//! Client-side picker state. The server owns the candidate cache, query, and ranked list; the
//! client owns the highlighted row plus a small persisted slot (`last_selected`) used to restore
//! the highlight on reopen via `view { center_on }`.

use crate::scroll::ScrollState;
use aether_protocol::directory::DirectoryEntry;
use aether_protocol::lsp::{LspProgress, LspStatus};
use aether_protocol::picker::{CaseMode, PickerFilters, PickerItem, PickerKind, ScopedPath};
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub enum DefaultSkip {
    Buffer(BufferId),
    Project(String),
}

#[derive(Debug, Default)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    /// The throbber glyph while a search is in progress, or `None` when settled (synced from the
    /// core's `spinner_glyph`).
    pub spinner: Option<&'static str>,
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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

impl ChipValue {}

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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub enum DirListingState {
    /// A fetch for `listing_dir_abs` is owed or in flight — validity is unknown, commits wait.
    Pending,
    /// `listing` reflects `listing_dir_abs`; the directory exists.
    Loaded,
    /// The fetch failed — the dir portion doesn't exist (or sits outside the project boundary).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn root_complete(&self, labels: &[String]) -> bool {
        self.root_ghost(labels)
            .is_some_and(|(_, suffix)| suffix.is_empty())
    }

    /// Store a `directory/list` response, keeping only subdirectories — a file never completes
    /// a directory scope.
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn set_dir_listing(&mut self, entries: Vec<DirectoryEntry>) {
        self.listing = entries.into_iter().filter(|e| e.is_dir).collect();
        self.listing_state = DirListingState::Loaded;
        self.suggestion_idx = 0;
    }

    /// Record that the `directory/list` fetch failed: the dir portion names a directory that
    /// doesn't exist (or one outside the project boundary). The path renders invalid and the
    /// commit gate refuses it until the next path change re-syncs.
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn set_dir_listing_failed(&mut self) {
        self.listing.clear();
        self.listing_state = DirListingState::Failed;
        self.suggestion_idx = 0;
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn dir_value(&self, i: usize) -> Option<&ScopedPath> {
        match self.chips.get(i) {
            Some(ChipValue::Dir(d)) => Some(d),
            _ => None,
        }
    }

    /// The glob behind chip `i`, when chip `i` is a glob — the editor's pre-fill.
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn glob_value(&self, i: usize) -> Option<&str> {
        match self.chips.get(i) {
            Some(ChipValue::Glob(g)) => Some(g.as_str()),
            _ => None,
        }
    }

    /// Apply a glob editor commit: `None` clears the glob being edited (or cancels when it
    /// wasn't editing one); duplicates collapse — committing an existing glob is a no-op (the
    /// chip already says it), editing into one drops the edited entry; an in-place edit keeps
    /// its position in the row. `edit` indexes the chip list. Returns whether the filters
    /// changed (the caller follows up with the filter-change RPC).
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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

    /// True if the highlighted row is the synthetic "create" row (the Projects picker's
    /// "create new project" affordance). The selector uses this to route to `project/create`
    /// instead of the normal `picker/select` flow.
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
    pub fn highlighted_is_synthetic_create(&self) -> bool {
        Some(self.selected) == self.synthetic_create_idx
    }

    /// The item currently under the highlight, if any.
    #[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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

    fn empty_state(kind: PickerKind, query: &str) -> PickerState {
        PickerState {
            open: true,
            kind: Some(kind),
            query: TextInput::new(query),
            ..PickerState::default()
        }
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
    fn dir_editor_path_ghost_hidden_when_cursor_not_at_end() {
        let mut ed = ChipEditor::dir("s".into(), ChipEditorField::Path, 0, None);
        ed.set_dir_listing(vec![listing_entry("src", true)]);
        assert_eq!(ed.path_ghost().as_deref(), Some("rc/"));
        ed.input.move_left();
        assert_eq!(ed.path_ghost(), None);
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
}
