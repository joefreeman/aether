//! The save-as prompt's editor (`Alt-s`): a project-relative path field with the same
//! directory-completion UX as the picker's dir-scope chip editor (docs/picker-filters.md § 1.6),
//! so saving somewhere reuses the muscle memory of scoping a search there.
//!
//! It mirrors [`crate::chips::ChipEditor`]'s dir half — a multi-root projects' leading root field
//! (inline smartcase typeahead, `:` separator) ahead of a `directory/list`-backed path field with
//! ghost suggestions, `Tab`/`Alt-l` accept, `Alt-j`/`k` cycle, and fish-style `Alt-Backspace`
//! segment pop — with two deliberate departures, because the path's final segment is a *new
//! filename* rather than an existing subdirectory:
//!
//! - The cached listing keeps **files as well as directories** ([`set_dir_listing`]): completing
//!   onto an existing file is how you overwrite it. The ghost appends `/` only behind a directory.
//! - Committing saves the **literal typed path** ([`save_target`]); a partially typed leaf is *not*
//!   silently snapped to the highlighted suggestion (that is what `Tab` is for), and a non-matching
//!   leaf never blocks the save — you're naming a file that needn't exist yet. A missing *parent*
//!   directory still renders red ([`path_invalid`]) as an advisory.
//!
//! Text editing (caret, insert, delete) is owned by each shell's input, which syncs the whole
//! value via [`crate::update`]'s `save_as_set_input` / `save_as_set_root_filter`; the core keeps
//! only the value and handles the command keys (see `on_save_as_key`).

use crate::chips::{
    dir_of_input, join_root_relative, matching_indices, partial_of_input, pop_segment,
    root_candidates, ChipEditorField, DirListingState, Input,
};
use crate::labels::root_labels;
use aether_protocol::directory::DirectoryEntry;

/// The save-as path editor. In single-root projects only the path field exists (`field` is always
/// `Path`); multi-root projects add the leading root field.
#[derive(Debug)]
pub struct SaveAsEditor {
    /// Which segment has focus. Always `Path` in single-root projects.
    pub field: ChipEditorField,
    /// The root-relative path being typed (directory portion + filename leaf).
    pub input: Input,
    /// Multi-root: the prefix filter typed into the root field.
    pub root_filter: Input,
    /// Multi-root: highlight within [`root_candidates`]' matches for the current filter.
    pub root_selected: usize,
    /// The root the editor opened with — the fallback when the filter matches nothing.
    pub root_index: u32,
    /// Cached `directory/list` entries (files *and* directories) for the dir portion of `input`.
    pub listing: Vec<DirectoryEntry>,
    /// The absolute path `listing` was last synced against (the staleness key).
    pub listing_dir_abs: String,
    /// Where `listing` stands relative to `listing_dir_abs`.
    pub listing_state: DirListingState,
    /// Position within the filtered match set producing the current path ghost.
    pub suggestion_idx: usize,
}

impl SaveAsEditor {
    /// Open the editor pre-filled with `path` under root `root_index`. `field` is the initially
    /// focused segment (callers focus the root field for a brand-new buffer in a multi-root
    /// project, the path field otherwise). `listing_dir_abs` starts empty so the caller's first
    /// [`SaveAsEditor::sync_dir_listing`] always reports a refetch is due.
    pub fn new(path: String, field: ChipEditorField, root_index: u32) -> Self {
        SaveAsEditor {
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

    // ---- root field (multi-root only) ----------------------------------------------------------

    /// The root the editor would save into: the highlighted candidate for the current filter,
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

    /// True when the root field holds a complete root label (the ghost suffix is empty) — what
    /// lets a typed `:` act as the root/path separator.
    pub fn root_complete(&self, labels: &[String]) -> bool {
        self.root_ghost(labels)
            .is_some_and(|(_, suffix)| suffix.is_empty())
    }

    /// True when the root field would refuse a commit: a non-empty filter that prefix-matches no
    /// root label. (An empty filter matches every root.)
    pub fn root_invalid(&self, labels: &[String]) -> bool {
        root_candidates(labels, &self.root_filter.text).is_empty()
    }

    /// Confirm the root field (adopting the ghost completion) and move focus into the path. An
    /// *invalid* root refuses: focus stays on the (red) root field. Returns `true` when the
    /// listing went stale and the caller should refetch.
    pub fn commit_root_field(&mut self, labels: &[String], project_paths: &[String]) -> bool {
        let Some((idx, _)) = self.root_ghost(labels) else {
            return false; // no candidate ⇔ root_invalid — stay put
        };
        self.root_filter = Input::new(labels[idx].clone());
        // The full label may still prefix-match several roots ("beta" vs "beta-api") — keep the
        // highlight on the adopted one.
        self.root_selected = root_candidates(labels, &self.root_filter.text)
            .iter()
            .position(|&c| c == idx)
            .unwrap_or(0);
        self.field = ChipEditorField::Path;
        self.sync_dir_listing(project_paths)
    }

    // ---- directory listing ---------------------------------------------------------------------

    /// The absolute directory the path field's suggestions should list: the dir portion of the
    /// typed path, resolved under the chosen root. `None` under an *invalid* root (suggestions
    /// beneath the fallback root would read as silently defaulting to it).
    pub fn dir_listing_path(&self, project_paths: &[String]) -> Option<String> {
        let root = if project_paths.len() > 1 {
            let labels = root_labels(project_paths);
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

    /// Store a `directory/list` response, keeping **every** entry — unlike the dir-scope chip
    /// editor, a file completes a save path (you're overwriting it).
    pub fn set_dir_listing(&mut self, entries: Vec<DirectoryEntry>) {
        self.listing = entries;
        self.listing_state = DirListingState::Loaded;
        self.suggestion_idx = 0;
    }

    /// Record that the `directory/list` fetch failed: the path renders invalid (the parent dir
    /// doesn't exist) until the next path change re-syncs.
    pub fn set_dir_listing_failed(&mut self) {
        self.listing.clear();
        self.listing_state = DirListingState::Failed;
        self.suggestion_idx = 0;
    }

    /// Reconcile the listing key with the current (root, dir-portion) pair. Returns `true` when
    /// they diverged — the listing was cleared and the caller should fire a fresh `directory/list`
    /// for [`SaveAsEditor::dir_listing_path`].
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

    // ---- path field --------------------------------------------------------------------------

    /// The path field's ghost: the rest of the current match beyond the partial leaf, plus a
    /// trailing `/` when the match is a directory (a file completes outright). Computed from the
    /// value alone; each shell suppresses it when its own caret isn't at the end of the input.
    pub fn path_ghost(&self) -> Option<String> {
        let partial = partial_of_input(&self.input.text);
        let matches = matching_indices(&self.listing, partial);
        let pick = *matches.get(self.suggestion_idx)?;
        let entry = self.listing.get(pick)?;
        let mut suffix: String = entry.name.chars().skip(partial.chars().count()).collect();
        if entry.is_dir {
            suffix.push('/');
        }
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

    /// Tab / Alt-l in the path field: absorb the ghost into the input. Returns `true` when the
    /// dir portion grew (a directory was accepted) and the caller should refetch — accepting a
    /// *file* extends only the leaf, so no refetch.
    pub fn accept_path_suggestion(&mut self, project_paths: &[String]) -> bool {
        let Some(suffix) = self.path_ghost() else {
            return false;
        };
        self.input.push_str(&suffix);
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Alt-Backspace in a non-empty path field: drop the rightmost segment, fish-style. Returns
    /// `true` when the dir portion shrank and a refetch is due.
    pub fn pop_path_segment(&mut self, project_paths: &[String]) -> bool {
        let popped = pop_segment(&self.input.text);
        self.input.set(popped);
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// Bookkeeping after a free-form edit to the path field: reset the suggestion highlight and
    /// report whether the dir portion moved.
    pub fn path_edited(&mut self, project_paths: &[String]) -> bool {
        self.suggestion_idx = 0;
        self.sync_dir_listing(project_paths)
    }

    /// True when the path is *definitely* unsaveable as typed — the red-worthy condition: the dir
    /// portion failed to list (its parent directory doesn't exist or sits outside the project
    /// boundary). The filename leaf is free, so it never invalidates; a `Pending` listing is
    /// unknown, not invalid.
    pub fn path_invalid(&self) -> bool {
        matches!(self.listing_state, DirListingState::Failed)
    }

    /// The `(path_index, relative_path)` a commit should save to — the literal typed path under
    /// the chosen root. `None` for an empty path (nothing to save to). Absolute paths (a leading
    /// `/`) are handled by the caller, which re-resolves them against the roots.
    pub fn save_target(&self, project_paths: &[String]) -> Option<(u32, String)> {
        let path = self.input.text.trim().to_string();
        if path.is_empty() {
            return None;
        }
        let path_index = if project_paths.len() > 1 {
            self.chosen_root(&root_labels(project_paths))
        } else {
            0
        };
        Some((path_index, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> DirectoryEntry {
        DirectoryEntry {
            name: name.into(),
            is_dir,
        }
    }

    #[test]
    fn single_root_path_listing_and_ghost() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = SaveAsEditor::new(String::new(), ChipEditorField::Path, 0);
        // First sync establishes the listing key and asks for a refetch.
        assert!(ed.sync_dir_listing(&roots));
        assert_eq!(ed.listing_dir_abs, "/tmp/root");
        assert_eq!(ed.listing_state, DirListingState::Pending);

        ed.set_dir_listing(vec![entry("src", true), entry("main.rs", false)]);
        // A directory ghost ends in `/`; the file is the second match.
        ed.input.set("s".into());
        assert_eq!(ed.path_ghost().as_deref(), Some("rc/"));
        ed.input.set("m".into());
        assert_eq!(ed.path_ghost().as_deref(), Some("ain.rs"));
    }

    #[test]
    fn accepting_a_dir_refetches_but_a_file_does_not() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = SaveAsEditor::new(String::new(), ChipEditorField::Path, 0);
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing(vec![entry("src", true), entry("main.rs", false)]);

        // Accept the directory: input grows to `src/`, dir portion moved → refetch.
        ed.input.set("sr".into());
        assert!(ed.accept_path_suggestion(&roots));
        assert_eq!(ed.input.text, "src/");

        // Now under `src/`, accept a file: input grows to `src/main.rs`, no refetch.
        ed.set_dir_listing(vec![entry("main.rs", false)]);
        ed.input.set("src/ma".into());
        assert!(!ed.accept_path_suggestion(&roots));
        assert_eq!(ed.input.text, "src/main.rs");
    }

    #[test]
    fn save_target_is_literal_input_not_the_suggestion() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = SaveAsEditor::new(String::new(), ChipEditorField::Path, 0);
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing(vec![entry("macros", true)]);
        // Typing `ma` highlights `macros/` as a ghost, but Enter saves the literal `ma`.
        ed.input.set("ma".into());
        assert_eq!(ed.save_target(&roots), Some((0, "ma".into())));
        // Empty input has nothing to save to.
        ed.input.set(String::new());
        assert_eq!(ed.save_target(&roots), None);
    }

    #[test]
    fn missing_parent_dir_is_advisory_invalid() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = SaveAsEditor::new("nope/file.rs".into(), ChipEditorField::Path, 0);
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing_failed();
        assert!(ed.path_invalid());
        // ...but it's still a save target; the server reports the real error.
        assert_eq!(ed.save_target(&roots), Some((0, "nope/file.rs".into())));
    }

    #[test]
    fn multi_root_field_resolves_chosen_root() {
        let roots = vec!["/work/api".to_string(), "/personal/web".to_string()];
        let labels = root_labels(&roots);
        let mut ed = SaveAsEditor::new(String::new(), ChipEditorField::Root, 0);
        // Filter to the second root, then commit the root field → focus moves to the path.
        ed.root_filter.set("web".into());
        ed.root_selected = 0;
        assert!(!ed.root_invalid(&labels));
        let refetch = ed.commit_root_field(&labels, &roots);
        assert_eq!(ed.field, ChipEditorField::Path);
        assert!(refetch);
        assert_eq!(ed.chosen_root(&labels), 1);
        assert!(ed
            .dir_listing_path(&roots)
            .unwrap()
            .starts_with("/personal/web"));
    }
}
