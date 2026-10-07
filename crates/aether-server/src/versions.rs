//! Stepping through a file's versions in history — `git/step_version`.
//!
//! The stops are the commits that changed the file (a **file** step) or the cursor line (a **line**
//! step) — `git log -p -- <file>` and `git log -L` respectively — and a stop is shown with that
//! commit's own change against its parent. Older is the nearest stop strictly older than the
//! version on screen: from a version that is itself a stop, the change before it; from one that
//! isn't (your working file, or a commit that left the line alone), the change that set what you
//! are looking at. Newer is the mirror image, ending at the working file.
//!
//! **First-parent, both directions.** Older steps follow first parents; newer steps walk HEAD's
//! first-parent chain back to the version in front of you and then forwards. A merged side branch
//! therefore arrives as one step at its merge commit. That is the price of the two directions
//! being inverses: a newer step has to choose *a* path to HEAD, and an older step that chose a
//! different one (as blame does) would leave `older` then `newer` somewhere other than where you
//! started.
//!
//! **One line mapping.** Carrying a line across a change uses [`align`] over the changed hunk,
//! read forwards for a newer step and backwards for an older one. Reading one alignment both ways
//! is what makes a line step undoable by its opposite; two heuristics, one per direction, would
//! disagree on exactly the edits worth stepping through.
//!
//! The working tree is the newest version: a pseudo-commit whose first parent is HEAD and whose
//! content is the live buffer. It is never an older step's destination — its own change is the
//! live file's inline diff. Paths are followed literally — a rename ends the history, as it does
//! for the file-history picker.

use std::path::Path;

use aether_protocol::git::{VersionLabel, VersionNote};

use crate::git::{hunks_from_buffers, normalize_lf, DiffHunk};

/// Commits examined before a walk gives up. A newer step has to find the version you are on in
/// HEAD's first-parent chain, and a version off that chain would otherwise walk the whole history
/// to say so.
const MAX_WALK: usize = 50_000;

/// Where a step starts.
pub struct Origin<'a> {
    /// The revision being shown, or `None` for the working tree.
    pub rev: Option<&'a str>,
    /// Repo-relative path.
    pub path: &'a str,
    /// The cursor line, 0-based.
    pub line: u32,
    /// The working tree's content: the live buffer's text when a buffer holds the file, else
    /// what is on disk; `None` when the file is gone from the working tree. Read for the working
    /// tree origin, and for a newer step that runs off the end of the commits.
    pub worktree: Option<&'a str>,
}

/// Which way, and how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Older,
    Newer,
}

/// Where a step lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    /// The file as of this commit (a full hash), at this 0-based line.
    Revision {
        rev: String,
        path: String,
        line: u32,
    },
    /// The working-tree file itself.
    WorkingFile { path: String, line: u32 },
}

/// A step's answer: somewhere to go, something to say, or both.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stepped {
    pub landing: Option<Landing>,
    pub note: Option<VersionNote>,
}

impl Stepped {
    fn to(landing: Landing) -> Self {
        Self {
            landing: Some(landing),
            note: None,
        }
    }

    fn note(note: VersionNote) -> Self {
        Self {
            landing: None,
            note: Some(note),
        }
    }
}

/// Step one version older or newer, by file or by line.
pub fn step(workdir: &Path, origin: &Origin<'_>, line_scope: bool, step: Step) -> Stepped {
    let Ok(repo) = git2::Repository::discover(workdir) else {
        return Stepped::note(VersionNote::Untracked);
    };
    let history = History {
        repo: &repo,
        path: origin.path,
    };
    let Some(rev) = origin.rev else {
        return match (step, origin.worktree) {
            (Step::Older, Some(worktree)) => {
                history.older_than_worktree(worktree, origin.line, line_scope)
            }
            (Step::Older, None) => Stepped::note(VersionNote::Untracked),
            // The working tree is the end of the line.
            (Step::Newer, _) => Stepped::note(VersionNote::Newest),
        };
    };
    let Ok(at) = repo.revparse_single(rev).and_then(|o| o.peel_to_commit()) else {
        return Stepped::note(VersionNote::Untracked);
    };
    match step {
        Step::Older if line_scope => history.line_older(&at, origin.line),
        Step::Older => history.file_older(&at, origin.line),
        Step::Newer => history.newer(&at, origin.line, line_scope, origin.worktree),
    }
}

struct History<'r> {
    repo: &'r git2::Repository,
    path: &'r str,
}

/// The commit that last set a line's content, walking back from a version.
struct Change<'r> {
    /// The commit the line's current content came from.
    commit: git2::Commit<'r>,
    /// The line, in `commit`.
    line: u32,
    /// What the line was before `commit`.
    before: Before<'r>,
}

enum Before<'r> {
    /// It existed, here: the first parent, and the line in it.
    Line(git2::Commit<'r>, u32),
    /// `commit` added it, or created the file: there is no before.
    Nothing,
}

impl<'r> History<'r> {
    /// The blob this path names in `commit`, or `None` where the path isn't a file there.
    fn blob(&self, commit: &git2::Commit<'_>) -> Option<git2::Oid> {
        let entry = commit.tree().ok()?.get_path(Path::new(self.path)).ok()?;
        (entry.kind() == Some(git2::ObjectType::Blob)).then(|| entry.id())
    }

    /// A blob's text, normalised as buffers are on load so a CRLF file doesn't diff as wholly
    /// changed against its own buffer.
    fn text(&self, blob: git2::Oid) -> String {
        let bytes = self
            .repo
            .find_blob(blob)
            .map(|b| b.content().to_vec())
            .unwrap_or_default();
        String::from_utf8_lossy(&normalize_lf(bytes)).into_owned()
    }

    fn label(&self, commit: &git2::Commit<'_>) -> VersionLabel {
        VersionLabel {
            short_hash: commit.id().to_string().chars().take(7).collect(),
            subject: commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string(),
        }
    }

    /// The commit that introduced the file content `commit` holds: walk first parents while the
    /// blob stays the same. Where every stop lands, so the label names the change on screen.
    fn introducer(&self, commit: git2::Commit<'r>) -> git2::Commit<'r> {
        let blob = self.blob(&commit);
        let mut at = commit;
        for _ in 0..MAX_WALK {
            match at.parent(0) {
                Ok(p) if self.blob(&p) == blob => at = p,
                _ => break,
            }
        }
        at
    }

    /// HEAD's first-parent chain down to `version`, oldest first and excluding `version` itself.
    /// `None` when `version` isn't on it — off the branch, or past the walk cap — which is what
    /// makes "newer" undefined.
    fn chain_above(&self, version: git2::Oid) -> Option<Vec<git2::Commit<'r>>> {
        let mut at = self.repo.head().ok()?.peel_to_commit().ok()?;
        let mut chain = Vec::new();
        for _ in 0..MAX_WALK {
            if at.id() == version {
                chain.reverse();
                return Some(chain);
            }
            let parent = at.parent(0).ok()?;
            chain.push(at);
            at = parent;
        }
        None
    }

    /// Walk back from `line` in `version` to the commit that last changed it.
    fn last_change(&self, version: git2::Commit<'r>, line: u32) -> Change<'r> {
        let (mut at, mut line) = (version, line);
        for _ in 0..MAX_WALK {
            let blob = self.blob(&at);
            let parent = match at.parent(0) {
                Ok(p) => p,
                Err(_) => break,
            };
            let Some(parent_blob) = self.blob(&parent) else {
                break;
            };
            if Some(parent_blob) == blob {
                at = parent;
                continue;
            }
            let (old, new) = (
                self.text(parent_blob),
                blob.map(|b| self.text(b)).unwrap_or_default(),
            );
            match map_to_old(&old, &new, line) {
                Mapped::Same(l) => {
                    at = parent;
                    line = l;
                }
                Mapped::Changed(l) => {
                    return Change {
                        commit: at,
                        line,
                        before: Before::Line(parent, l),
                    }
                }
                Mapped::Gone(_) => {
                    return Change {
                        commit: at,
                        line,
                        before: Before::Nothing,
                    }
                }
            }
        }
        Change {
            commit: at,
            line,
            before: Before::Nothing,
        }
    }

    /// The nearest older stop for the line: the change that set it, unless that change is the
    /// version on screen — then the change before it.
    fn line_older(&self, version: &git2::Commit<'r>, line: u32) -> Stepped {
        let change = self.last_change(version.clone(), line);
        if change.commit.id() != version.id() {
            return Stepped::to(self.revision(&change.commit, change.line));
        }
        match change.before {
            Before::Line(parent, line) => {
                let previous = self.last_change(parent, line);
                Stepped::to(self.revision(&previous.commit, previous.line))
            }
            Before::Nothing => Stepped::note(VersionNote::LineAdded {
                at: Some(self.label(&change.commit)),
            }),
        }
    }

    /// Whether `commit` changed the file — the stops of a file step. A root commit that has the
    /// file created it.
    fn touches(&self, commit: &git2::Commit<'_>) -> bool {
        let blob = self.blob(commit);
        blob.is_some() && commit.parent(0).ok().and_then(|p| self.blob(&p)) != blob
    }

    /// The nearest older stop for the file: the commit that wrote the content on screen, unless
    /// that is the version on screen — then the change before it.
    fn file_older(&self, version: &git2::Commit<'r>, line: u32) -> Stepped {
        if !self.touches(version) {
            // Same content all the way down to its introducer, so the line needs no mapping.
            return Stepped::to(self.revision(&self.introducer(version.clone()), line));
        }
        let parent = version.parent(0).ok();
        let Some((parent, parent_blob)) = parent.and_then(|p| self.blob(&p).map(|b| (p, b))) else {
            return Stepped::note(VersionNote::Oldest {
                at: self.label(version),
            });
        };
        let (old, new) = (
            self.text(parent_blob),
            self.blob(version).map(|b| self.text(b)).unwrap_or_default(),
        );
        let line = map_to_old(&old, &new, line).line();
        Stepped::to(self.revision(&self.introducer(parent), line))
    }

    /// Older than the working tree: the working file's own change is its inline diff, so the
    /// first stop is always in HEAD's history — the commit that wrote HEAD's version of the file,
    /// or of the line.
    fn older_than_worktree(&self, worktree: &str, line: u32, line_scope: bool) -> Stepped {
        let Some(head) = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok()) else {
            return Stepped::note(VersionNote::Untracked);
        };
        let Some(head_blob) = self.blob(&head) else {
            return Stepped::note(VersionNote::Untracked);
        };
        let mapped = map_to_old(&self.text(head_blob), worktree, line);
        if !line_scope {
            return Stepped::to(self.revision(&self.introducer(head), mapped.line()));
        }
        match mapped {
            // Written in the working tree: nothing committed is a version of it.
            Mapped::Gone(_) => Stepped::note(VersionNote::LineAdded { at: None }),
            Mapped::Same(l) | Mapped::Changed(l) => {
                let change = self.last_change(head, l);
                Stepped::to(self.revision(&change.commit, change.line))
            }
        }
    }

    /// One version newer: up HEAD's first-parent chain to the next change, then the working tree.
    fn newer(
        &self,
        version: &git2::Commit<'r>,
        line: u32,
        line_scope: bool,
        worktree: Option<&str>,
    ) -> Stepped {
        let Some(chain) = self.chain_above(version.id()) else {
            return Stepped::note(VersionNote::Unreachable);
        };
        let (mut blob, mut line) = (self.blob(version), line);
        for commit in chain {
            let Some(next) = self.blob(&commit) else {
                return Stepped::note(VersionNote::FileRemoved {
                    at: Some(self.label(&commit)),
                });
            };
            if Some(next) == blob {
                continue;
            }
            let (old, new) = (
                blob.map(|b| self.text(b)).unwrap_or_default(),
                self.text(next),
            );
            match map_to_new(&old, &new, line) {
                Mapped::Same(l) if line_scope => line = l,
                Mapped::Same(l) | Mapped::Changed(l) => {
                    return Stepped::to(self.revision(&commit, l));
                }
                Mapped::Gone(l) if line_scope => {
                    return Stepped {
                        landing: Some(self.revision(&commit, l)),
                        note: Some(VersionNote::LineRemoved {
                            at: Some(self.label(&commit)),
                        }),
                    };
                }
                Mapped::Gone(l) => return Stepped::to(self.revision(&commit, l)),
            }
            blob = Some(next);
        }
        // Past HEAD: the working tree, the last stop either way.
        let Some(worktree) = worktree else {
            return Stepped::note(VersionNote::FileRemoved { at: None });
        };
        let committed = blob.map(|b| self.text(b)).unwrap_or_default();
        let working = |line| Landing::WorkingFile {
            path: self.path.to_string(),
            line,
        };
        match map_to_new(&committed, worktree, line) {
            Mapped::Gone(l) if line_scope => Stepped {
                landing: Some(working(l)),
                note: Some(VersionNote::LineRemoved { at: None }),
            },
            mapped => Stepped::to(working(mapped.line())),
        }
    }

    fn revision(&self, commit: &git2::Commit<'_>, line: u32) -> Landing {
        Landing::Revision {
            rev: commit.id().to_string(),
            path: self.path.to_string(),
            line,
        }
    }
}

/// Where a line went across one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapped {
    /// Outside every hunk: the same text, perhaps on a different line.
    Same(u32),
    /// Inside a hunk, paired with this line on the other side.
    Changed(u32),
    /// Inside a hunk with no counterpart — deleted going forwards, added going back. The line is
    /// where the hunk sits on the other side.
    Gone(u32),
}

impl Mapped {
    pub fn line(self) -> u32 {
        match self {
            Self::Same(l) | Self::Changed(l) | Self::Gone(l) => l,
        }
    }
}

/// Carry an `old` line into `new`.
pub fn map_to_new(old: &str, new: &str, line: u32) -> Mapped {
    let new_lines: Vec<&str> = new.lines().collect();
    let mut shift: i64 = 0; // new minus old, over the hunks above `line`
    for h in hunks_from_buffers(old.as_bytes(), new.as_bytes()) {
        let removed = h.deleted.len() as u32;
        if line < h.old_start {
            break;
        }
        if line < h.old_start + removed {
            let k = (line - h.old_start) as usize;
            return match pairs_of(&h, &new_lines).iter().find(|(i, _)| *i == k) {
                Some((_, j)) => Mapped::Changed(h.anchor_line + *j as u32),
                None => Mapped::Gone(h.anchor_line),
            };
        }
        shift += h.new_lines as i64 - removed as i64;
    }
    Mapped::Same((line as i64 + shift).max(0) as u32)
}

/// Carry a `new` line back into `old` — [`map_to_new`] read the other way.
pub fn map_to_old(old: &str, new: &str, line: u32) -> Mapped {
    let new_lines: Vec<&str> = new.lines().collect();
    let mut shift: i64 = 0; // new minus old, over the hunks above `line`
    for h in hunks_from_buffers(old.as_bytes(), new.as_bytes()) {
        if line < h.anchor_line {
            break;
        }
        if line < h.anchor_line + h.new_lines {
            let k = (line - h.anchor_line) as usize;
            return match pairs_of(&h, &new_lines).iter().find(|(_, j)| *j == k) {
                Some((i, _)) => Mapped::Changed(h.old_start + *i as u32),
                None => Mapped::Gone(h.old_start),
            };
        }
        shift += h.new_lines as i64 - h.deleted.len() as i64;
    }
    Mapped::Same((line as i64 - shift).max(0) as u32)
}

/// The aligned pairs of one hunk, as (index into its removed lines, index into its added lines).
fn pairs_of(hunk: &DiffHunk, new_lines: &[&str]) -> Vec<(usize, usize)> {
    let start = hunk.anchor_line as usize;
    let end = (start + hunk.new_lines as usize).min(new_lines.len());
    let added = new_lines.get(start..end).unwrap_or_default();
    let removed: Vec<&str> = hunk.deleted.iter().map(String::as_str).collect();
    align(&removed, added)
}

/// Similarity a pair needs before it counts as one line edited rather than one removed and
/// another added.
const PAIR_THRESHOLD: f64 = 0.4;
/// Hunks bigger than this (removed × added) pair by position instead of by the full alignment.
const ALIGN_MAX_CELLS: usize = 40_000;
/// Characters of a line the similarity reads. A minified line is no more one line for being long.
const SIMILARITY_MAX_CHARS: usize = 512;
/// How strongly a tie prefers the pair at the same relative position in the hunk.
const POSITION_WEIGHT: f64 = 0.01;

/// Pair the removed and added lines of one hunk: which added line each removed one became.
///
/// An order-preserving matching that maximises total similarity (char-bigram Dice over trimmed
/// text), counting only pairs at or above [`PAIR_THRESHOLD`]; ties go to the pair nearer the same
/// relative position. Lines left unpaired were deleted or added outright. Deterministic, and read
/// in both directions — which is what makes an older line step and a newer one inverses.
pub fn align(removed: &[&str], added: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (removed.len(), added.len());
    if n == 0 || m == 0 {
        return Vec::new();
    }
    let old: Vec<Vec<u64>> = removed.iter().map(|l| bigrams(l)).collect();
    let new: Vec<Vec<u64>> = added.iter().map(|l| bigrams(l)).collect();
    let similarity = |i: usize, j: usize| dice(&old[i], &new[j], removed[i], added[j]);

    if n * m > ALIGN_MAX_CELLS {
        return (0..n.min(m))
            .filter(|&k| similarity(k, k) >= PAIR_THRESHOLD)
            .map(|k| (k, k))
            .collect();
    }

    // score[i][j]: the best total over removed[..i] × added[..j].
    let mut score = vec![vec![0.0f64; m + 1]; n + 1];
    let mut paired = vec![vec![false; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            let skip = score[i - 1][j].max(score[i][j - 1]);
            let s = similarity(i - 1, j - 1);
            let mut best = skip;
            if s >= PAIR_THRESHOLD {
                let drift = ((i - 1) as f64 / n as f64 - (j - 1) as f64 / m as f64).abs();
                let diagonal = score[i - 1][j - 1] + s - POSITION_WEIGHT * drift;
                if diagonal > skip {
                    best = diagonal;
                    paired[i][j] = true;
                }
            }
            score[i][j] = best;
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if paired[i][j] {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if score[i - 1][j] >= score[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}

/// A line's character bigrams, trimmed and sorted for a merge-count.
fn bigrams(line: &str) -> Vec<u64> {
    let chars: Vec<char> = line.trim().chars().take(SIMILARITY_MAX_CHARS).collect();
    let mut out: Vec<u64> = chars
        .windows(2)
        .map(|w| ((w[0] as u64) << 32) | w[1] as u64)
        .collect();
    out.sort_unstable();
    out
}

/// Dice coefficient over two sorted bigram multisets. Lines too short to have a bigram are similar
/// only when equal.
fn dice(a: &[u64], b: &[u64], a_text: &str, b_text: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return if a_text.trim() == b_text.trim() {
            1.0
        } else {
            0.0
        };
    }
    let (mut x, mut y, mut common) = (0, 0, 0usize);
    while x < a.len() && y < b.len() {
        match a[x].cmp(&b[y]) {
            std::cmp::Ordering::Less => x += 1,
            std::cmp::Ordering::Greater => y += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                x += 1;
                y += 1;
            }
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- alignment ------------------------------------------------------------------------------

    #[test]
    fn an_edited_line_pairs_with_its_edit() {
        let pairs = align(
            &["fn alpha(a: u32) {}", "fn beta() {}"],
            &["fn beta() -> u32 {}"],
        );
        assert_eq!(
            pairs,
            vec![(1, 0)],
            "the edit is beta's, not the line above's"
        );
    }

    #[test]
    fn unrelated_lines_stay_unpaired() {
        assert_eq!(align(&["let total = 0;"], &["}"]), vec![]);
        assert_eq!(align(&[""], &["let x = 1;"]), vec![]);
    }

    #[test]
    fn identical_candidates_pair_by_position() {
        // Every pair is equally similar, so the same relative position wins each tie.
        let pairs = align(&["x = 1", "x = 1"], &["x = 2", "x = 2"]);
        assert_eq!(pairs, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn alignment_preserves_order() {
        // Each line matches only its counterpart across: the two pairs cross, so one has to go.
        let pairs = align(
            &["total = compute(a)", "beta"],
            &["beta", "total = compute(a, b)"],
        );
        assert_eq!(
            pairs,
            vec![(1, 0)],
            "the exact match wins; the crossing one can't also pair"
        );
    }

    // ---- line mapping ---------------------------------------------------------------------------

    const OLD: &str = "one\ntwo\nlet value = compute(a);\nfour\n";
    const NEW: &str = "zero\none\ntwo\nlet value = compute(a, b);\nadded\nfour\n";

    #[test]
    fn mapping_carries_unchanged_lines_past_insertions() {
        assert_eq!(map_to_new(OLD, NEW, 0), Mapped::Same(1));
        assert_eq!(map_to_new(OLD, NEW, 3), Mapped::Same(5));
        assert_eq!(map_to_old(OLD, NEW, 5), Mapped::Same(3));
    }

    #[test]
    fn mapping_pairs_an_edited_line_both_ways() {
        assert_eq!(map_to_new(OLD, NEW, 2), Mapped::Changed(3));
        assert_eq!(map_to_old(OLD, NEW, 3), Mapped::Changed(2));
    }

    #[test]
    fn an_added_line_has_no_old_counterpart() {
        assert_eq!(map_to_old(OLD, NEW, 0), Mapped::Gone(0), "added at the top");
        assert_eq!(
            map_to_old(OLD, NEW, 4),
            Mapped::Gone(2),
            "added beside the edit"
        );
    }

    #[test]
    fn a_deleted_line_has_no_new_counterpart() {
        let new = "one\nfour\n";
        assert_eq!(map_to_new(OLD, new, 1), Mapped::Gone(1));
        assert_eq!(map_to_new(OLD, new, 3), Mapped::Same(1));
    }

    // ---- stepping through a repo ----------------------------------------------------------------

    struct Repo {
        dir: tempfile::TempDir,
        repo: git2::Repository,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init(dir.path()).unwrap();
            Self { dir, repo }
        }

        /// Commit `content` as `f.rs`, returning the commit's hash.
        fn commit(&self, subject: &str, content: &str) -> String {
            self.commit_files(subject, &[("f.rs", Some(content))])
        }

        fn commit_files(&self, subject: &str, files: &[(&str, Option<&str>)]) -> String {
            let mut index = self.repo.index().unwrap();
            for (name, content) in files {
                let path = self.dir.path().join(name);
                match content {
                    Some(c) => {
                        std::fs::write(&path, c).unwrap();
                        index.add_path(Path::new(name)).unwrap();
                    }
                    None => {
                        std::fs::remove_file(&path).unwrap();
                        index.remove_path(Path::new(name)).unwrap();
                    }
                }
            }
            index.write().unwrap();
            let tree = self.repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("T", "t@example.com").unwrap();
            let parent = self.repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
            self.repo
                .commit(Some("HEAD"), &sig, &sig, subject, &tree, &parents)
                .unwrap()
                .to_string()
        }

        fn step(&self, rev: Option<&str>, line: u32, line_scope: bool, step: Step) -> Stepped {
            self.step_with(rev, line, line_scope, step, None)
        }

        fn step_with(
            &self,
            rev: Option<&str>,
            line: u32,
            line_scope: bool,
            step: Step,
            worktree: Option<&str>,
        ) -> Stepped {
            let disk = std::fs::read_to_string(self.dir.path().join("f.rs")).ok();
            super::step(
                self.dir.path(),
                &Origin {
                    rev,
                    path: "f.rs",
                    line,
                    worktree: worktree.or(disk.as_deref()),
                },
                line_scope,
                step,
            )
        }
    }

    fn at(rev: &str, line: u32) -> Option<Landing> {
        Some(Landing::Revision {
            rev: rev.to_string(),
            path: "f.rs".to_string(),
            line,
        })
    }

    fn working(line: u32) -> Option<Landing> {
        Some(Landing::WorkingFile {
            path: "f.rs".to_string(),
            line,
        })
    }

    /// Three commits, each touching a different line; a fourth touching none of them.
    fn three_edits() -> (Repo, [String; 4]) {
        let r = Repo::new();
        let c1 = r.commit("one", "a = 1\nb = 1\nc = 1\n");
        let c2 = r.commit("two", "a = 1\nb = 2\nc = 1\n");
        let c3 = r.commit("three", "a = 1\nb = 3\nc = 3\n");
        let c4 = r.commit("four", "head\na = 1\nb = 3\nc = 3\n");
        (r, [c1, c2, c3, c4])
    }

    #[test]
    fn file_steps_visit_every_change_and_come_back() {
        let (r, [c1, c2, c3, c4]) = three_edits();
        // From the working file the first stop is the last change to the file — four's, which is
        // HEAD's version: the stop shows its own change, so it is never a no-op to land on.
        let s = r.step(None, 3, false, Step::Older);
        assert_eq!(s.landing, at(&c4, 3));
        let s = r.step(Some(&c4), 3, false, Step::Older);
        assert_eq!(
            s.landing,
            at(&c3, 2),
            "carried across the inserted top line"
        );
        let s = r.step(Some(&c3), 2, false, Step::Older);
        assert_eq!(s.landing, at(&c2, 2));
        let s = r.step(Some(&c2), 2, false, Step::Older);
        assert_eq!(s.landing, at(&c1, 2));
        let s = r.step(Some(&c1), 2, false, Step::Older);
        assert_eq!(s.landing, None);
        assert!(matches!(s.note, Some(VersionNote::Oldest { at }) if at.subject == "one"));

        let s = r.step(Some(&c1), 2, false, Step::Newer);
        assert_eq!(s.landing, at(&c2, 2));
        let s = r.step(Some(&c3), 2, false, Step::Newer);
        assert_eq!(s.landing, at(&c4, 3));
        let s = r.step(Some(&c4), 3, false, Step::Newer);
        assert_eq!(s.landing, working(3), "past HEAD is the working file");
        let s = r.step(None, 3, false, Step::Newer);
        assert_eq!(s.note, Some(VersionNote::Newest));
    }

    #[test]
    fn uncommitted_edits_dont_move_the_first_stop() {
        let (r, [.., c4]) = three_edits();
        // The working file's own change is its inline diff; the first older stop is still HEAD's.
        let s = r.step_with(
            None,
            0,
            false,
            Step::Older,
            Some("edited\na = 1\nb = 3\nc = 3\n"),
        );
        assert_eq!(s.landing, at(&c4, 0));
    }

    #[test]
    fn a_version_that_left_the_file_alone_steps_to_the_change_it_shows() {
        let (r, [_, _, c3, c4]) = three_edits();
        let unrelated = r.commit_files("unrelated", &[("other.rs", Some("x"))]);
        // The unrelated commit holds four's version unchanged, so the nearest older stop is four
        // itself — the change on screen — and only then three.
        let s = r.step(Some(&unrelated), 1, false, Step::Older);
        assert_eq!(s.landing, at(&c4, 1));
        let s = r.step(Some(&c4), 1, false, Step::Older);
        assert_eq!(s.landing, at(&c3, 0));
    }

    #[test]
    fn line_steps_visit_the_changes_to_the_line() {
        let (r, [c1, c2, c3, c4]) = three_edits();
        // `b` was set by one, changed by two and three. From the working file (line 2 there) the
        // first stop is three — the change that made it what it is — skipping four, which left
        // it alone.
        let s = r.step(None, 2, true, Step::Older);
        assert_eq!(s.landing, at(&c3, 1));
        let s = r.step(Some(&c3), 1, true, Step::Older);
        assert_eq!(s.landing, at(&c2, 1));
        let s = r.step(Some(&c2), 1, true, Step::Older);
        assert_eq!(s.landing, at(&c1, 1));
        let s = r.step(Some(&c1), 1, true, Step::Older);
        assert_eq!(s.landing, None, "born with the file");
        assert!(
            matches!(s.note, Some(VersionNote::LineAdded { at: Some(at) }) if at.subject == "one")
        );

        // From four, which left `b` alone, the nearest older stop is still three.
        let s = r.step(Some(&c4), 2, true, Step::Older);
        assert_eq!(s.landing, at(&c3, 1));

        // And back: each newer step is the inverse, then four's insertion leaves `b` alone, so the
        // walk runs to the working file.
        let s = r.step(Some(&c1), 1, true, Step::Newer);
        assert_eq!(s.landing, at(&c2, 1));
        let s = r.step(Some(&c2), 1, true, Step::Newer);
        assert_eq!(s.landing, at(&c3, 1));
        let s = r.step(Some(&c3), 1, true, Step::Newer);
        assert_eq!(s.landing, working(2));

        // `a` was only ever set by one.
        let s = r.step(None, 1, true, Step::Older);
        assert_eq!(s.landing, at(&c1, 0));
    }

    #[test]
    fn an_uncommitted_edit_steps_to_the_committed_lines_change() {
        let (r, [_, _, c3, _]) = three_edits();
        let s = r.step_with(
            None,
            2,
            true,
            Step::Older,
            Some("head\na = 1\nb = 4\nc = 3\n"),
        );
        // HEAD's `b = 3`, set by three.
        assert_eq!(s.landing, at(&c3, 1));
        // A line written in the working tree has nothing committed behind it.
        let s = r.step_with(
            None,
            0,
            true,
            Step::Older,
            Some("new\nhead\na = 1\nb = 3\nc = 3\n"),
        );
        assert_eq!(s.landing, None);
        assert_eq!(s.note, Some(VersionNote::LineAdded { at: None }));
    }

    #[test]
    fn a_line_added_partway_stops_at_its_addition() {
        let (r, [.., c4]) = three_edits();
        // `head` was added by four: that is its one stop, and there is nothing older.
        let s = r.step(None, 0, true, Step::Older);
        assert_eq!(s.landing, at(&c4, 0));
        let s = r.step(Some(&c4), 0, true, Step::Older);
        assert_eq!(s.landing, None);
        assert!(
            matches!(s.note, Some(VersionNote::LineAdded { at: Some(at) }) if at.subject == "four")
        );
    }

    #[test]
    fn a_deleted_line_says_so_going_forward() {
        let r = Repo::new();
        let c1 = r.commit("one", "keep\ndrop me\nkeep too\n");
        let c2 = r.commit("two", "keep\nkeep too\n");
        let s = r.step(Some(&c1), 1, true, Step::Newer);
        assert_eq!(s.landing, at(&c2, 1));
        assert!(
            matches!(s.note, Some(VersionNote::LineRemoved { at: Some(at) }) if at.subject == "two")
        );
    }

    #[test]
    fn newer_needs_the_version_on_heads_first_parent_chain() {
        let r = Repo::new();
        r.commit("one", "a\n");
        // A commit no branch reaches.
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        let head = r.repo.head().unwrap().peel_to_commit().unwrap();
        let tree = head.tree().unwrap();
        let orphan = r
            .repo
            .commit(None, &sig, &sig, "detached", &tree, &[&head])
            .unwrap()
            .to_string();
        let s = r.step(Some(&orphan), 0, false, Step::Newer);
        assert_eq!(s.note, Some(VersionNote::Unreachable));
    }

    #[test]
    fn a_file_deleted_on_the_branch_ends_the_newer_walk() {
        let r = Repo::new();
        let c1 = r.commit("one", "a\n");
        r.commit_files("gone", &[("f.rs", None)]);
        let s = r.step(Some(&c1), 0, false, Step::Newer);
        assert_eq!(s.landing, None);
        assert!(
            matches!(s.note, Some(VersionNote::FileRemoved { at: Some(at) }) if at.subject == "gone")
        );
    }

    #[test]
    fn an_untracked_file_has_no_versions() {
        let r = Repo::new();
        r.commit_files("other", &[("other.rs", Some("x"))]);
        std::fs::write(r.dir.path().join("f.rs"), "new\n").unwrap();
        assert_eq!(
            r.step(None, 0, false, Step::Older).note,
            Some(VersionNote::Untracked)
        );
    }
}
