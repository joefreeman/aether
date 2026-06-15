//! Save-as prompt view model (`Alt-s`).
//!
//! A render-side mirror of the core [`aether_client::save_as::SaveAsEditor`], shaped exactly like
//! the picker's dir [`crate::picker::ChipEditor`] so the two share their look and muscle memory:
//! a multi-root projects' leading **root** field (smartcase typeahead, `:` separator) ahead of a
//! `directory/list`-backed **path** field with ghost suggestions, `Tab`/`Alt-l` accept, `Alt-j`/`k`
//! cycle, and `Alt-Backspace` segment pop.
//!
//! Two deliberate departures from the dir chip editor, because the path's final segment is a *new
//! filename* rather than an existing subdirectory:
//!
//! - The cached listing keeps **files as well as directories**; completing onto an existing file is
//!   how you overwrite it. The path ghost appends `/` only behind a directory (a file completes
//!   outright).
//! - The path is only ever flagged red when its *parent* directory failed to list
//!   ([`SavePromptState::path_invalid`]); a non-matching filename leaf is fine (you're naming a file
//!   that needn't exist yet).
//!
//! This struct holds no logic the commit cares about — the core owns the value and the command
//! keys. The shell syncs the focused field's text via `save_as_set_input` / `save_as_set_root_filter`
//! and keeps the caret in its own [`crate::overlay_input`] editor. Everything here exists only to
//! render: `field`, the per-segment text (with carets baked into the `TextInput`s during the
//! `save_as_view` projection), the cached listing, and the typeahead bookkeeping.

use crate::picker::{ChipEditorField, DirListingState};
use crate::text_input::TextInput;
use aether_protocol::directory::DirectoryEntry;

/// One save-prompt instance — the render mirror of [`aether_client::save_as::SaveAsEditor`].
#[derive(Debug, Clone)]
pub struct SavePromptState {
    /// Which segment has focus. Always `Path` in single-root projects.
    pub field: ChipEditorField,
    /// The root-relative path being typed (directory portion + filename leaf). Caret baked in.
    pub input: TextInput,
    /// Multi-root: the prefix filter typed into the root field. Caret baked in.
    pub root_filter: TextInput,
    /// Multi-root: highlight within the root candidates matching the current filter.
    pub root_selected: usize,
    /// The root the editor opened with — the fallback when the filter matches nothing.
    pub root_index: u32,
    /// Whether the project has more than one root (so the root field exists at all).
    pub multi_root: bool,
    /// Cached `directory/list` entries (files *and* directories) for the dir portion of `input`.
    pub listing: Vec<DirectoryEntry>,
    /// The absolute path `listing` was last synced against — the staleness key (unused by the
    /// renderer, carried for parity with the core editor / debugging).
    #[allow(dead_code)] // view-model surface synced from the core
    pub listing_dir_abs: String,
    /// Where `listing` stands relative to `listing_dir_abs`.
    pub listing_state: DirListingState,
    /// Position within the filtered match set producing the current path ghost.
    pub suggestion_idx: usize,
}

impl SavePromptState {
    // ---- root field (multi-root only) ----------------------------------------------------------

    /// The root field's ghost completion: the current match's root index and the part of its label
    /// beyond the typed prefix (rendered gray after the caret). `None` when nothing matches — the
    /// red typed filter is then the cue. `labels` are the project's disambiguated root labels.
    pub fn root_ghost(&self, labels: &[String]) -> Option<(usize, String)> {
        let candidates = matching_root_indices(labels, &self.root_filter.text);
        let &idx = candidates.get(self.root_selected.min(candidates.len().saturating_sub(1)))?;
        let typed_chars = self.root_filter.text.chars().count();
        let suffix: String = labels[idx].chars().skip(typed_chars).collect();
        Some((idx, suffix))
    }

    /// The chosen root's label — the blue committed prefix shown while focus is in the path.
    /// Falls back to the opening root's label when the filter matches nothing.
    pub fn root_display(&self, labels: &[String]) -> String {
        let chosen = match self.root_ghost(labels) {
            Some((idx, _)) => idx,
            None => self.root_index as usize,
        };
        labels.get(chosen).cloned().unwrap_or_default()
    }

    /// True when the root field would refuse a commit: a non-empty filter that prefix-matches no
    /// root label. (An empty filter matches every root.) The invalid field renders red.
    pub fn root_invalid(&self, labels: &[String]) -> bool {
        matching_root_indices(labels, &self.root_filter.text).is_empty()
    }

    // ---- path field ----------------------------------------------------------------------------

    /// The path field's ghost: the rest of the current match beyond the partial leaf, plus a
    /// trailing `/` only when the match is a directory (a file completes outright — the save-as
    /// idiom's one departure from the dir chip editor). Visible only with the caret at the end of
    /// the input.
    pub fn path_ghost(&self) -> Option<String> {
        if self.input.cursor != self.input.text.len() {
            return None;
        }
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

    /// True when the path is *definitely* unsaveable as typed — the red-worthy condition: the dir
    /// portion failed to list (its parent directory doesn't exist or sits outside the project
    /// boundary). The filename leaf is free, so it never invalidates; a `Pending` listing is
    /// unknown, not invalid.
    pub fn path_invalid(&self) -> bool {
        matches!(self.listing_state, DirListingState::Failed)
    }
}

// ---- pure helpers ------------------------------------------------------------------------------
// These are re-used by `crate::picker::ChipEditor` (path-validity, ghost matching), so their names
// and signatures are load-bearing — keep them.

/// Split an input string at the last `/`, returning the `dir_part` (everything up to and
/// including the last `/`, possibly empty) and the `partial_leaf` (everything after, the
/// leaf-being-typed). Examples:
///   "src/foo/file.txt" → ("src/foo/", "file.txt")
///   "src/foo/"         → ("src/foo/", "")
///   "src"              → ("", "src")
///   ""                 → ("", "")
fn split_input(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(i) => input.split_at(i + 1),
        None => ("", input),
    }
}

pub(crate) fn partial_of_input(input: &str) -> &str {
    split_input(input).1
}

/// Indices into `listing` that smartcase-prefix-match `filter`. Empty filter matches all.
pub fn matching_indices(listing: &[DirectoryEntry], filter: &str) -> Vec<usize> {
    let (needle, has_upper) = smartcase_needle(filter);
    if filter.is_empty() {
        return (0..listing.len()).collect();
    }
    let mut buf = String::new();
    let mut out = Vec::new();
    for (i, entry) in listing.iter().enumerate() {
        if prefix_matches(&entry.name, &needle, has_upper, &mut buf) {
            out.push(i);
        }
    }
    out
}

/// Same matching rule applied to project-root labels.
pub fn matching_root_indices(root_labels: &[String], filter: &str) -> Vec<usize> {
    let (needle, has_upper) = smartcase_needle(filter);
    if filter.is_empty() {
        return (0..root_labels.len()).collect();
    }
    let mut buf = String::new();
    let mut out = Vec::new();
    for (i, label) in root_labels.iter().enumerate() {
        if prefix_matches(label, &needle, has_upper, &mut buf) {
            out.push(i);
        }
    }
    out
}

fn smartcase_needle(filter: &str) -> (String, bool) {
    let has_upper = filter.chars().any(|c| c.is_uppercase());
    let needle: String = if has_upper {
        filter.to_string()
    } else {
        filter.chars().flat_map(char::to_lowercase).collect()
    };
    (needle, has_upper)
}

fn prefix_matches(haystack: &str, needle: &str, has_upper: bool, buf: &mut String) -> bool {
    if has_upper {
        haystack.starts_with(needle)
    } else {
        buf.clear();
        buf.extend(haystack.chars().flat_map(char::to_lowercase));
        buf.starts_with(needle)
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

    fn prompt(input: &str, listing: Vec<DirectoryEntry>) -> SavePromptState {
        SavePromptState {
            field: ChipEditorField::Path,
            input: TextInput::new(input),
            root_filter: TextInput::default(),
            root_selected: 0,
            root_index: 0,
            multi_root: false,
            listing,
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Loaded,
            suggestion_idx: 0,
        }
    }

    #[test]
    fn split_input_examples() {
        assert_eq!(split_input("src/foo/file.txt"), ("src/foo/", "file.txt"));
        assert_eq!(split_input("src/foo/"), ("src/foo/", ""));
        assert_eq!(split_input("src"), ("", "src"));
        assert_eq!(split_input(""), ("", ""));
    }

    #[test]
    fn path_ghost_includes_files_and_marks_dirs() {
        let p = prompt("s", vec![entry("src", true), entry("setup.rs", false)]);
        // A directory ghost ends in `/`.
        assert_eq!(p.path_ghost().as_deref(), Some("rc/"));
        // A file completes without a trailing slash.
        let p = prompt("se", vec![entry("setup.rs", false)]);
        assert_eq!(p.path_ghost().as_deref(), Some("tup.rs"));
    }

    #[test]
    fn path_ghost_hidden_when_caret_not_at_end() {
        let mut p = prompt("src", vec![entry("src", true)]);
        p.input.cursor = 1; // caret mid-input
        assert_eq!(p.path_ghost(), None);
    }

    #[test]
    fn path_invalid_only_when_parent_failed() {
        let mut p = prompt("nope/file.rs", Vec::new());
        p.listing_state = DirListingState::Pending;
        assert!(!p.path_invalid());
        p.listing_state = DirListingState::Failed;
        assert!(p.path_invalid());
    }

    #[test]
    fn root_ghost_and_invalid() {
        let labels = vec!["api".to_string(), "web".to_string()];
        let mut p = prompt("", Vec::new());
        p.multi_root = true;
        p.field = ChipEditorField::Root;
        p.root_filter.set("we");
        p.root_selected = 0;
        assert_eq!(p.root_ghost(&labels), Some((1, "b".into())));
        assert!(!p.root_invalid(&labels));
        // A filter matching nothing is invalid (and has no ghost).
        p.root_filter.set("zzz");
        assert_eq!(p.root_ghost(&labels), None);
        assert!(p.root_invalid(&labels));
    }
}
