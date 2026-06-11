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
pub enum PromptMode {
    Editing(EditingState),
    SelectingRoot(SelectingRoot),
}

/// Editing-mode state. The whole project-relative path lives in `input.text`; this struct just
/// remembers which root we're saving into, the most recently fetched directory listing, and the
/// position within the filtered match set that produces the current ghost suggestion.
#[derive(Debug, Clone)]
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
pub struct SelectingRoot {
    pub from_root: u32,
    pub suggestion_idx: usize,
}

/// What the caller should do after a transition that may have changed the prefix dir. The
/// caller is responsible for the side effects (refetching the listing, redrawing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionHint {
    /// Nothing to do beyond redraw.
    None,
    /// The dir portion of the prompt changed; caller should fire a fresh `directory/list`.
    RefreshListing,
}

/// What pressing Enter should do, given the current prompt state. Enter never silently closes
/// the prompt — Esc is the only explicit cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterAction {
    /// Issue the `buffer/save` RPC using [`SavePromptState::save_target`]. The literal input
    /// text is saved — the ghost suggestion is *not* auto-committed (use Tab for that). This
    /// lets the user save a name that happens to be a prefix of an existing file.
    Save,
    /// Behave like Tab — commit the SelectingRoot cycle into a fresh Editing.
    Tab,
    /// No saveable target yet (empty input in Editing, or otherwise nothing to save). The
    /// prompt stays open.
    Nothing,
}

impl SavePromptState {
    /// Open the prompt pre-filled for an existing file. The input is the file's project-
    /// relative path; cursor sits at the end (no ghost suggestion until the user starts editing).
    pub fn open_for_existing(abs_path: &str, project_paths: &[String]) -> (Self, TransitionHint) {
        let (path_index, relative) = classify_abs(abs_path, project_paths).unwrap_or_else(|| {
            // File outside every root — fall back to root 0 with the raw absolute path.
            (0, abs_path.to_string())
        });
        let listing_dir_abs = dir_of_path(project_paths, path_index, &relative);
        let state = SavePromptState {
            mode: PromptMode::Editing(EditingState {
                path_index,
                listing: Vec::new(),
                listing_dir_abs,
                suggestion_idx: 0,
            }),
            input: TextInput::new(relative),
        };
        (state, TransitionHint::RefreshListing)
    }

    /// Open the prompt for a scratch buffer. Defaults to root 0 with an empty input.
    pub fn open_for_scratch(project_paths: &[String]) -> (Self, TransitionHint) {
        let listing_dir_abs = project_paths.first().cloned().unwrap_or_default();
        let state = SavePromptState {
            mode: PromptMode::Editing(EditingState {
                path_index: 0,
                listing: Vec::new(),
                listing_dir_abs,
                suggestion_idx: 0,
            }),
            input: TextInput::default(),
        };
        (state, TransitionHint::RefreshListing)
    }

    /// Store the response from `directory/list`. No-op in `SelectingRoot` (the cycled roots
    /// come from `project_paths`, not the server).
    pub fn set_listing(&mut self, entries: Vec<DirectoryEntry>) {
        if let PromptMode::Editing(e) = &mut self.mode {
            e.listing = entries;
            e.suggestion_idx = 0;
        }
    }

    /// The canonical absolute path the caller should hand to `directory/list`. Returns the dir
    /// portion of the current input (resolved against the chosen root) in Editing; `None` in
    /// SelectingRoot.
    pub fn listing_path(&self, project_paths: &[String]) -> Option<String> {
        match &self.mode {
            PromptMode::Editing(e) => {
                Some(dir_of_path(project_paths, e.path_index, &self.input.text))
            }
            PromptMode::SelectingRoot(_) => None,
        }
    }

    /// Insert a typed char. In `Editing` the char goes into `input` as-is — `/` no longer has
    /// any special "promote" behaviour, it's just a separator and the next listing refresh
    /// picks up the new dir portion. In `SelectingRoot` the char builds a smartcase prefix
    /// filter against root labels.
    pub fn type_char(&mut self, c: char, project_paths: &[String]) -> TransitionHint {
        match &mut self.mode {
            PromptMode::SelectingRoot(sr) => {
                self.input.insert_char(c);
                sr.suggestion_idx = 0;
                TransitionHint::None
            }
            PromptMode::Editing(e) => {
                let dir_before = dir_of_input(&self.input.text).to_string();
                self.input.insert_char(c);
                e.suggestion_idx = 0;
                let dir_after = dir_of_input(&self.input.text).to_string();
                if dir_after != dir_before {
                    let abs = join_root_relative(project_paths, e.path_index, &dir_after);
                    e.listing_dir_abs = abs;
                    e.listing.clear();
                    TransitionHint::RefreshListing
                } else {
                    TransitionHint::None
                }
            }
        }
    }

    /// Backspace: deletes one char from `input`. If the dir portion shrank (the deleted char
    /// was a `/`, or backspacing into a previous segment), trigger a refetch. At an *empty*
    /// Editing input, peels into `SelectingRoot` (multi-root only) — the same leftward
    /// gesture the dir editor uses to step from the path back into the root segment.
    pub fn backspace(&mut self, project_paths: &[String]) -> TransitionHint {
        match &mut self.mode {
            PromptMode::SelectingRoot(sr) => {
                self.input.backspace();
                sr.suggestion_idx = 0;
                TransitionHint::None
            }
            PromptMode::Editing(e) => {
                if self.input.text.is_empty() {
                    if project_paths.len() > 1 {
                        let from_root = e.path_index;
                        self.mode = PromptMode::SelectingRoot(SelectingRoot {
                            from_root,
                            suggestion_idx: from_root as usize,
                        });
                    }
                    return TransitionHint::None;
                }
                let dir_before = dir_of_input(&self.input.text).to_string();
                self.input.backspace();
                e.suggestion_idx = 0;
                let dir_after = dir_of_input(&self.input.text).to_string();
                if dir_after != dir_before {
                    let abs = join_root_relative(project_paths, e.path_index, &dir_after);
                    e.listing_dir_abs = abs;
                    e.listing.clear();
                    TransitionHint::RefreshListing
                } else {
                    TransitionHint::None
                }
            }
        }
    }

    /// Alt-Backspace:
    ///   1. In `SelectingRoot` with typed/cycled state → clear it. Bare SelectingRoot is a
    ///      no-op (we're at the top of the stack).
    ///   2. In `Editing` with non-empty input → delete the rightmost path segment, fish-style.
    ///   3. In `Editing` with empty input → peel into SelectingRoot (multi-root) or no-op
    ///      (single-root).
    pub fn alt_backspace(&mut self, project_paths: &[String]) -> TransitionHint {
        if let PromptMode::SelectingRoot(sr) = &mut self.mode {
            // Clear any typed filter, re-defaulting the suggestion to from_root (the same state
            // as immediately after peeling). Idempotent in bare SelectingRoot.
            self.input.clear();
            sr.suggestion_idx = sr.from_root as usize;
            return TransitionHint::None;
        }
        let PromptMode::Editing(e) = &mut self.mode else {
            return TransitionHint::None;
        };
        if self.input.text.is_empty() {
            // Peel into SelectingRoot if multi-root; single-root is a no-op (only one root, no
            // pick to make). Initial suggestion_idx is from_root's position in the unfiltered
            // match list (= from_root itself), so the default ghost shows the root we came
            // from — Tab without cycling effectively undoes the peel.
            if project_paths.len() > 1 {
                let from_root = e.path_index;
                self.mode = PromptMode::SelectingRoot(SelectingRoot {
                    from_root,
                    suggestion_idx: from_root as usize,
                });
            }
            return TransitionHint::None;
        }
        let new_text = pop_segment(&self.input.text);
        self.input.set(new_text);
        e.suggestion_idx = 0;
        let dir_after = dir_of_input(&self.input.text).to_string();
        let abs = join_root_relative(project_paths, e.path_index, &dir_after);
        if abs != e.listing_dir_abs {
            e.listing_dir_abs = abs;
            e.listing.clear();
            TransitionHint::RefreshListing
        } else {
            TransitionHint::None
        }
    }

    /// Alt-j: advance to the next match. In `Editing`, cycles ghost suggestions through the
    /// filtered listing; in `SelectingRoot`, cycles project roots (filtered by typed input).
    pub fn alt_j(&mut self, project_paths: &[String]) {
        match &mut self.mode {
            PromptMode::SelectingRoot(_) => self.cycle_root(project_paths, 1),
            PromptMode::Editing(e) => {
                let partial = partial_of_input(&self.input.text);
                let matches = matching_indices(&e.listing, partial);
                if matches.is_empty() {
                    return;
                }
                if e.suggestion_idx + 1 < matches.len() {
                    e.suggestion_idx += 1;
                }
            }
        }
    }

    /// Alt-k: symmetric to `alt_j`.
    pub fn alt_k(&mut self, project_paths: &[String]) {
        match &mut self.mode {
            PromptMode::SelectingRoot(_) => self.cycle_root(project_paths, -1),
            PromptMode::Editing(e) => {
                let partial = partial_of_input(&self.input.text);
                let matches = matching_indices(&e.listing, partial);
                if matches.is_empty() {
                    return;
                }
                if e.suggestion_idx > 0 {
                    e.suggestion_idx -= 1;
                }
            }
        }
    }

    /// Tab: in `Editing`, commit the ghost suggestion (append its suffix to `input`). No-op
    /// when the ghost isn't visible — cursor not at end, or no matches. In `SelectingRoot`,
    /// commit the cycled root (or the first match of any typed filter, or `from_root`).
    pub fn tab(&mut self, project_paths: &[String]) -> TransitionHint {
        if let PromptMode::SelectingRoot(sr) = &self.mode {
            // Commit whatever root the ghost currently surfaces. Falls back to `from_root` when
            // there are no matches (typed input doesn't prefix-match any root label) — keeps an
            // accidental peel + nonsense type from silently switching roots.
            let labels = crate::labels::root_labels(project_paths);
            let matches = matching_root_indices(&labels, &self.input.text);
            let target = matches
                .get(sr.suggestion_idx)
                .copied()
                .unwrap_or(sr.from_root as usize);
            self.commit_root(target, project_paths);
            return TransitionHint::RefreshListing;
        }
        // Editing: append the visible ghost suffix (if any).
        let Some(suffix) = self.ghost_suffix(project_paths) else {
            return TransitionHint::None;
        };
        // Insert the suffix at the cursor (which is at end, since `ghost_suffix` short-circuits
        // otherwise).
        for c in suffix.chars() {
            self.input.insert_char(c);
        }
        let PromptMode::Editing(e) = &mut self.mode else {
            return TransitionHint::None;
        };
        e.suggestion_idx = 0;
        // If the suffix ended in `/` (directory completion), the dir portion has grown —
        // refresh the listing for the new dir.
        let ends_with_slash = self.input.text.ends_with('/');
        let dir_after = dir_of_input(&self.input.text).to_string();
        if ends_with_slash {
            let abs = join_root_relative(project_paths, e.path_index, &dir_after);
            if abs != e.listing_dir_abs {
                e.listing_dir_abs = abs;
                e.listing.clear();
                return TransitionHint::RefreshListing;
            }
        }
        TransitionHint::None
    }

    /// Enter routing. See [`EnterAction`]. Note that Enter does *not* auto-commit a ghost
    /// suggestion — that's Tab's job — so the user can save a name that happens to prefix an
    /// existing file.
    pub fn enter_action(&self) -> EnterAction {
        match &self.mode {
            PromptMode::SelectingRoot(_) => EnterAction::Tab,
            PromptMode::Editing(_) => {
                if self.input.text.trim().is_empty() {
                    EnterAction::Nothing
                } else {
                    EnterAction::Save
                }
            }
        }
    }

    /// Resolve to the `(path_index, relative_path)` pair the save RPC needs. `None` in
    /// `SelectingRoot` or when the input trims to empty.
    pub fn save_target(&self) -> Option<(u32, String)> {
        let PromptMode::Editing(e) = &self.mode else {
            return None;
        };
        let trimmed = self.input.text.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some((e.path_index, trimmed.to_string()))
    }

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

    /// Step one position through the filtered project roots (used by Alt-j / Alt-k in
    /// SelectingRoot). Wraps at both ends, matching the dir editor's root typeahead — the
    /// root set is small and cycling round beats bumping into an edge. (The Editing ghost
    /// cycle still clamps: directory listings can be long, and wrapping there loses track of
    /// where you are.)
    fn cycle_root(&mut self, project_paths: &[String], delta: i32) {
        let PromptMode::SelectingRoot(sr) = &mut self.mode else {
            return;
        };
        if project_paths.len() < 2 {
            return;
        }
        let labels = crate::labels::root_labels(project_paths);
        let matches = matching_root_indices(&labels, &self.input.text);
        if matches.is_empty() {
            return;
        }
        let n = matches.len() as i64;
        let sel = (sr.suggestion_idx as i64).min(n - 1);
        sr.suggestion_idx = (sel + delta as i64).rem_euclid(n) as usize;
    }

    /// Transition from `SelectingRoot` back to a fresh `Editing` at the chosen root. Input is
    /// cleared; the caller's RefreshListing hint refetches the new root's top-level directory.
    fn commit_root(&mut self, root_idx: usize, project_paths: &[String]) {
        let path_index = root_idx as u32;
        let listing_dir_abs = project_paths.get(root_idx).cloned().unwrap_or_default();
        self.mode = PromptMode::Editing(EditingState {
            path_index,
            listing: Vec::new(),
            listing_dir_abs,
            suggestion_idx: 0,
        });
        self.input.clear();
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

pub(crate) fn dir_of_input(input: &str) -> &str {
    split_input(input).0
}

pub(crate) fn partial_of_input(input: &str) -> &str {
    split_input(input).1
}

/// Fish-style segment delete: drop the rightmost segment, keeping the parent's trailing `/`
/// when one exists.
///   "src/foo/file.txt" → "src/foo/"
///   "src/foo/"         → "src/"
///   "src/"             → ""
///   "src/foo"          → "src/"
///   "src"              → ""
pub(crate) fn pop_segment(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    if let Some(stripped) = input.strip_suffix('/') {
        // Trailing slash: delete the slash + the segment before it (if any).
        match stripped.rfind('/') {
            Some(i) => stripped[..=i].to_string(),
            None => String::new(),
        }
    } else {
        // No trailing slash: delete back to (and including) the last `/`, if any.
        match input.rfind('/') {
            Some(i) => input[..=i].to_string(),
            None => String::new(),
        }
    }
}

/// Resolve `dir_part` (project-relative, possibly with trailing `/`) under the chosen root.
pub(crate) fn join_root_relative(
    project_paths: &[String],
    path_index: u32,
    dir_part: &str,
) -> String {
    let Some(root) = project_paths.get(path_index as usize) else {
        return String::new();
    };
    if dir_part.is_empty() {
        return root.clone();
    }
    // dir_part always ends with `/` when non-empty (it's the prefix up to and including the
    // last `/` in the input); trim it for Path::join.
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

/// Dir-of an existing-file's relative path, resolved under the chosen root. Used at prompt
/// open time to seed `listing_dir_abs`.
fn dir_of_path(project_paths: &[String], path_index: u32, relative: &str) -> String {
    join_root_relative(project_paths, path_index, dir_of_input(relative))
}

/// Classify an absolute file path against the project's roots, returning the longest-match
/// `(path_index, project_relative_path)`. `None` when outside every root.
fn classify_abs(abs: &str, project_paths: &[String]) -> Option<(u32, String)> {
    let p = std::path::Path::new(abs);
    project_paths
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            let r = std::path::Path::new(root);
            p.strip_prefix(r)
                .ok()
                .map(|rel| (i, r.as_os_str().len(), rel.display().to_string()))
        })
        .max_by_key(|(_, len, _)| *len)
        .map(|(i, _, rel)| (i as u32, rel))
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

    fn paths() -> Vec<String> {
        vec!["/proj".into()]
    }

    fn editing_at_root(project_paths: &[String]) -> SavePromptState {
        SavePromptState::open_for_scratch(project_paths).0
    }

    // ---- prompt-open ----

    #[test]
    fn opens_for_existing_seeds_input_with_relative_path() {
        let (state, hint) = SavePromptState::open_for_existing("/proj/src/app.rs", &paths());
        assert_eq!(hint, TransitionHint::RefreshListing);
        let PromptMode::Editing(e) = &state.mode else {
            panic!("expected editing");
        };
        assert_eq!(e.path_index, 0);
        assert_eq!(e.listing_dir_abs, "/proj/src");
        assert_eq!(state.input.text, "src/app.rs");
        assert_eq!(state.input.cursor, "src/app.rs".len());
    }

    #[test]
    fn opens_for_scratch_lands_at_root_with_empty_input() {
        let (state, hint) = SavePromptState::open_for_scratch(&paths());
        assert_eq!(hint, TransitionHint::RefreshListing);
        let PromptMode::Editing(e) = &state.mode else {
            panic!("expected editing");
        };
        assert_eq!(e.listing_dir_abs, "/proj");
        assert!(state.input.text.is_empty());
    }

    // ---- ghost suggestion ----

    #[test]
    fn ghost_shows_suffix_of_first_match_at_empty_partial() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false), entry("main.rs", false)]);
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("lib.rs"));
    }

    #[test]
    fn ghost_shows_suffix_when_partial_matches() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false), entry("main.rs", false)]);
        s.type_char('l', &paths());
        // Partial "l" → match "lib.rs" → suffix "ib.rs".
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("ib.rs"));
    }

    #[test]
    fn ghost_appends_slash_for_directories() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("src", true)]);
        s.type_char('s', &paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("rc/"));
    }

    #[test]
    fn ghost_hidden_when_cursor_not_at_end() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false)]);
        s.type_char('l', &paths());
        s.input.move_left();
        assert_eq!(s.ghost_suffix(&paths()), None);
    }

    #[test]
    fn ghost_hidden_when_no_matches() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false)]);
        s.type_char('z', &paths());
        assert_eq!(s.ghost_suffix(&paths()), None);
    }

    #[test]
    fn alt_j_advances_through_matches_without_wrap() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![
            entry("foo", false),
            entry("foobar", false),
            entry("foobaz", false),
        ]);
        // All match the empty partial — Alt-j moves through them.
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("foo"));
        s.alt_j(&paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("foobar"));
        s.alt_j(&paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("foobaz"));
        // No wrap.
        s.alt_j(&paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("foobaz"));
    }

    #[test]
    fn alt_k_steps_back() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![
            entry("a", false),
            entry("b", false),
            entry("c", false),
        ]);
        s.alt_j(&paths());
        s.alt_j(&paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("c"));
        s.alt_k(&paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("b"));
    }

    // ---- Tab ----

    #[test]
    fn tab_appends_ghost_suffix_to_input() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false)]);
        s.type_char('l', &paths());
        let h = s.tab(&paths());
        assert_eq!(h, TransitionHint::None);
        assert_eq!(s.input.text, "lib.rs");
    }

    #[test]
    fn tab_into_directory_appends_slash_and_refreshes() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("src", true)]);
        let h = s.tab(&paths());
        assert_eq!(h, TransitionHint::RefreshListing);
        assert_eq!(s.input.text, "src/");
        let PromptMode::Editing(e) = &s.mode else {
            panic!();
        };
        assert_eq!(e.listing_dir_abs, "/proj/src");
    }

    #[test]
    fn tab_is_no_op_when_no_ghost_visible() {
        let mut s = editing_at_root(&paths());
        // Empty listing → no ghost. Tab should do nothing.
        let h = s.tab(&paths());
        assert_eq!(h, TransitionHint::None);
        assert!(s.input.text.is_empty());
    }

    #[test]
    fn tab_is_no_op_when_cursor_not_at_end() {
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("lib.rs", false)]);
        s.type_char('l', &paths());
        s.input.move_left();
        s.tab(&paths());
        // Cursor wasn't at end — Tab is suppressed.
        assert_eq!(s.input.text, "l");
    }

    // ---- typing / backspace / `/` ----

    #[test]
    fn typing_slash_marks_dir_change_and_requests_refresh() {
        let mut s = editing_at_root(&paths());
        let h = s.type_char('s', &paths());
        assert_eq!(h, TransitionHint::None);
        let h = s.type_char('/', &paths());
        assert_eq!(h, TransitionHint::RefreshListing);
        let PromptMode::Editing(e) = &s.mode else {
            panic!();
        };
        assert_eq!(e.listing_dir_abs, "/proj/s");
    }

    #[test]
    fn backspacing_a_slash_marks_dir_change() {
        let mut s = editing_at_root(&paths());
        s.type_char('s', &paths());
        s.type_char('/', &paths());
        // Eat the response so set_listing has been called.
        s.set_listing(vec![]);
        let h = s.backspace(&paths());
        assert_eq!(h, TransitionHint::RefreshListing);
        let PromptMode::Editing(e) = &s.mode else {
            panic!();
        };
        assert_eq!(e.listing_dir_abs, "/proj");
    }

    // ---- Alt-Backspace ----

    #[test]
    fn alt_backspace_pops_trailing_segment_with_slash() {
        let mut s = editing_at_root(&paths());
        s.input.set("src/foo/file.txt");
        let h = s.alt_backspace(&paths());
        assert_eq!(s.input.text, "src/foo/");
        assert_eq!(h, TransitionHint::RefreshListing);
    }

    #[test]
    fn alt_backspace_on_trailing_slash_drops_parent_segment() {
        let mut s = editing_at_root(&paths());
        s.input.set("src/foo/");
        s.alt_backspace(&paths());
        assert_eq!(s.input.text, "src/");
    }

    #[test]
    fn alt_backspace_at_lone_slash_clears_to_empty() {
        let mut s = editing_at_root(&paths());
        s.input.set("src/");
        s.alt_backspace(&paths());
        assert_eq!(s.input.text, "");
    }

    #[test]
    fn alt_backspace_on_bare_word_clears() {
        let mut s = editing_at_root(&paths());
        s.input.set("src");
        s.alt_backspace(&paths());
        assert_eq!(s.input.text, "");
    }

    #[test]
    fn plain_backspace_at_empty_input_peels_into_selecting_root() {
        // The same leftward gesture the dir editor uses to step from the path back into the
        // root segment; single-root projects no-op (there's no root to pick).
        let multi = vec!["/a".into(), "/b".into()];
        let mut s = SavePromptState::open_for_scratch(&multi).0;
        let h = s.backspace(&multi);
        assert_eq!(h, TransitionHint::None);
        assert!(matches!(s.mode, PromptMode::SelectingRoot(_)));
        let mut single = editing_at_root(&paths());
        single.backspace(&paths());
        assert!(matches!(single.mode, PromptMode::Editing(_)));
    }

    #[test]
    fn selecting_root_cycle_wraps() {
        // Root cycling wraps at both ends, matching the dir editor's root typeahead.
        let multi = vec!["/a".into(), "/b".into(), "/c".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi); // → SelectingRoot, suggestion on from_root (= /a, idx 0)
        s.alt_k(&multi); // up from the first match wraps to the last
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some("c"));
        s.alt_j(&multi); // and back down wraps to the first
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some("a"));
    }

    #[test]
    fn selecting_root_completed_label_has_empty_ghost_suffix() {
        // The `:`-commits-a-completed-root binding keys off ghost_suffix == "" — typing the
        // full label leaves nothing for the ghost to add.
        let multi = vec!["/apple".into(), "/banana".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi);
        for c in "apple".chars() {
            s.type_char(c, &multi);
        }
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some(""));
    }

    #[test]
    fn alt_backspace_at_empty_input_peels_into_selecting_root_in_multi_root() {
        let multi = vec!["/a".into(), "/b".into()];
        let mut s = SavePromptState::open_for_scratch(&multi).0;
        let h = s.alt_backspace(&multi);
        assert_eq!(h, TransitionHint::None);
        assert!(matches!(s.mode, PromptMode::SelectingRoot(_)));
    }

    #[test]
    fn alt_backspace_at_empty_input_is_no_op_in_single_root() {
        let mut s = editing_at_root(&paths());
        s.alt_backspace(&paths());
        assert!(matches!(s.mode, PromptMode::Editing(_)));
    }

    // ---- SelectingRoot retained behaviour ----

    #[test]
    fn selecting_root_typing_filters_root_labels_and_shows_ghost() {
        let multi = vec!["/apple".into(), "/banana".into(), "/avocado".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi); // → SelectingRoot
        s.type_char('a', &multi);
        // Typing resets suggestion_idx to 0; ghost = suffix of first match "apple" beyond "a".
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some("pple"));
        s.alt_j(&multi);
        // Step forward through filtered matches; second match is "avocado".
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some("vocado"));
    }

    #[test]
    fn selecting_root_alt_backspace_clears_typed_filter() {
        let multi = vec!["/apple".into(), "/banana".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi);
        s.type_char('a', &multi);
        s.alt_backspace(&multi);
        assert_eq!(s.input.text, "");
        // Suggestion_idx falls back to from_root (the root we peeled from) — Tab now undoes the
        // peel.
        let PromptMode::SelectingRoot(sr) = &s.mode else {
            panic!();
        };
        assert_eq!(sr.suggestion_idx, sr.from_root as usize);
    }

    #[test]
    fn selecting_root_default_ghost_is_from_root_label() {
        let multi = vec!["/a".into(), "/b".into()];
        // Open as Editing at root 1, then peel.
        let mut s = SavePromptState::open_for_scratch(&multi).0;
        if let PromptMode::Editing(e) = &mut s.mode {
            e.path_index = 1;
        }
        s.alt_backspace(&multi);
        // Default ghost shows from_root's label ("b") in full since input is empty.
        assert_eq!(s.ghost_suffix(&multi).as_deref(), Some("b"));
    }

    #[test]
    fn selecting_root_tab_commits_to_editing_at_chosen_root() {
        let multi = vec!["/a".into(), "/b".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi);
        s.alt_j(&multi);
        let h = s.tab(&multi);
        assert_eq!(h, TransitionHint::RefreshListing);
        let PromptMode::Editing(e) = &s.mode else {
            panic!();
        };
        assert_eq!(e.path_index, 1);
        assert_eq!(e.listing_dir_abs, "/b");
    }

    #[test]
    fn selecting_root_tab_with_typed_filter_commits_first_match() {
        let multi = vec!["/apple".into(), "/banana".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi);
        s.type_char('b', &multi);
        s.tab(&multi);
        let PromptMode::Editing(e) = &s.mode else {
            panic!();
        };
        assert_eq!(e.path_index, 1);
    }

    // ---- save_target / enter_action ----

    #[test]
    fn save_target_returns_input_under_chosen_root() {
        let mut s = editing_at_root(&paths());
        s.input.set("src/app.rs");
        assert_eq!(s.save_target(), Some((0, "src/app.rs".into())));
    }

    #[test]
    fn save_target_is_none_in_selecting_root() {
        let multi = vec!["/a".into(), "/b".into()];
        let mut s = editing_at_root(&multi);
        s.alt_backspace(&multi);
        assert!(s.save_target().is_none());
    }

    #[test]
    fn enter_action_routes_correctly() {
        let multi = vec!["/a".into(), "/b".into()];
        let mut s = editing_at_root(&multi);
        assert_eq!(s.enter_action(), EnterAction::Nothing);
        s.input.set("file.rs");
        assert_eq!(s.enter_action(), EnterAction::Save);
        s.input.clear();
        s.alt_backspace(&multi); // → SelectingRoot
        assert_eq!(s.enter_action(), EnterAction::Tab);
    }

    #[test]
    fn enter_saves_literal_input_even_when_ghost_visible() {
        // The user can save a name that's a prefix of an existing file. Ghost is visible but
        // Enter ignores it — Tab is the only way to absorb the suggestion.
        let mut s = editing_at_root(&paths());
        s.set_listing(vec![entry("app.rs", false)]);
        s.type_char('a', &paths());
        assert_eq!(s.ghost_suffix(&paths()).as_deref(), Some("pp.rs"));
        assert_eq!(s.enter_action(), EnterAction::Save);
        assert_eq!(s.save_target(), Some((0, "a".into())));
    }

    // ---- pure helpers ----

    #[test]
    fn split_input_examples() {
        assert_eq!(split_input("src/foo/file.txt"), ("src/foo/", "file.txt"));
        assert_eq!(split_input("src/foo/"), ("src/foo/", ""));
        assert_eq!(split_input("src"), ("", "src"));
        assert_eq!(split_input(""), ("", ""));
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
}
