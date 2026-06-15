//! Picker filter chips (docs/picker-filters.md): the ordered chip list that *is* the client's
//! filter state, plus the valued-chip editor (glob / dir) revealed below the picker input.
//!
//! Ported from the terminal client's `picker.rs` chip machinery — same semantics, free of
//! ratatui/crossterm types so `picker.rs` (rendering) and `app.rs` (keys/RPC) stay thin. The
//! wire `PickerFilters` is derived per send ([`wire_filters`]) and converted back on
//! open/resume ([`adopt_filters`]); insertion order is session-ephemeral.

use aether_protocol::directory::DirectoryEntry;
use aether_protocol::picker::{CaseMode, PickerFilters, PickerKind, ScopedPath};

/// Minimal editable text field (byte cursor), for the chip editor's segments. The picker query
/// keeps its own inline pair; this exists so the editor's two fields don't triplicate the
/// cursor arithmetic.
#[derive(Debug, Default, Clone)]
pub struct Input {
    /// The field value. Text editing (caret, insert, delete) is owned by each shell's input —
    /// native `text_input`/`<input>` in the rich clients, a shell-local editor in the TUI — which
    /// syncs the whole value via [`crate::update`]'s `chip_editor_set_input` /
    /// `chip_editor_set_root_filter`. The core keeps only the value.
    pub text: String,
}

impl Input {
    pub fn new(text: String) -> Self {
        Input { text }
    }

    pub fn set(&mut self, text: String) {
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Append `s` (used by ghost-suggestion accept, which completes the partial leaf at the end).
    pub fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }
}

/// Which filter a chip stands for — the handle used to edit/remove it. `Dir` and `Glob` carry
/// their index into the chip list (the repeatable chips; the rendered row is the stored list,
/// so row index = storage index). There's no root chip: scoping to a whole root is a `Dir`
/// chip with an empty relative path.
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

/// One chip, by value — the element of the client's ordered filter state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipValue {
    Dir(ScopedPath),
    Glob(String),
    /// `Sensitive` or `Insensitive` — `Smart` (the default) is "no chip".
    Case(CaseMode),
    Word,
    Lit,
    /// Gitignored-file visibility. `hide` records the per-kind direction at creation time
    /// (the Explorer hides, Grep includes), so the wire conversion needs no kind context.
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

/// One rendered filter chip — derived from the chip list on demand (never stored).
#[derive(Debug, Clone)]
pub struct Chip {
    pub id: ChipId,
    pub label: String,
}

/// Whether a filter chip applies to this picker kind (chords are clean no-ops elsewhere):
/// Grep takes everything; Files the scope chips + changed-only; the Explorer the visibility
/// chips + changed-only.
pub fn filter_applies(kind: PickerKind, id: ChipId) -> bool {
    match kind {
        PickerKind::Grep => true,
        PickerKind::Files => matches!(id, ChipId::Dir(_) | ChipId::Glob(_) | ChipId::Changed),
        PickerKind::Explorer => matches!(id, ChipId::Ignored | ChipId::Hidden | ChipId::Changed),
        _ => false,
    }
}

/// Render the chip row: the stored list, verbatim — insertion order *is* the storage order, so
/// row index = storage index. Labels are compact: the dir chip is just the path with a
/// trailing `/` (multi-root labels lead with the disambiguated root label), and the flags are
/// two-or-three-char abbreviations. The ignored/hidden chips render `+` (include — Grep) or
/// `-` (hide — Explorer) per the direction stored in the value.
pub fn derive_chips(values: &[ChipValue], project_paths: &[String]) -> Vec<Chip> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let (id, label) = match v {
                ChipValue::Dir(d) => {
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

/// Fold the chip list into the wire format — the normalized, unordered `PickerFilters` sent
/// with every `picker/query`/`picker/view`.
pub fn wire_filters(values: &[ChipValue]) -> PickerFilters {
    let mut f = PickerFilters::default();
    for v in values {
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

/// Convert a wire filter set into a chip list (open/resume). The wire carries no order, so
/// restored chips come back in canonical order (dirs, globs, flags).
pub fn adopt_filters(f: &PickerFilters) -> Vec<ChipValue> {
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
    chips
}

/// Toggle/cycle the filter a flag chip stands for: booleans flip (appearing appends,
/// disappearing drops out); `case` cycles smart → sensitive → insensitive → smart *in place*
/// while the chip stays visible. The ignored/hidden chips record the per-kind direction
/// (`explorer` hides; everything else includes) in the value at creation time. Returns `false`
/// for the valued chips (dir, glob — those go through their editors).
pub fn apply_chip_toggle(values: &mut Vec<ChipValue>, id: ChipId, explorer: bool) -> bool {
    let value = match id {
        ChipId::Case => {
            let pos = values.iter().position(|v| matches!(v, ChipValue::Case(_)));
            match pos {
                None => values.push(ChipValue::Case(CaseMode::Sensitive)),
                Some(i) => match values[i] {
                    ChipValue::Case(CaseMode::Sensitive) => {
                        values[i] = ChipValue::Case(CaseMode::Insensitive);
                    }
                    _ => {
                        values.remove(i);
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
    match values.iter().position(|v| v.same_kind(&value)) {
        Some(i) => {
            values.remove(i);
        }
        None => values.push(value),
    }
    true
}

/// Remove the chip — it disappears from the row and from the next [`wire_filters`] fold.
pub fn remove_chip(values: &mut Vec<ChipValue>, id: ChipId) {
    match id {
        ChipId::Dir(i) | ChipId::Glob(i) => {
            if i < values.len() {
                values.remove(i);
            }
        }
        ChipId::Case => values.retain(|v| !matches!(v, ChipValue::Case(_))),
        ChipId::Word => values.retain(|v| *v != ChipValue::Word),
        ChipId::Lit => values.retain(|v| *v != ChipValue::Lit),
        ChipId::Ignored => values.retain(|v| !matches!(v, ChipValue::Ignored { .. })),
        ChipId::Hidden => values.retain(|v| !matches!(v, ChipValue::Hidden { .. })),
        ChipId::Changed => values.retain(|v| *v != ChipValue::Changed),
    }
}

/// Apply a glob editor commit: `None` clears the glob being edited (or cancels when it wasn't
/// editing one); duplicates collapse — committing an existing glob is a no-op, editing into
/// one drops the edited entry; an in-place edit keeps its position in the row. `edit` indexes
/// the chip list. Returns whether the filters changed.
pub fn commit_glob_edit(
    values: &mut Vec<ChipValue>,
    normalized: Option<String>,
    edit: Option<usize>,
) -> bool {
    let edit = edit.filter(|&i| matches!(values.get(i), Some(ChipValue::Glob(_))));
    let Some(g) = normalized else {
        return match edit {
            Some(i) => {
                values.remove(i);
                true
            }
            None => false, // empty/useless new glob — treat as cancel
        };
    };
    commit_valued(values, ChipValue::Glob(g), edit)
}

/// Apply a dir editor commit — same shape as [`commit_glob_edit`]: `None` clears the scope
/// being edited (or cancels when adding), duplicates collapse, in-place edits keep their
/// position.
pub fn commit_dir_edit(
    values: &mut Vec<ChipValue>,
    value: Option<ScopedPath>,
    edit: Option<usize>,
) -> bool {
    let edit = edit.filter(|&i| matches!(values.get(i), Some(ChipValue::Dir(_))));
    let Some(d) = value else {
        return match edit {
            Some(i) => {
                values.remove(i);
                true
            }
            None => false, // empty new scope in a single-root project — cancel
        };
    };
    commit_valued(values, ChipValue::Dir(d), edit)
}

fn commit_valued(values: &mut Vec<ChipValue>, value: ChipValue, edit: Option<usize>) -> bool {
    match edit {
        Some(i) => {
            if values
                .iter()
                .enumerate()
                .any(|(j, v)| j != i && *v == value)
            {
                values.remove(i);
            } else {
                values[i] = value;
            }
            true
        }
        None => {
            if values.contains(&value) {
                false // already present — the chip says it; nothing to change
            } else {
                values.push(value);
                true
            }
        }
    }
}

/// Normalize a committed glob. `None` means "don't keep a chip": empty input, or a degenerate
/// match-everything glob (`*`, `**`, also negated — `!*` would exclude *everything*). A glob
/// starting with `.` (or `!.`) that contains no other glob syntax is an extension shorthand:
/// `.rs` → `*.rs`.
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

/// Indices of the root labels matching `filter` as a smartcase prefix (everything, on an empty
/// filter) — the root field's typeahead candidates, in root order.
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

/// Indices into `listing` that smartcase-prefix-match `filter`. Empty filter matches all.
pub fn matching_indices(listing: &[DirectoryEntry], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..listing.len()).collect();
    }
    let sensitive = filter.chars().any(|c| c.is_uppercase());
    let needle: String = if sensitive {
        filter.to_string()
    } else {
        filter.chars().flat_map(char::to_lowercase).collect()
    };
    listing
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if sensitive {
                e.name.starts_with(needle.as_str())
            } else {
                e.name
                    .chars()
                    .flat_map(char::to_lowercase)
                    .collect::<String>()
                    .starts_with(needle.as_str())
            }
        })
        .map(|(i, _)| i)
        .collect()
}

/// Split a path input at the last `/`: `(dir portion incl. the slash, partial leaf)`.
fn split_input(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(i) => input.split_at(i + 1),
        None => ("", input),
    }
}

pub fn dir_of_input(input: &str) -> &str {
    split_input(input).0
}

pub fn partial_of_input(input: &str) -> &str {
    split_input(input).1
}

/// Fish-style segment delete: drop the rightmost segment, keeping the parent's trailing `/`
/// when one exists. `"src/foo/file"` → `"src/foo/"`, `"src/foo/"` → `"src/"`, `"src"` → `""`.
pub fn pop_segment(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    if let Some(stripped) = input.strip_suffix('/') {
        match stripped.rfind('/') {
            Some(i) => stripped[..=i].to_string(),
            None => String::new(),
        }
    } else {
        match input.rfind('/') {
            Some(i) => input[..=i].to_string(),
            None => String::new(),
        }
    }
}

/// Resolve `dir_part` (root-relative, possibly with trailing `/`) under the chosen root.
pub fn join_root_relative(project_paths: &[String], path_index: u32, dir_part: &str) -> String {
    let Some(root) = project_paths.get(path_index as usize) else {
        return String::new();
    };
    let trimmed = dir_part.trim_end_matches('/');
    if trimmed.is_empty() {
        root.clone()
    } else {
        std::path::Path::new(root)
            .join(trimmed)
            .display()
            .to_string()
    }
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
    /// Same shape for the (equally repeatable) dir scopes.
    Dir { edit: Option<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipEditorField {
    Root,
    Path,
}

/// The editor line for a valued chip, revealed below the picker's input row. The dir editor
/// reads as a single `dir:` field: in multi-root projects a root segment (an inline typeahead)
/// leads, separated by `:` from the root-relative path; single-root projects show only the
/// path. The path segment carries directory-only ghost suggestions, cached in `listing`.
#[derive(Debug)]
pub struct ChipEditor {
    pub kind: ChipEditorKind,
    /// Which field has focus. Always `Path` for glob and single-root dir editors.
    pub field: ChipEditorField,
    /// The glob text / the root-relative directory path.
    pub input: Input,
    /// Dir, multi-root: the prefix filter typed into the root field.
    pub root_filter: Input,
    /// Dir, multi-root: highlight within [`root_candidates`]' matches for the current filter.
    pub root_selected: usize,
    /// Dir: the root the editor opened with — the fallback when the filter matches nothing.
    pub root_index: u32,
    /// Dir: cached `directory/list` entries (subdirectories only — files never complete a dir
    /// scope) for the dir portion of `input`.
    pub listing: Vec<DirectoryEntry>,
    /// Dir: the absolute path `listing` was last synced against (the staleness key).
    pub listing_dir_abs: String,
    /// Dir: where `listing` stands relative to `listing_dir_abs`.
    pub listing_state: DirListingState,
    /// Dir: position within the filtered match set producing the current path ghost.
    pub suggestion_idx: usize,
}

impl ChipEditor {
    /// A glob editor: one field, no suggestion machinery.
    pub fn glob(prefill: String, edit: Option<usize>) -> Self {
        ChipEditor {
            kind: ChipEditorKind::Glob { edit },
            field: ChipEditorField::Path,
            input: Input::new(prefill),
            root_filter: Input::default(),
            root_selected: 0,
            root_index: 0,
            listing: Vec::new(),
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Pending,
            suggestion_idx: 0,
        }
    }

    /// A dir editor. `listing_dir_abs` starts empty, so the caller's first
    /// [`ChipEditor::sync_dir_listing`] always reports a refetch is due.
    pub fn dir(path: String, field: ChipEditorField, root_index: u32, edit: Option<usize>) -> Self {
        ChipEditor {
            kind: ChipEditorKind::Dir { edit },
            field,
            input: Input::new(path),
            root_filter: Input::default(),
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

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, ChipEditorKind::Dir { .. })
    }

    /// The root the editor would commit: the highlighted candidate for the current filter,
    /// falling back to the root it opened with when the filter matches nothing.
    pub fn chosen_root(&self, labels: &[String]) -> u32 {
        let candidates = root_candidates(labels, &self.root_filter.text);
        match candidates.get(self.root_selected.min(candidates.len().saturating_sub(1))) {
            Some(&i) => i as u32,
            None => self.root_index,
        }
    }

    /// The root field's ghost completion: the current match's root index and the part of its
    /// label beyond the typed prefix. `None` when nothing matches the typed prefix.
    pub fn root_ghost(&self, labels: &[String]) -> Option<(usize, String)> {
        let candidates = root_candidates(labels, &self.root_filter.text);
        let &idx = candidates.get(self.root_selected.min(candidates.len().saturating_sub(1)))?;
        let typed_chars = self.root_filter.text.chars().count();
        let suffix: String = labels[idx].chars().skip(typed_chars).collect();
        Some((idx, suffix))
    }

    /// True when the root field holds a complete root label (the current match's ghost suffix
    /// is empty) — what lets a typed `:` act as the root/path separator.
    pub fn root_complete(&self, labels: &[String]) -> bool {
        self.root_ghost(labels)
            .is_some_and(|(_, suffix)| suffix.is_empty())
    }

    /// Confirm the root field (adopting the ghost completion) and move focus into the path.
    /// An *invalid* root refuses: focus stays on the (red) root field. Returns `true` when the
    /// listing went stale and the caller should refetch.
    pub fn commit_root_field(&mut self, labels: &[String], project_paths: &[String]) -> bool {
        let Some((idx, _)) = self.root_ghost(labels) else {
            return false; // no candidate ⇔ root_invalid — stay put
        };
        self.root_filter = Input::new(labels[idx].clone());
        // The full label may still prefix-match several roots ("beta" vs "beta-api") — keep
        // the highlight on the adopted one.
        self.root_selected = root_candidates(labels, &self.root_filter.text)
            .iter()
            .position(|&c| c == idx)
            .unwrap_or(0);
        self.field = ChipEditorField::Path;
        self.sync_dir_listing(project_paths)
    }

    /// The absolute directory the path field's suggestions should list: the dir portion of the
    /// typed path, resolved under the chosen root. `None` for glob editors — and under an
    /// *invalid* root (suggestions beneath the fallback root would read as silently defaulting
    /// to it).
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
        Some(join_root_relative(
            project_paths,
            root,
            dir_of_input(&self.input.text),
        ))
    }

    /// Store a `directory/list` response, keeping only subdirectories — a file never completes
    /// a directory scope.
    pub fn set_dir_listing(&mut self, entries: Vec<DirectoryEntry>) {
        self.listing = entries.into_iter().filter(|e| e.is_dir).collect();
        self.listing_state = DirListingState::Loaded;
        self.suggestion_idx = 0;
    }

    /// Record that the `directory/list` fetch failed: the path renders invalid and the commit
    /// gate refuses it until the next path change re-syncs.
    pub fn set_dir_listing_failed(&mut self) {
        self.listing.clear();
        self.listing_state = DirListingState::Failed;
        self.suggestion_idx = 0;
    }

    /// Reconcile the listing key with the current (root, dir-portion) pair. Returns `true`
    /// when they diverged — the listing was cleared and the caller should fire a fresh
    /// `directory/list` for [`ChipEditor::dir_listing_path`].
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
    /// no root label. (An empty filter matches every root.)
    pub fn root_invalid(&self, labels: &[String]) -> bool {
        root_candidates(labels, &self.root_filter.text).is_empty()
    }

    /// True when the path field holds a committable value: empty (whole-root scope / clear),
    /// or a path whose dir portion is vouched for by a `Loaded` listing and whose leaf is
    /// either empty or prefixes at least one listed subdirectory. A `Pending` listing can't
    /// vouch, so a commit racing the fetch waits. Always true for glob editors.
    pub fn path_valid(&self) -> bool {
        if !self.is_dir() || self.input.text.is_empty() {
            return true;
        }
        if self.listing_state != DirListingState::Loaded {
            return false;
        }
        let leaf = partial_of_input(&self.input.text);
        leaf.is_empty() || !matching_indices(&self.listing, leaf).is_empty()
    }

    /// The path a commit should adopt: the typed text, with a partially typed leaf completed
    /// to the highlighted suggestion — Enter on a prefix selects the completion, and the ghost
    /// shows exactly what will commit.
    pub fn committed_path(&self) -> String {
        if !self.is_dir() || self.listing_state != DirListingState::Loaded {
            return self.input.text.clone();
        }
        let dir = dir_of_input(&self.input.text);
        let leaf = partial_of_input(&self.input.text);
        if leaf.is_empty() {
            return self.input.text.clone();
        }
        let matches = matching_indices(&self.listing, leaf);
        match matches
            .get(self.suggestion_idx)
            .and_then(|&i| self.listing.get(i))
        {
            Some(entry) => format!("{dir}{}", entry.name),
            None => self.input.text.clone(),
        }
    }

    /// True when the path is *definitely* wrong — the red-worthy condition: the dir portion
    /// failed to list, or the loaded listing holds no directory the leaf even prefixes. A
    /// `Pending` listing is neither committable nor flagged (unknown ≠ invalid).
    pub fn path_invalid(&self) -> bool {
        if !self.is_dir() || self.input.text.is_empty() {
            return false;
        }
        match self.listing_state {
            DirListingState::Pending => false,
            DirListingState::Failed => true,
            DirListingState::Loaded => {
                let leaf = partial_of_input(&self.input.text);
                !leaf.is_empty() && matching_indices(&self.listing, leaf).is_empty()
            }
        }
    }

    /// The path field's ghost: the rest of the current directory match beyond the partial
    /// leaf, plus the `/` that opens the next segment. Computed from the value alone; each shell
    /// suppresses it when its own caret isn't at the end of the input (the core no longer owns the
    /// caret — see [`Input`]).
    pub fn path_ghost(&self) -> Option<String> {
        if !self.is_dir() {
            return None;
        }
        let partial = partial_of_input(&self.input.text);
        let matches = matching_indices(&self.listing, partial);
        let pick = *matches.get(self.suggestion_idx)?;
        let entry = self.listing.get(pick)?;
        let mut suffix: String = entry.name.chars().skip(partial.chars().count()).collect();
        suffix.push('/');
        Some(suffix)
    }

    /// Step the path ghost through the filtered matches (Alt-j/k), clamped at both ends.
    pub fn cycle_path_suggestion(&mut self, down: bool) {
        let partial = partial_of_input(&self.input.text);
        let n = matching_indices(&self.listing, partial).len();
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
    /// in `/`, so the dir portion grew — returns `true` so the caller refetches.
    pub fn accept_path_suggestion(&mut self, project_paths: &[String]) -> bool {
        let Some(suffix) = self.path_ghost() else {
            return false;
        };
        self.input.push_str(&suffix);
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Alt-Backspace in a non-empty path field: drop the rightmost segment, fish-style.
    /// Returns `true` when the dir portion shrank and a refetch is due.
    pub fn pop_path_segment(&mut self, project_paths: &[String]) -> bool {
        let popped = pop_segment(&self.input.text);
        self.input.set(popped);
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Bookkeeping after a free-form edit to the path field: reset the suggestion highlight
    /// and report whether the dir portion moved.
    pub fn path_edited(&mut self, project_paths: &[String]) -> bool {
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_normalization() {
        assert_eq!(normalize_glob("  "), None);
        assert_eq!(normalize_glob("*"), None);
        assert_eq!(normalize_glob("!**"), None);
        assert_eq!(normalize_glob(".rs"), Some("*.rs".into()));
        assert_eq!(normalize_glob("!.rs"), Some("!*.rs".into()));
        assert_eq!(normalize_glob("src/**"), Some("src/**".into()));
        assert_eq!(normalize_glob(".config/*"), Some(".config/*".into()));
    }

    #[test]
    fn wire_roundtrip_restores_canonical_chips() {
        let chips = vec![
            ChipValue::Word,
            ChipValue::Glob("*.rs".into()),
            ChipValue::Dir(ScopedPath {
                path_index: 1,
                relative_path: "src".into(),
            }),
            ChipValue::Ignored { hide: false },
            ChipValue::Case(CaseMode::Insensitive),
        ];
        let wire = wire_filters(&chips);
        assert!(wire.whole_word && wire.include_ignored);
        assert_eq!(wire.globs, vec!["*.rs".to_string()]);
        assert_eq!(wire.case, CaseMode::Insensitive);
        let restored = adopt_filters(&wire);
        // Canonical order: dirs, globs, flags.
        assert!(matches!(restored[0], ChipValue::Dir(_)));
        assert!(matches!(restored[1], ChipValue::Glob(_)));
        assert_eq!(wire_filters(&restored), wire);
    }

    #[test]
    fn case_cycles_in_place() {
        let mut chips = vec![ChipValue::Word];
        assert!(apply_chip_toggle(&mut chips, ChipId::Case, false));
        assert_eq!(chips[1], ChipValue::Case(CaseMode::Sensitive));
        assert!(apply_chip_toggle(&mut chips, ChipId::Case, false));
        assert_eq!(chips[1], ChipValue::Case(CaseMode::Insensitive));
        assert!(apply_chip_toggle(&mut chips, ChipId::Case, false));
        assert_eq!(chips.len(), 1); // back to smart — chip gone
                                    // Boolean toggles flip; the explorer direction is recorded in the value.
        assert!(apply_chip_toggle(&mut chips, ChipId::Ignored, true));
        assert_eq!(chips[1], ChipValue::Ignored { hide: true });
        assert!(apply_chip_toggle(&mut chips, ChipId::Ignored, true));
        assert_eq!(chips.len(), 1);
    }

    #[test]
    fn valued_commits_dedupe_and_edit_in_place() {
        let mut chips = vec![ChipValue::Glob("*.rs".into()), ChipValue::Word];
        // Committing a duplicate as a new chip is a no-op.
        assert!(!commit_glob_edit(&mut chips, Some("*.rs".into()), None));
        // A fresh glob appends.
        assert!(commit_glob_edit(&mut chips, Some("!*.md".into()), None));
        assert_eq!(chips.len(), 3);
        // Editing entry 0 in place keeps its position.
        assert!(commit_glob_edit(&mut chips, Some("*.toml".into()), Some(0)));
        assert_eq!(chips[0], ChipValue::Glob("*.toml".into()));
        // Editing into an existing value drops the edited entry (leaving [Word, !*.md]).
        assert!(commit_glob_edit(&mut chips, Some("!*.md".into()), Some(0)));
        assert_eq!(chips.len(), 2);
        // An empty commit clears the chip being edited; a non-glob edit target is a cancel.
        assert!(!commit_glob_edit(&mut chips, None, Some(0))); // chips[0] is Word
        assert!(commit_glob_edit(&mut chips, None, Some(1)));
        assert_eq!(chips, vec![ChipValue::Word]);
    }

    #[test]
    fn root_candidates_are_smartcase_prefixes() {
        let labels = vec!["api (work)".to_string(), "Api (personal)".to_string()];
        assert_eq!(root_candidates(&labels, ""), vec![0, 1]);
        assert_eq!(root_candidates(&labels, "ap"), vec![0, 1]); // insensitive
        assert_eq!(root_candidates(&labels, "Ap"), vec![1]); // upper → sensitive
        assert!(root_candidates(&labels, "x").is_empty());
    }

    #[test]
    fn pop_segment_examples() {
        assert_eq!(pop_segment("src/foo/file.txt"), "src/foo/");
        assert_eq!(pop_segment("src/foo/"), "src/");
        assert_eq!(pop_segment("src/"), "");
        assert_eq!(pop_segment("src/foo"), "src/");
        assert_eq!(pop_segment("src"), "");
        assert_eq!(pop_segment(""), "");
    }

    #[test]
    fn dir_editor_path_flow() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = ChipEditor::dir(String::new(), ChipEditorField::Path, 0, None);
        assert!(ed.sync_dir_listing(&roots));
        assert_eq!(ed.listing_dir_abs, "/tmp/root");
        assert!(ed.path_valid()); // an empty path is always committable (whole-root scope)
        ed.set_dir_listing(vec![
            DirectoryEntry {
                name: "src".into(),
                is_dir: true,
            },
            DirectoryEntry {
                name: "docs".into(),
                is_dir: true,
            },
            DirectoryEntry {
                name: "README.md".into(),
                is_dir: false,
            },
        ]);
        assert_eq!(ed.listing.len(), 2); // files dropped
        ed.input.push_str("s");
        assert!(!ed.path_edited(&roots)); // dir portion unchanged — no refetch due
        assert_eq!(ed.path_ghost(), Some("rc/".into()));
        assert!(ed.path_valid());
        assert_eq!(ed.committed_path(), "src");
        // Accepting the ghost grows the dir portion → refetch due.
        assert!(ed.accept_path_suggestion(&roots));
        assert_eq!(ed.input.text, "src/");
        assert_eq!(ed.listing_dir_abs, "/tmp/root/src");
        // A leaf that prefixes nothing is invalid once the listing loads.
        ed.set_dir_listing(vec![]);
        ed.input.push_str("zzz");
        assert!(ed.path_invalid());
        assert!(!ed.path_valid());
    }
}
