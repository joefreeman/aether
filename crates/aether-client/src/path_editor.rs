//! The path field every path-naming surface uses, with the same directory-completion UX as the
//! picker's dir-scope chip editor — so naming a path anywhere reuses the muscle memory of scoping a
//! search.
//!
//! Four users, split by two independent switches. [`PathBase`] says what the path is measured
//! against: **rooted** (save-as `Alt-s`, and the workspace-settings add-project row) or **absolute**
//! (the add-root row, and the open-from-path prompt `Space Alt-w`). `allow_files` says whether a
//! file may complete: yes for the two that name a file (save-as overwrites one, open-from-path
//! opens one), no for the two that name a directory (a project *is* its directory; so is a root).
//!
//! It mirrors [`crate::chips::ChipEditor`]'s dir half — a multi-root workspaces' leading root field
//! (inline smartcase typeahead, `:` separator) ahead of a `directory/list`-backed path field with
//! ghost suggestions, `Alt-l` accept (never `Tab` — that traverses fields, one key one meaning),
//! `Alt-j`/`k` cycle, and fish-style `Alt-Backspace`
//! segment pop — including its `allow_files` switch over what the listing keeps, and with one
//! deliberate departure:
//!
//! - Committing saves the **literal typed path** ([`PathEditor::save_target`]); a partially typed
//!   leaf is *not* silently snapped to the highlighted suggestion (that is what `Tab` is for), and
//!   a non-matching leaf never blocks the commit — when saving, you're naming a file that needn't
//!   exist yet. A missing *parent* directory still renders red ([`PathEditor::path_invalid`]) as an
//!   advisory.
//!
//! Suggesting what the surface would then reject is the failure both switches exist to prevent — a
//! file offered for a root, or a workspace-relative completion for a path that isn't relative to
//! one.
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

/// What the typed path is measured against — the editor's mode, and the thing every other
/// difference follows from.
///
/// It is a mode rather than, say, an `Option<u32> root_index`, because none of the differences are
/// about *which* root: they are which directory to list, which commit shape to produce, and whether
/// a root segment exists to focus at all. A `None` root index would say none of that, and would
/// leave `root_filter`/`root_selected` live and meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathBase {
    /// Root-relative: the path is under one of the workspace's roots, picked by the root segment in
    /// multi-root workspaces. Commits as `(path_index, relative_path)`. Save-as and add-project.
    Rooted,
    /// An absolute filesystem path, standing on its own — no root segment, ever. Commits as the
    /// literal typed string. Add-root and open-from-path, the two fields that name somewhere the
    /// workspace doesn't already reach.
    Absolute,
}

/// The path editor. In single-root workspaces (and always, in [`PathBase::Absolute`]) only the path
/// field exists — `field` is always `Path`; multi-root [`PathBase::Rooted`] adds the leading root
/// field.
#[derive(Debug)]
pub struct PathEditor {
    /// What the path is measured against. Decides [`PathEditor::multi_root`], and through it every
    /// root-segment behaviour in the shared key handler and all three shells.
    pub base: PathBase,
    /// Which segment has focus. Always `Path` in single-root workspaces and in `Absolute`.
    pub field: ChipEditorField,
    /// The path being typed (directory portion + leaf) — root-relative in `Rooted`, absolute in
    /// `Absolute`, where a leading `~` is kept as typed and expanded server-side.
    pub input: Input,
    /// What the field opened with — `~/` for the absolute ones, empty for the rest.
    ///
    /// Kept so [`PathEditor::is_untouched`] can tell a seed apart from something the user typed,
    /// which is what lets a seeded field still show its "Add root…" affordance. Compared against
    /// rather than latched: deleting back down to the seed makes the field untouched again, which
    /// is the honest answer — there is nothing invested in it either way.
    pub seed: String,
    /// Multi-root: the prefix filter typed into the root field.
    pub root_filter: Input,
    /// Multi-root: highlight within [`root_candidates`]' matches for the current filter.
    pub root_selected: usize,
    /// The root the editor opened with — the fallback when the filter matches nothing.
    pub root_index: u32,
    /// When true the editor may complete to a file, not just subdirectories — set for the save-as
    /// prompt (completing onto a file overwrites it), cleared for the add-project row (a project is
    /// a directory). Controls what [`PathEditor::set_dir_listing`] keeps.
    pub allow_files: bool,
    /// Cached `directory/list` entries for the dir portion of `input` — subdirectories only, unless
    /// `allow_files` (then files are kept too, so a file name can complete).
    pub listing: Vec<DirectoryEntry>,
    /// The absolute path `listing` was last synced against (the staleness key).
    pub listing_dir_abs: String,
    /// Where `listing` stands relative to `listing_dir_abs`.
    pub listing_state: DirListingState,
    /// Position within the filtered match set producing the current path ghost.
    pub suggestion_idx: usize,
}

impl PathEditor {
    /// Open the editor pre-filled with `path` under root `root_index`. `field` is the initially
    /// focused segment (callers focus the root field for a brand-new buffer in a multi-root
    /// workspace, the path field otherwise). `allow_files` decides whether the path field may
    /// complete to a file. `listing_dir_abs` starts empty so the caller's first
    /// [`PathEditor::sync_dir_listing`] always reports a refetch is due.
    pub fn new(path: String, field: ChipEditorField, root_index: u32, allow_files: bool) -> Self {
        PathEditor {
            base: PathBase::Rooted,
            field,
            seed: path.clone(),
            input: Input::new(path),
            root_filter: Input::default(),
            // Empty filter → candidates are all roots in order, so the opening root's index
            // doubles as its position among them.
            root_selected: root_index as usize,
            root_index,
            allow_files,
            listing: Vec::new(),
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Pending,
            suggestion_idx: 0,
        }
    }

    /// Open an editor over an **absolute** filesystem path ([`PathBase::Absolute`]).
    ///
    /// A separate constructor rather than a fifth argument to [`PathEditor::new`], because `new`'s
    /// `field` and `root_index` are both meaningless here — and threading them through would invite
    /// a caller to pass `ChipEditorField::Root`, focusing a segment this editor does not have.
    pub fn absolute(path: String, allow_files: bool) -> Self {
        PathEditor {
            base: PathBase::Absolute,
            field: ChipEditorField::Path,
            seed: path.clone(),
            input: Input::new(path),
            root_filter: Input::default(),
            root_selected: 0,
            root_index: 0,
            allow_files,
            listing: Vec::new(),
            listing_dir_abs: String::new(),
            listing_state: DirListingState::Pending,
            suggestion_idx: 0,
        }
    }

    /// Whether this editor has a leading root segment — the question every root-related behaviour
    /// actually turns on.
    ///
    /// It lives here rather than at the call sites because it used to be re-derived, as
    /// `workspace_paths.len() > 1`, in the shared key handler, the web view projection and both
    /// native shells. An absolute editor in a multi-root workspace would have answered `true` at
    /// every one of them, and `path_editor_key`'s `BackTab` / `Alt-h` / `Alt-Backspace`-at-empty
    /// arms would then have moved focus into a root segment that does not exist.
    pub fn multi_root(&self, workspace_paths: &[String]) -> bool {
        self.base == PathBase::Rooted && workspace_paths.len() > 1
    }

    /// Nothing has been invested in this field: it still holds exactly what it opened with.
    ///
    /// For an unseeded editor that is simply "empty"; for a seeded one it also covers the bare `~/`
    /// it opens with, which is what lets a seeded row keep showing its "Add root…" affordance while
    /// unfocused. A field the user has typed into and then deleted back down to the seed counts as
    /// untouched again — there is nothing in it either way, so nothing to preserve.
    pub fn is_untouched(&self) -> bool {
        self.input.text == self.seed
    }

    // ---- root field (Rooted, multi-root only) --------------------------------------------------

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
    pub fn commit_root_field(&mut self, labels: &[String], workspace_paths: &[String]) -> bool {
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
        self.sync_dir_listing(workspace_paths)
    }

    /// Move to the path segment *without* adopting the root ghost — `Tab` traversal, as opposed to
    /// `Alt-l`'s accept-and-advance ([`Self::commit_root_field`]).
    ///
    /// The typed filter stands as it is: [`chosen_root`](crate::chips::ChipEditor::chosen_root)
    /// already resolves a partial-but-matching prefix to a real root, and the unfocused segment
    /// renders that root's settled label — so nothing is lost by not rewriting the text. Returns
    /// whether the directory listing needs refetching.
    pub fn advance_to_path(&mut self, workspace_paths: &[String]) -> bool {
        self.field = ChipEditorField::Path;
        self.sync_dir_listing(workspace_paths)
    }

    // ---- directory listing ---------------------------------------------------------------------

    /// The absolute directory the path field's suggestions should list.
    ///
    /// `Rooted`: the dir portion of the typed path, resolved under the chosen root. `None` under an
    /// *invalid* root — suggestions beneath the fallback root would read as silently defaulting to
    /// it.
    ///
    /// `Absolute`: the dir portion **verbatim**, tilde and all (the server expands it). Not through
    /// `join_root_relative`, which trims the trailing separator and would turn the filesystem root
    /// `"/"` into `""`. `None` for an empty dir portion — there is nothing to list yet, and asking
    /// for `""` would come back as a canonicalize failure and paint the field red before the user
    /// has typed anything.
    pub fn dir_listing_path(&self, workspace_paths: &[String]) -> Option<String> {
        let dir = dir_of_input(&self.input.text);
        if self.base == PathBase::Absolute {
            return (!dir.is_empty()).then(|| dir.to_string());
        }
        let root = if workspace_paths.len() > 1 {
            let labels = root_labels(workspace_paths);
            if self.root_invalid(&labels) {
                return None;
            }
            self.chosen_root(&labels)
        } else {
            0
        };
        Some(join_root_relative(workspace_paths, root, dir))
    }

    /// Store a `directory/list` response. Without `allow_files`, keep only subdirectories — a file
    /// can't be a project. With it, keep files too, so a file name can complete a save path
    /// (you're overwriting it).
    pub fn set_dir_listing(&mut self, entries: Vec<DirectoryEntry>) {
        self.listing = if self.allow_files {
            entries
        } else {
            entries.into_iter().filter(|e| e.is_dir).collect()
        };
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
    /// for [`PathEditor::dir_listing_path`].
    pub fn sync_dir_listing(&mut self, workspace_paths: &[String]) -> bool {
        let Some(abs) = self.dir_listing_path(workspace_paths) else {
            // Absolute: no dir portion, so there is nothing to list *and* nothing the old listing
            // still describes. Dropping it matters — an early return here would leave a field that
            // has been emptied after a bad path stuck `Failed` (red forever, since `path_invalid`
            // reads the state alone), and would keep ghosting the previous directory's entries onto
            // whatever unrelated leaf is typed next.
            //
            // The `Rooted` invalid-root case takes the early return it always has. It is arguably
            // wrong there too, for the second of those reasons, but that is a live behaviour change
            // for save-as and add-project and belongs in its own commit.
            if self.base == PathBase::Absolute {
                self.listing_dir_abs.clear();
                self.listing.clear();
                self.listing_state = DirListingState::Pending;
                self.suggestion_idx = 0;
            }
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
    pub fn accept_path_suggestion(&mut self, workspace_paths: &[String]) -> bool {
        let Some(suffix) = self.path_ghost() else {
            return false;
        };
        self.input.push_str(&suffix);
        self.suggestion_idx = 0;
        self.sync_dir_listing(workspace_paths)
    }

    /// Alt-Backspace in a non-empty path field: drop the rightmost segment, fish-style. Returns
    /// `true` when the dir portion shrank and a refetch is due.
    pub fn pop_path_segment(&mut self, workspace_paths: &[String]) -> bool {
        let popped = pop_segment(&self.input.text);
        self.input.set(popped);
        self.suggestion_idx = 0;
        self.sync_dir_listing(workspace_paths)
    }

    /// Bookkeeping after a free-form edit to the path field: reset the suggestion highlight and
    /// report whether the dir portion moved.
    pub fn path_edited(&mut self, workspace_paths: &[String]) -> bool {
        self.suggestion_idx = 0;
        self.sync_dir_listing(workspace_paths)
    }

    /// True when the path is *definitely* unsaveable as typed — the red-worthy condition: the dir
    /// portion failed to list (its parent directory doesn't exist or sits outside the workspace
    /// boundary). The filename leaf is free, so it never invalidates; a `Pending` listing is
    /// unknown, not invalid.
    pub fn path_invalid(&self) -> bool {
        matches!(self.listing_state, DirListingState::Failed)
    }

    /// The `(path_index, relative_path)` a commit should save to — the literal typed path under
    /// the chosen root. `None` for an empty path (nothing to save to), and `None` in
    /// [`PathBase::Absolute`], where there is no root to be relative to.
    ///
    /// That second `None` is the point of the pair: an absolute editor reaching this would
    /// otherwise hand back `(0, "/home/me/code")` and the caller would post an absolute path as a
    /// *relative* one. Each commit site calls exactly one of this and
    /// [`PathEditor::absolute_target`], and gets `None` if it picked the wrong one.
    ///
    /// Absolute paths *typed into a rooted editor* (a leading `/` in save-as) are a different
    /// thing, and are re-resolved against the roots by the caller.
    pub fn save_target(&self, workspace_paths: &[String]) -> Option<(u32, String)> {
        if self.base == PathBase::Absolute {
            return None;
        }
        let path = self.input.text.trim().to_string();
        if path.is_empty() {
            return None;
        }
        let path_index = if workspace_paths.len() > 1 {
            self.chosen_root(&root_labels(workspace_paths))
        } else {
            0
        };
        Some((path_index, path))
    }

    /// The literal absolute path a commit should use — the counterpart to
    /// [`PathEditor::save_target`], and `None` in [`PathBase::Rooted`] for the same reason.
    ///
    /// Returned verbatim, `~` included: the server expands it (the client core compiles to wasm,
    /// where there is no `$HOME`), and both `workspace/add_root` and `workspace/open_path` already
    /// do so on the way in.
    pub fn absolute_target(&self) -> Option<String> {
        if self.base == PathBase::Rooted {
            return None;
        }
        let path = self.input.text.trim().to_string();
        (!path.is_empty()).then_some(path)
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
        let mut ed = PathEditor::new(String::new(), ChipEditorField::Path, 0, true);
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

    /// Without `allow_files` the listing keeps directories only, so a file can never be ghosted,
    /// cycled to, or accepted — the add-project row's whole reason for the flag. A project is its
    /// directory, and the server refuses anything else, so offering a file could only ever bait a
    /// rejection.
    #[test]
    fn without_allow_files_only_directories_complete() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = PathEditor::new(String::new(), ChipEditorField::Path, 0, false);
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing(vec![
            entry("src", true),
            entry("Cargo.toml", false),
            entry("scripts", true),
        ]);
        assert_eq!(
            ed.listing
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "scripts"],
            "files are dropped on the way in, so nothing downstream has to re-check"
        );

        // The manifest is exactly what you'd reach for under the old marker-file model, and it is
        // precisely what must not complete now.
        ed.input.set("Car".into());
        assert_eq!(ed.path_ghost(), None);
        assert!(!ed.accept_path_suggestion(&roots));
        assert_eq!(ed.input.text, "Car", "a non-matching leaf is left alone");

        // Directories still complete, and still carry the trailing `/`.
        ed.input.set("scr".into());
        assert_eq!(ed.path_ghost().as_deref(), Some("ipts/"));

        // The same listing under the save-as prompt keeps the file — the flag is the only
        // difference between the two users.
        let mut save_as = PathEditor::new(String::new(), ChipEditorField::Path, 0, true);
        save_as.sync_dir_listing(&roots);
        save_as.set_dir_listing(vec![entry("src", true), entry("Cargo.toml", false)]);
        save_as.input.set("Car".into());
        assert_eq!(save_as.path_ghost().as_deref(), Some("go.toml"));
    }

    // ---- absolute mode -------------------------------------------------------------------------

    /// What an absolute editor asks the server to list, across the shapes a half-typed path passes
    /// through. The dir portion goes out **verbatim** — no root join, tilde intact.
    #[test]
    fn absolute_lists_the_dir_portion_verbatim() {
        // Deliberately multi-root: an absolute editor must ignore the roots entirely.
        let roots = vec!["/work/api".to_string(), "/personal/web".to_string()];
        let dir_for = |typed: &str| {
            let ed = PathEditor::absolute(typed.into(), true);
            ed.dir_listing_path(&roots)
        };

        assert_eq!(dir_for("/hom").as_deref(), Some("/"));
        assert_eq!(dir_for("/etc/ngin").as_deref(), Some("/etc/"));
        // The filesystem root survives: `join_root_relative` would have trimmed this to `""`.
        assert_eq!(dir_for("/").as_deref(), Some("/"));
        // The tilde is the server's to expand — the client core has no `$HOME` under wasm.
        assert_eq!(dir_for("~/Pro").as_deref(), Some("~/"));
        assert_eq!(dir_for("~/").as_deref(), Some("~/"));
        // Nothing typed yet: nothing to list. Asking for `""` would come back a canonicalize
        // failure and paint the field red before the user has done anything.
        assert_eq!(dir_for(""), None);
        assert_eq!(dir_for("hom"), None);
    }

    /// An absolute editor never has a root segment, however many roots the workspace has — the
    /// property that keeps `path_editor_key`'s BackTab / Alt-h / Alt-Backspace arms from moving
    /// focus somewhere that doesn't exist.
    #[test]
    fn absolute_never_has_a_root_segment() {
        let one = vec!["/work/api".to_string()];
        let many = vec!["/work/api".to_string(), "/personal/web".to_string()];

        let abs = PathEditor::absolute("/etc".into(), true);
        assert!(!abs.multi_root(&one));
        assert!(!abs.multi_root(&many));
        assert_eq!(abs.field, ChipEditorField::Path);

        // A rooted editor still answers the original question.
        let rooted = PathEditor::new(String::new(), ChipEditorField::Path, 0, true);
        assert!(!rooted.multi_root(&one));
        assert!(rooted.multi_root(&many));
    }

    /// The two commit accessors are exclusive, so a commit site that reaches for the wrong one gets
    /// `None` rather than a plausible-looking wrong answer — an absolute path posted as a relative
    /// one, in particular.
    #[test]
    fn commit_accessors_are_exclusive() {
        let roots = vec!["/work/api".to_string(), "/personal/web".to_string()];

        let abs = PathEditor::absolute("/home/me/code".into(), false);
        assert_eq!(abs.absolute_target().as_deref(), Some("/home/me/code"));
        assert_eq!(abs.save_target(&roots), None);

        let rooted = PathEditor::new("notes.md".into(), ChipEditorField::Path, 1, true);
        assert_eq!(rooted.save_target(&roots), Some((1, "notes.md".into())));
        assert_eq!(rooted.absolute_target(), None);

        // Empty is nothing to commit either way.
        assert_eq!(
            PathEditor::absolute("   ".into(), false).absolute_target(),
            None
        );
    }

    /// Emptying an absolute field after a bad path clears the failure with it.
    ///
    /// Without the reset in `sync_dir_listing`'s `None` branch, both halves of this go wrong: the
    /// field stays red forever (`path_invalid` reads the listing state alone), and the dead listing
    /// keeps ghosting its entries onto whatever is typed next.
    #[test]
    fn emptying_an_absolute_field_clears_the_stale_listing() {
        let roots = vec!["/work/api".to_string()];
        let mut ed = PathEditor::absolute(String::new(), true);

        ed.input.set("/etc/ho".into());
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing(vec![entry("hosts", false), entry("nginx", true)]);
        assert_eq!(ed.path_ghost().as_deref(), Some("sts"));

        // Back to a bare leaf with no dir portion: the `/etc` entries must not follow it.
        ed.input.set("ho".into());
        assert!(!ed.path_edited(&roots));
        assert!(ed.listing.is_empty());
        assert_eq!(ed.path_ghost(), None);

        // And a failure doesn't outlive the text that caused it.
        ed.input.set("/nope/".into());
        ed.sync_dir_listing(&roots);
        ed.set_dir_listing_failed();
        assert!(ed.path_invalid());
        ed.input.set(String::new());
        assert!(!ed.path_edited(&roots));
        assert!(
            !ed.path_invalid(),
            "an emptied field is unknown, not invalid"
        );
    }

    #[test]
    fn accepting_a_dir_refetches_but_a_file_does_not() {
        let roots = vec!["/tmp/root".to_string()];
        let mut ed = PathEditor::new(String::new(), ChipEditorField::Path, 0, true);
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
        let mut ed = PathEditor::new(String::new(), ChipEditorField::Path, 0, true);
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
        let mut ed = PathEditor::new("nope/file.rs".into(), ChipEditorField::Path, 0, true);
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
        let mut ed = PathEditor::new(String::new(), ChipEditorField::Root, 0, true);
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
