//! Save-as prompt state + transitions.
//!
//! Two modes:
//!
//! - `Editing`: the common case. The user types a project-relative path into a single input
//!   field. A ghost-style suggestion (gray text after the cursor) shows the first matching
//!   directory entry; Alt-j/Alt-k cycles matches; Tab / Alt-l commits the ghost into the
//!   input (the same accept gesture as the pickers' dir editor). The typed `/` is just a
//!   separator — there's no "committed path prefix" concept any more. Alt-Backspace deletes
//!   the rightmost path segment (fish-style); at empty input it (and plain Backspace) peels
//!   into `SelectingRoot` (multi-root only).
//!
//! - `SelectingRoot`: only entered in multi-root projects, when the user has Backspaced past
//!   the empty input. The user picks one of the project roots — by cycling via Alt-j/k
//!   (wrapping, like the dir editor's root typeahead) or by typing a smartcase prefix filter
//!   against root labels. Tab / Alt-l / Enter commits, as does typing `:` on a completed
//!   label. Alt-Backspace clears any typed filter / cycled candidate back to the bare
//!   just-peeled state.
//!
//! In multi-root projects the chosen root's label renders as a blue committed prefix to the
//! left of the editable area. The label is not part of the input — you can't type into it.
//!
//! State is held entirely client-side; the only server contact is a `directory/list` RPC,
//! fired whenever the dir portion of the typed path changes (or when SelectingRoot transitions
//! into a fresh Editing).

use crate::text_input::TextInput;
use aether_protocol::directory::DirectoryEntry;

/// One save-prompt instance.
#[derive(Debug, Clone)]
pub struct SavePromptState {
    pub mode: PromptMode,
    pub input: TextInput,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub enum PromptMode {
    Editing(EditingState),
    SelectingRoot(SelectingRoot),
}

/// Editing-mode state. The whole project-relative path lives in `input.text`; this struct just
/// remembers which root we're saving into, the most recently fetched directory listing, and the
/// position within the filtered match set that produces the current ghost suggestion.
#[derive(Debug, Clone)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub struct EditingState {
    pub path_index: u32,
    /// Cached listing for the directory portion of `input.text` (whatever was typed up to and
    /// including the last `/`). Refetched whenever `dir_of_input` would differ from
    /// `listing_dir_abs`.
    pub listing: Vec<DirectoryEntry>,
    /// Canonical absolute path of the directory `listing` was fetched against. Lets us detect
    /// when an edit moved the dir portion (e.g. user typed `/` or Alt-Backspaced) and the
    /// listing is stale.
    pub listing_dir_abs: String,
    /// Position in the filtered match set (`matches`) of the currently-displayed ghost
    /// suggestion. Reset to 0 on any edit. Alt-j/k navigates within matches.
    pub suggestion_idx: usize,
}

/// Root-selection state. Reached by Alt-Backspace at empty input in multi-root projects. Uses
/// the same ghost-suggestion shape as Editing: `suggestion_idx` is a position in the *filtered*
/// match list; the renderer pulls the suffix of `labels[matches[suggestion_idx]]` (beyond the
/// typed input) as the gray ghost; Tab commits.
///
/// `from_root` is the root that was active when we peeled. On entry `suggestion_idx` defaults
/// to from_root's position in the (initially unfiltered) match list — so an untouched Tab
/// commits the root we came from, making an accidental peel one keystroke to undo. Any typing
/// resets `suggestion_idx` to 0 (the first filtered match).
#[derive(Debug, Clone)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub struct SelectingRoot {
    pub from_root: u32,
    pub suggestion_idx: usize,
}

impl SavePromptState {
    /// The gray suffix to render after the cursor, or `None` when no ghost is visible. Visible
    /// only when (a) the cursor is at the end of the input, and (b) at least one candidate
    /// prefix-matches the relevant portion of the input — the partial leaf in Editing, or the
    /// whole input in SelectingRoot (root labels are matched as a whole, not segment-wise).
    pub fn ghost_suffix(&self, project_paths: &[String]) -> Option<String> {
        if !self.cursor_at_end() {
            return None;
        }
        match &self.mode {
            PromptMode::Editing(e) => {
                let partial = partial_of_input(&self.input.text);
                let matches = matching_indices(&e.listing, partial);
                let pick = *matches.get(e.suggestion_idx)?;
                let entry = e.listing.get(pick)?;
                let mut suffix: String = entry.name.chars().skip(partial.chars().count()).collect();
                if entry.is_dir {
                    suffix.push('/');
                }
                Some(suffix)
            }
            PromptMode::SelectingRoot(sr) => {
                let labels = crate::labels::root_labels(project_paths);
                let matches = matching_root_indices(&labels, &self.input.text);
                let pick = *matches.get(sr.suggestion_idx)?;
                let label = labels.get(pick)?;
                let typed = self.input.text.chars().count();
                let suffix: String = label.chars().skip(typed).collect();
                Some(suffix)
            }
        }
    }

    /// `(position, total)` for the cycle counter — 1-based position within the *filtered*
    /// match set, total match count. Returns `None` when there's nothing useful to show
    /// (cursor not at end, or ≤ 1 match).
    pub fn cycle_position(&self, project_paths: &[String]) -> Option<(usize, usize)> {
        if !self.cursor_at_end() {
            return None;
        }
        let (idx, total) = match &self.mode {
            PromptMode::Editing(e) => {
                let partial = partial_of_input(&self.input.text);
                let matches = matching_indices(&e.listing, partial);
                (e.suggestion_idx, matches.len())
            }
            PromptMode::SelectingRoot(sr) => {
                let labels = crate::labels::root_labels(project_paths);
                let matches = matching_root_indices(&labels, &self.input.text);
                (sr.suggestion_idx, matches.len())
            }
        };
        if total <= 1 {
            return None;
        }
        Some((idx + 1, total))
    }

    /// `true` when the input cursor sits at the very end of the input text. Many UI rules
    /// depend on this (ghost visibility, Tab semantics).
    pub fn cursor_at_end(&self) -> bool {
        self.input.cursor == self.input.text.len()
    }
}

// ---- pure helpers ------------------------------------------------------------------------------

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

    // ---- prompt-open ----

    // ---- ghost suggestion ----

    // ---- Tab ----

    // ---- typing / backspace / `/` ----

    // ---- Alt-Backspace ----

    // ---- SelectingRoot retained behaviour ----

    // ---- save_target / enter_action ----

    // ---- pure helpers ----

    #[test]
    fn split_input_examples() {
        assert_eq!(split_input("src/foo/file.txt"), ("src/foo/", "file.txt"));
        assert_eq!(split_input("src/foo/"), ("src/foo/", ""));
        assert_eq!(split_input("src"), ("", "src"));
        assert_eq!(split_input(""), ("", ""));
    }
}
