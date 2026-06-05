//! Git integration: diffing the live buffer against a Git baseline.
//!
//! Computes per-line change hunks between a file's committed content (HEAD) and the buffer's
//! current in-memory text, using libgit2's in-memory patch API so the diff reflects *unsaved*
//! edits — not what's on disk. Drives the gutter change-bar, the inline diff view, and blame.
//!
//! Best-effort throughout: a missing repo, an untracked file, or any libgit2 error folds into an
//! empty result, so git integration can never block opening or editing a buffer.
//!
//! ## Cost model
//! Repository discovery and reading the committed blob are the expensive parts, so they're done
//! **once** — in [`load_baseline`], on open and whenever HEAD changes (the file watcher refreshes
//! it on external commit/checkout/stage). Per-edit work is just [`diff_hunks`], an in-memory diff
//! of the cached baseline against the buffer — no repo I/O on the keystroke path.
//!
//! ## Staged vs unstaged (planned)
//! Today the baseline is **HEAD**, surfacing all uncommitted changes (staged + unstaged),
//! matching `git diff HEAD`. A later split — unstaged = buffer vs index, staged = index vs HEAD —
//! is a change of baseline source in [`load_baseline`] plus a `stage` tag on each [`DiffHunk`];
//! the hunk shape and anchoring below are unaffected.

use aether_protocol::git::BlameInfo;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One contiguous run of changes between the baseline and the live buffer, in **0-based buffer
/// line** coordinates.
// Phase 1 produces and stores these; the fields are consumed by the inline diff renderer
// (Phase 3) and gutter. `allow(dead_code)` keeps a plain `cargo build` quiet until then — the
// test module already exercises every field.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// What sort of change this is, derived from which sides carry lines.
    pub kind: ChangeKind,
    /// The buffer line this hunk anchors to:
    ///   - `Added` / `Modified`: the first changed line on the new (buffer) side.
    ///   - `Deleted`: the line the removed text should render *above* (the surviving line that
    ///     now follows the deletion). For a deletion at end-of-buffer this is `line_count`.
    pub anchor_line: u32,
    /// Number of buffer lines this hunk covers on the new side. `0` for a pure deletion.
    pub new_lines: u32,
    /// Baseline lines removed or replaced by this hunk, in order, newline-free. Empty for a pure
    /// addition. The inline diff renders these as phantom "deleted" rows above `anchor_line`.
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A buffer's resolved Git location: the repo working directory and the file's path within it.
/// Cached per buffer so edits and blame don't re-run repository discovery.
#[derive(Debug, Clone)]
pub struct GitRepo {
    pub workdir: PathBuf,
    pub rel_path: PathBuf,
}

/// Cached Git baseline for a buffer: where it lives in a repo (if anywhere) and the committed
/// (HEAD) content to diff against. Resolved on open and refreshed when HEAD changes — *not* on
/// every edit.
#[derive(Debug, Clone, Default)]
pub struct GitBaseline {
    /// `Some` when the file is inside a Git repo. A cached `None` means "checked, not in a repo",
    /// so editing a non-git file doesn't re-run discovery every keystroke.
    pub repo: Option<GitRepo>,
    /// HEAD content of the file, **LF-normalized** so a CRLF-committed file doesn't read as
    /// "every line modified". `None` when untracked / not committed / no repo.
    pub blob: Option<Vec<u8>>,
}

/// Resolve a path's repo and read its HEAD baseline. The expensive part — discovery plus reading
/// and decompressing the committed blob — so it runs on open and on external Git changes, never
/// per edit. Synchronous and `!Send`-clean (every libgit2 object is dropped before returning).
pub fn load_baseline(path: &Path) -> GitBaseline {
    // Canonicalise so `strip_prefix` against the (also canonicalised) workdir is symlink-proof,
    // and so a not-yet-on-disk file (new buffer) resolves to "no repo" rather than erroring.
    let Ok(canonical) = path.canonicalize() else {
        return GitBaseline::default();
    };
    let Ok(repo) = git2::Repository::discover(&canonical) else {
        return GitBaseline::default();
    };
    let Some(workdir) = repo.workdir().and_then(|w| w.canonicalize().ok()) else {
        return GitBaseline::default();
    };
    let Ok(rel) = canonical.strip_prefix(&workdir) else {
        return GitBaseline::default();
    };
    let rel_path = rel.to_path_buf();
    let blob = head_blob_bytes(&repo, &rel_path).map(normalize_lf);
    GitBaseline {
        repo: Some(GitRepo { workdir, rel_path }),
        blob,
    }
}

/// The file's committed (HEAD) content as raw bytes, or `None` when untracked / not committed.
fn head_blob_bytes(repo: &git2::Repository, rel: &Path) -> Option<Vec<u8>> {
    let tree = repo.head().ok()?.peel_to_tree().ok()?;
    let entry = tree.get_path(rel).ok()?;
    let blob = entry.to_object(repo).ok()?.peel_to_blob().ok()?;
    Some(blob.content().to_vec())
}

/// CRLF → LF, matching how the editor normalizes buffer text on load (`Buffer::load_from_file`),
/// so a CRLF-committed file doesn't diff as entirely modified against the LF buffer. A lone `\r`
/// is left untouched.
fn normalize_lf(bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.contains(&b'\r') {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Diff a cached `baseline` against the live buffer. Cheap — no repo I/O — so it's fine on every
/// edit. Empty when there's no baseline (untracked / no repo) or the sides match. The buffer text
/// is only materialised when a baseline exists.
pub fn diff_hunks(baseline: Option<&[u8]>, current: &ropey::Rope) -> Vec<DiffHunk> {
    let Some(baseline) = baseline else {
        return Vec::new();
    };
    let new = current.to_string();
    hunks_from_buffers(baseline, new.as_bytes())
}

/// Core diff: turn two in-memory buffers into buffer-line hunks. Factored out from
/// [`compute_hunks`] so it's testable without touching a repo.
fn hunks_from_buffers(old: &[u8], new: &[u8]) -> Vec<DiffHunk> {
    let mut opts = git2::DiffOptions::new();
    // No surrounding context: we want one hunk per actual change run, and `force_text` keeps
    // libgit2 from guessing "binary" on path-less buffers (which would yield zero hunks).
    opts.context_lines(0).force_text(true);

    let patch = match git2::Patch::from_buffers(old, None, new, None, Some(&mut opts)) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut hunks = Vec::new();
    for h in 0..patch.num_hunks() {
        let Ok((hunk, _)) = patch.hunk(h) else { continue };
        let line_count = patch.num_lines_in_hunk(h).unwrap_or(0);

        let mut deleted = Vec::new();
        for l in 0..line_count {
            let Ok(line) = patch.line_in_hunk(h, l) else { continue };
            if line.origin() == '-' {
                deleted.push(line_content(&line));
            }
        }

        let new_lines = hunk.new_lines();
        let kind = if hunk.old_lines() == 0 {
            ChangeKind::Added
        } else if new_lines == 0 {
            ChangeKind::Deleted
        } else {
            ChangeKind::Modified
        };

        // libgit2 hunk line numbers are 1-based. For add/modify, `new_start` is the first changed
        // line → 0-based is `new_start - 1`. For a pure deletion the new side is empty and
        // `new_start` is the 1-based line the removal sits *after*, which is exactly the 0-based
        // index of the line it now sits *above* — so we use it verbatim.
        let anchor_line = if new_lines == 0 {
            hunk.new_start()
        } else {
            hunk.new_start().saturating_sub(1)
        };

        hunks.push(DiffHunk {
            kind,
            anchor_line,
            new_lines,
            deleted,
        });
    }
    hunks
}

/// Blame the whole file, one entry per 0-based buffer line (`None` for a line with no blame, e.g.
/// the trailing empty line of a newline-terminated file). Outer `None` means blame isn't
/// available at all: no repo, untracked file, or any libgit2 error.
///
/// Blame is computed against the **live buffer** via libgit2's buffer-blame, so lines the user
/// has edited but not committed report as uncommitted instead of being misattributed to whoever
/// last touched that line number. Whole-file (libgit2 has no cheap single-line blame); callers
/// cache the result per buffer revision so cursor movement doesn't recompute.
///
/// Takes the cached [`GitRepo`] so it doesn't re-run discovery. Synchronous and `!Send`-clean.
pub fn compute_blame(repo: &GitRepo, current: &ropey::Rope) -> Option<Vec<Option<BlameInfo>>> {
    let git_repo = git2::Repository::open(&repo.workdir).ok()?;

    let committed = git_repo.blame_file(&repo.rel_path, None).ok()?;
    let text = current.to_string();
    // Re-map the committed blame onto the in-memory buffer so unsaved edits are attributed to the
    // working tree rather than the line they displaced.
    let blame = committed.blame_buffer(text.as_bytes()).ok()?;

    // Resolve each commit at most once — many lines typically share a commit. `None` marks an oid
    // that doesn't resolve to a commit, which is exactly the working-tree (uncommitted) case:
    // libgit2's buffer-blame gives those hunks a null signature, so we must *not* read it
    // directly (it dereferences a null pointer) — going through `find_commit` sidesteps that.
    let mut commits: HashMap<git2::Oid, Option<CommitMeta>> = HashMap::new();
    let line_count = current.len_lines();
    let mut out = Vec::with_capacity(line_count);
    for i in 0..line_count {
        // libgit2 blame lines are 1-based; a line past the content (trailing empty line) is None.
        out.push(blame.get_line(i + 1).map(|hunk| {
            let oid = hunk.final_commit_id();
            let meta = commits.entry(oid).or_insert_with(|| {
                let commit = git_repo.find_commit(oid).ok()?;
                let sig = commit.author();
                Some(CommitMeta {
                    commit: format!("{oid:.7}"),
                    author: sig.name().unwrap_or("(unknown)").to_string(),
                    timestamp: sig.when().seconds(),
                    summary: commit.summary().unwrap_or_default().to_string(),
                })
            });
            match meta {
                Some(m) => BlameInfo {
                    commit: m.commit.clone(),
                    author: m.author.clone(),
                    timestamp: m.timestamp,
                    summary: m.summary.clone(),
                    is_uncommitted: false,
                },
                None => BlameInfo {
                    commit: String::new(),
                    author: String::new(),
                    timestamp: 0,
                    summary: String::new(),
                    is_uncommitted: true,
                },
            }
        }));
    }
    Some(out)
}

/// Resolved author/summary for one commit, cached across the lines that share it.
struct CommitMeta {
    commit: String,
    author: String,
    timestamp: i64,
    summary: String,
}

/// A diff line's text with its trailing newline stripped (libgit2 includes it in the content).
fn line_content(line: &git2::DiffLine) -> String {
    let mut s = String::from_utf8_lossy(line.content()).into_owned();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rope(s: &str) -> ropey::Rope {
        ropey::Rope::from_str(s)
    }

    // ---- hunks_from_buffers (no repo needed) ----------------------------------------------------

    #[test]
    fn identical_buffers_have_no_hunks() {
        assert!(hunks_from_buffers(b"a\nb\nc\n", b"a\nb\nc\n").is_empty());
    }

    #[test]
    fn pure_addition_in_middle() {
        // Insert "new" between b and c.
        let hunks = hunks_from_buffers(b"a\nb\nc\n", b"a\nb\nnew\nc\n");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.kind, ChangeKind::Added);
        assert_eq!(h.anchor_line, 2, "added line is 0-based buffer line 2");
        assert_eq!(h.new_lines, 1);
        assert!(h.deleted.is_empty());
    }

    #[test]
    fn modification_carries_old_text() {
        let hunks = hunks_from_buffers(b"a\nb\nc\n", b"a\nB\nc\n");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.kind, ChangeKind::Modified);
        assert_eq!(h.anchor_line, 1);
        assert_eq!(h.new_lines, 1);
        assert_eq!(h.deleted, vec!["b".to_string()]);
    }

    #[test]
    fn pure_deletion_anchors_above_following_line() {
        // Delete b and c; surviving lines are a (0) then d (1). The removed block sat above d.
        let hunks = hunks_from_buffers(b"a\nb\nc\nd\n", b"a\nd\n");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.kind, ChangeKind::Deleted);
        assert_eq!(h.new_lines, 0);
        assert_eq!(h.anchor_line, 1, "deleted block renders above 0-based line 1 (d)");
        assert_eq!(h.deleted, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn deletion_at_start_anchors_above_line_zero() {
        let hunks = hunks_from_buffers(b"a\nb\nc\n", b"b\nc\n");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.kind, ChangeKind::Deleted);
        assert_eq!(h.anchor_line, 0);
        assert_eq!(h.deleted, vec!["a".to_string()]);
    }

    #[test]
    fn multiple_disjoint_hunks() {
        let hunks = hunks_from_buffers(b"a\nb\nc\nd\ne\n", b"a\nB\nc\nd\nE\n");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].anchor_line, 1);
        assert_eq!(hunks[0].deleted, vec!["b".to_string()]);
        assert_eq!(hunks[1].anchor_line, 4);
        assert_eq!(hunks[1].deleted, vec!["e".to_string()]);
    }

    // ---- compute_hunks against a real repo ------------------------------------------------------

    /// Init a repo in `dir`, write `name` with `committed` content, and commit it.
    fn repo_with_committed_file(dir: &Path, name: &str, committed: &str) -> PathBuf {
        let repo = git2::Repository::init(dir).expect("init repo");
        let file = dir.join(name);
        std::fs::write(&file, committed).expect("write file");

        let mut index = repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add");
        index.write().expect("index write");
        let tree = repo.find_tree(index.write_tree().expect("write_tree")).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("commit");
        file
    }

    // ---- load_baseline + diff_hunks against a real repo -----------------------------------------

    fn hunks_for(file: &Path, current: &str) -> Vec<DiffHunk> {
        let baseline = load_baseline(file);
        diff_hunks(baseline.blob.as_deref(), &rope(current))
    }

    #[test]
    fn diff_hunks_against_head() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\nthree\n");

        // Live buffer modifies line 2 — never written to disk; the diff is against the cached
        // baseline, proving it reflects unsaved edits.
        let hunks = hunks_for(&file, "one\nTWO\nthree\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, ChangeKind::Modified);
        assert_eq!(hunks[0].anchor_line, 1);
        assert_eq!(hunks[0].deleted, vec!["two".to_string()]);
    }

    #[test]
    fn diff_hunks_clean_buffer_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\n");
        assert!(hunks_for(&file, "one\ntwo\n").is_empty());
    }

    #[test]
    fn crlf_committed_file_is_not_all_modified() {
        // A file committed with CRLF endings, against an LF buffer (the editor normalizes to LF).
        // Without baseline normalization every line would diff as modified; with it, none do.
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let file = dir.path().join("crlf.rs");
        std::fs::write(&file, b"one\r\ntwo\r\nthree\r\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("crlf.rs")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "t@e.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let baseline = load_baseline(&file);
        assert_eq!(baseline.blob.as_deref(), Some(&b"one\ntwo\nthree\n"[..]));
        assert!(
            diff_hunks(baseline.blob.as_deref(), &rope("one\ntwo\nthree\n")).is_empty(),
            "LF buffer should match a CRLF-committed file after normalization"
        );
    }

    #[test]
    fn untracked_file_has_baseline_but_no_blob() {
        // Repo exists but the file was never committed → repo resolved, blob None → no hunks.
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let file = dir.path().join("untracked.rs");
        std::fs::write(&file, "hello\n").unwrap();
        let baseline = load_baseline(&file);
        assert!(baseline.repo.is_some(), "repo discovered");
        assert!(baseline.blob.is_none(), "untracked → no committed blob");
        assert!(diff_hunks(baseline.blob.as_deref(), &rope("hello\nworld\n")).is_empty());
    }

    #[test]
    fn no_repo_resolves_to_empty_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loose.rs");
        std::fs::write(&file, "hello\n").unwrap();
        let baseline = load_baseline(&file);
        assert!(baseline.repo.is_none());
        assert!(baseline.blob.is_none());
    }

    // ---- compute_blame --------------------------------------------------------------------------

    #[test]
    fn blame_attributes_committed_lines_and_flags_edits() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\nthree\n");
        let repo = load_baseline(&file).repo.expect("repo resolved");

        // Edit line 2 in the live buffer only (not on disk).
        let blame = compute_blame(&repo, &rope("one\nEDITED\nthree\n")).expect("blame available");

        // Line 0 is committed → attributed to the test author, not uncommitted.
        let l0 = blame[0].as_ref().expect("line 0 blamed");
        assert_eq!(l0.author, "Test");
        assert!(!l0.is_uncommitted);
        assert_eq!(l0.summary, "init");
        assert_eq!(l0.commit.len(), 7);

        // Line 1 was edited in the buffer → uncommitted.
        let l1 = blame[1].as_ref().expect("line 1 blamed");
        assert!(l1.is_uncommitted, "edited line should be uncommitted");

        // Line 2 is still the committed line.
        assert!(!blame[2].as_ref().unwrap().is_uncommitted);
    }

    #[test]
    fn no_repo_has_no_blame() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loose.rs");
        std::fs::write(&file, "x\n").unwrap();
        assert!(load_baseline(&file).repo.is_none());
    }
}
