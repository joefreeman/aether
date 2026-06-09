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

use aether_protocol::git::{BlameInfo, CommitInfo, GitStatus};
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
    /// Index (staging-area) content of the file, LF-normalized like `blob`. `None` when the file
    /// has no index entry (untracked, or a staged whole-file deletion). The unstaged diff is the
    /// buffer against this; the staged diff is this against `blob`.
    pub index_blob: Option<Vec<u8>>,
    /// Current branch name (or short commit hash when detached). `None` outside a repo.
    pub branch: Option<String>,
    /// Staged diff (HEAD → index), computed once here since it's independent of the live buffer and
    /// only changes when HEAD or the index does (i.e. on the same refresh trigger as the blobs).
    pub staged_hunks: Vec<DiffHunk>,
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
    let index_blob = index_blob_bytes(&repo, &rel_path).map(normalize_lf);
    // Staged diff is HEAD → index; absent sides count as empty (a staged add has no HEAD side, a
    // staged whole-file delete has no index side).
    let staged_hunks =
        hunks_from_buffers(blob.as_deref().unwrap_or(b""), index_blob.as_deref().unwrap_or(b""));
    GitBaseline {
        repo: Some(GitRepo { workdir, rel_path }),
        blob,
        index_blob,
        branch: current_branch(&repo),
        staged_hunks,
    }
}

/// The file's staged (index) content as raw bytes, or `None` when it has no index entry. Stage `0`
/// is the normal, non-conflict slot; during a merge conflict there's no stage-0 entry and we fall
/// back to `None` (staged/unstaged for conflicted files is out of scope).
fn index_blob_bytes(repo: &git2::Repository, rel: &Path) -> Option<Vec<u8>> {
    let index = repo.index().ok()?;
    let entry = index.get_path(rel, 0)?;
    let blob = repo.find_blob(entry.id).ok()?;
    Some(blob.content().to_vec())
}

/// Current branch name, or a short commit hash when HEAD is detached. Handles an unborn branch
/// (fresh repo, no commits yet) by reading the symbolic `HEAD` target. `None` only on error.
fn current_branch(repo: &git2::Repository) -> Option<String> {
    if let Ok(head) = repo.head() {
        if head.is_branch() {
            return head.shorthand().map(String::from);
        }
        if let Some(oid) = head.target() {
            let s = oid.to_string();
            return Some(s[..7.min(s.len())].to_string());
        }
    }
    // Unborn branch: HEAD is a symbolic ref to a branch that has no commit yet.
    repo.find_reference("HEAD")
        .ok()?
        .symbolic_target()
        .and_then(|t| t.strip_prefix("refs/heads/"))
        .map(String::from)
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
                })
            });
            match meta {
                Some(m) => BlameInfo {
                    commit: m.commit.clone(),
                    author: m.author.clone(),
                    timestamp: m.timestamp,
                    is_uncommitted: false,
                },
                None => BlameInfo {
                    commit: String::new(),
                    author: String::new(),
                    timestamp: 0,
                    is_uncommitted: true,
                },
            }
        }));
    }
    Some(out)
}

/// Resolved author/time for one commit, cached across the lines that share it.
struct CommitMeta {
    commit: String,
    author: String,
    timestamp: i64,
}

/// Resolve full details for a single commit (the blame "commit details" popover). `rev` is any
/// revision libgit2 can parse — typically the abbreviated hash from a line's [`BlameInfo`]. Returns
/// `None` if the repo can't be opened or `rev` doesn't resolve to a commit.
pub fn commit_info(repo: &GitRepo, rev: &str) -> Option<CommitInfo> {
    let git_repo = git2::Repository::open(&repo.workdir).ok()?;
    let commit = git_repo.revparse_single(rev).ok()?.peel_to_commit().ok()?;
    let sig = commit.author();
    Some(CommitInfo {
        commit: commit.id().to_string(),
        author: sig.name().unwrap_or("(unknown)").to_string(),
        email: sig.email().unwrap_or_default().to_string(),
        date: format_commit_time(sig.when()),
        message: commit.message().unwrap_or_default().trim_end().to_string(),
    })
}

/// Git status of each immediate child of `dir`, keyed by leaf name, for colouring the file
/// explorer. One repo-wide [`git2::Repository::statuses`] call per listing — repo discovery (the
/// expensive part) runs here, never on the keystroke path: the explorer only rebuilds candidates
/// on open and directory navigation, not on filter input.
///
/// A directory child takes the **highest-priority** status among everything beneath it: each
/// status entry's repo-relative path is bucketed under its first path component below `dir`, so a
/// change deep in a subtree colours the top-level folder (folder aggregation). Untracked and
/// ignored subtrees are left collapsed (`recurse_*_dirs` off), so `node_modules/` is a single gray
/// bucket rather than a walk of thousands of files.
///
/// Best-effort: no repo, `dir` outside any repo, or any libgit2 error → an empty map, and the
/// explorer falls back to its default colours.
pub fn dir_statuses(dir: &Path) -> HashMap<String, GitStatus> {
    let mut out: HashMap<String, GitStatus> = HashMap::new();
    let Ok(canonical) = dir.canonicalize() else {
        return out;
    };
    let Ok(repo) = git2::Repository::discover(&canonical) else {
        return out;
    };
    let Some(workdir) = repo.workdir().and_then(|w| w.canonicalize().ok()) else {
        return out;
    };
    // The listed directory relative to the repo root — an empty path when listing the root itself,
    // which `strip_prefix` below treats as "every entry is in scope".
    let Ok(rel_dir) = canonical.strip_prefix(&workdir) else {
        return out;
    };

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(true)
        .recurse_untracked_dirs(false)
        .recurse_ignored_dirs(false)
        .exclude_submodules(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return out;
    };

    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        // Keep only entries inside the listed directory, then bucket each under the immediate child
        // of `dir` it lives in (a file → itself; a deeper path → the top folder).
        let Ok(suffix) = Path::new(path).strip_prefix(rel_dir) else {
            continue;
        };
        let Some(child) = suffix.components().next() else {
            continue;
        };
        let Some(status) = classify_status(entry.status()) else {
            continue;
        };
        let name = child.as_os_str().to_string_lossy().into_owned();
        out.entry(name)
            .and_modify(|cur| {
                if status_rank(status) < status_rank(*cur) {
                    *cur = status;
                }
            })
            .or_insert(status);
    }
    out
}

/// A repo's per-file status, scoped to one project root, for the Files picker. Holds the root's
/// own path within the repo plus a `repo-relative path → status` map, so a file's status is a
/// single lookup keyed by its root-relative path (no per-file repo discovery or canonicalisation).
pub struct RepoStatus {
    /// The project root's path relative to the repo workdir (empty when the root *is* the repo
    /// root). Joined with a file's root-relative path to form its repo-relative key.
    root_rel: PathBuf,
    map: HashMap<PathBuf, GitStatus>,
}

impl RepoStatus {
    /// Status of a file given its path relative to the project root (forward-slash separated, as
    /// stored in the workspace index). `None` when the file is clean.
    pub fn status_of(&self, root_rel_path: &str) -> Option<GitStatus> {
        self.map.get(&self.root_rel.join(root_rel_path)).copied()
    }
}

/// Resolve the Git status of every changed file under `root` in one `statuses()` pass, for the
/// Files picker. Untracked directories are recursed so each untracked file is reported
/// individually (the picker lists individual files); ignored files are excluded — the workspace
/// walker already skips them, so they never reach the picker. Best-effort: `None` when `root`
/// isn't in a repo or any libgit2 call fails.
pub fn repo_status_for_root(root: &Path) -> Option<RepoStatus> {
    let canonical = root.canonicalize().ok()?;
    let repo = git2::Repository::discover(&canonical).ok()?;
    let workdir = repo.workdir().and_then(|w| w.canonicalize().ok())?;
    let root_rel = canonical.strip_prefix(&workdir).ok()?.to_path_buf();

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repo.statuses(Some(&mut opts)).ok()?;

    let mut map = HashMap::new();
    for entry in statuses.iter() {
        if let Some(path) = entry.path() {
            if let Some(status) = classify_status(entry.status()) {
                map.insert(PathBuf::from(path), status);
            }
        }
    }
    Some(RepoStatus { root_rel, map })
}

/// Fold libgit2's per-path status bitflags into the one [`GitStatus`] we colour with, in priority
/// order (conflict beats deletion beats modification beats add beats untracked beats ignored).
/// `None` for an entry that carries no flag we render (e.g. a path that is exactly current).
fn classify_status(s: git2::Status) -> Option<GitStatus> {
    use git2::Status as S;
    if s.contains(S::CONFLICTED) {
        Some(GitStatus::Conflicted)
    } else if s.intersects(S::INDEX_DELETED | S::WT_DELETED) {
        Some(GitStatus::Deleted)
    } else if s.intersects(
        S::INDEX_MODIFIED
            | S::WT_MODIFIED
            | S::INDEX_RENAMED
            | S::WT_RENAMED
            | S::INDEX_TYPECHANGE
            | S::WT_TYPECHANGE,
    ) {
        Some(GitStatus::Modified)
    } else if s.contains(S::INDEX_NEW) {
        Some(GitStatus::Added)
    } else if s.contains(S::WT_NEW) {
        Some(GitStatus::Untracked)
    } else if s.contains(S::IGNORED) {
        Some(GitStatus::Ignored)
    } else {
        None
    }
}

/// Aggregation priority — lower is higher-priority (wins folder roll-up). Mirrors the declaration
/// order of [`GitStatus`].
fn status_rank(s: GitStatus) -> u8 {
    match s {
        GitStatus::Conflicted => 0,
        GitStatus::Deleted => 1,
        GitStatus::Modified => 2,
        GitStatus::Added => 3,
        GitStatus::Untracked => 4,
        GitStatus::Ignored => 5,
    }
}

/// Format a git signature time in its own recorded timezone as `YYYY-MM-DD HH:MM:SS ±HHMM` (git's
/// default `log` style), with no external date crate.
fn format_commit_time(t: git2::Time) -> String {
    let offset_min = t.offset_minutes() as i64;
    let local = t.seconds() + offset_min * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let sign = if offset_min < 0 { '-' } else { '+' };
    let off = offset_min.abs();
    format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {sign}{:02}{:02}",
        off / 60,
        off % 60
    )
}

/// Days since the Unix epoch → `(year, month, day)`. Howard Hinnant's `civil_from_days` algorithm,
/// valid across the full range of `i64` day counts.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
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

    // ---- commit-time formatting (no repo needed) ------------------------------------------------

    #[test]
    fn format_commit_time_renders_in_recorded_timezone() {
        // 1_700_000_000 == 2023-11-14 22:13:20 UTC.
        assert_eq!(
            format_commit_time(git2::Time::new(1_700_000_000, 0)),
            "2023-11-14 22:13:20 +0000"
        );
        // +60 min offset shifts the wall-clock forward an hour and is rendered as +0100.
        assert_eq!(
            format_commit_time(git2::Time::new(1_700_000_000, 60)),
            "2023-11-14 23:13:20 +0100"
        );
        // A negative offset (e.g. US Pacific, -480 min) rolls back across midnight.
        assert_eq!(
            format_commit_time(git2::Time::new(1_700_000_000, -480)),
            "2023-11-14 14:13:20 -0800"
        );
        // The Unix epoch itself.
        assert_eq!(
            format_commit_time(git2::Time::new(0, 0)),
            "1970-01-01 00:00:00 +0000"
        );
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

    // ---- dir_statuses (explorer colouring) ------------------------------------------------------

    /// Init a repo at `root`, write each `(path, content)`, stage them all, and commit. Paths may
    /// be nested (`sub/x.rs`); intermediate dirs are created.
    fn repo_with_files(root: &Path, files: &[(&str, &str)]) {
        let repo = git2::Repository::init(root).unwrap();
        let mut index = repo.index().unwrap();
        for (rel, content) in files {
            let abs = root.join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, content).unwrap();
            index.add_path(Path::new(rel)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "t@e.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    #[test]
    fn dir_statuses_colours_children_by_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(
            root,
            &[
                ("clean.rs", "clean\n"),
                ("mod.rs", "before\n"),
                ("sub/deep.rs", "deep\n"),
            ],
        );
        // Working-tree changes on disk — `statuses` reads the disk, not a live buffer.
        std::fs::write(root.join("mod.rs"), "after\n").unwrap(); // tracked → Modified
        std::fs::write(root.join("sub/deep.rs"), "changed\n").unwrap(); // change beneath sub/
        std::fs::write(root.join("new.rs"), "new\n").unwrap(); // Untracked
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("debug.log"), "noise\n").unwrap(); // Ignored

        let st = dir_statuses(root);
        assert_eq!(st.get("clean.rs"), None, "unchanged tracked file is uncoloured");
        assert_eq!(st.get("mod.rs"), Some(&GitStatus::Modified));
        assert_eq!(
            st.get("sub"),
            Some(&GitStatus::Modified),
            "folder inherits a descendant's change (aggregation)"
        );
        assert_eq!(st.get("new.rs"), Some(&GitStatus::Untracked));
        assert_eq!(st.get("debug.log"), Some(&GitStatus::Ignored));
    }

    #[test]
    fn dir_statuses_folder_prefers_real_change_over_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(
            root,
            &[("pkg/tracked.rs", "v1\n"), (".gitignore", "pkg/*.log\n")],
        );
        std::fs::write(root.join("pkg/tracked.rs"), "v2\n").unwrap(); // Modified
        std::fs::write(root.join("pkg/out.log"), "noise\n").unwrap(); // Ignored, same folder

        let st = dir_statuses(root);
        assert_eq!(
            st.get("pkg"),
            Some(&GitStatus::Modified),
            "a real change outranks an ignored sibling in the same folder"
        );
    }

    #[test]
    fn dir_statuses_lists_a_subdirectory() {
        // The explorer listing `sub/` keys statuses by the names visible there, not repo-relative.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(root, &[("sub/deep.rs", "deep\n"), ("top.rs", "top\n")]);
        std::fs::write(root.join("sub/deep.rs"), "changed\n").unwrap();

        let st = dir_statuses(&root.join("sub"));
        assert_eq!(st.get("deep.rs"), Some(&GitStatus::Modified));
        assert_eq!(st.get("top.rs"), None, "a sibling outside the listed dir is absent");
    }

    #[test]
    fn dir_statuses_no_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        assert!(dir_statuses(dir.path()).is_empty());
    }

    // ---- repo_status_for_root (Files picker) ----------------------------------------------------

    #[test]
    fn repo_status_for_root_reports_per_file_status() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(
            root,
            &[("clean.rs", "clean\n"), ("sub/mod.rs", "before\n")],
        );
        std::fs::write(root.join("sub/mod.rs"), "after\n").unwrap(); // modified, nested
        std::fs::write(root.join("new.rs"), "new\n").unwrap(); // untracked at root

        let rs = repo_status_for_root(root).expect("root is in a repo");
        // Keyed by the path relative to the project root (which == repo root here).
        assert_eq!(rs.status_of("clean.rs"), None, "clean file has no status");
        assert_eq!(rs.status_of("sub/mod.rs"), Some(GitStatus::Modified));
        assert_eq!(rs.status_of("new.rs"), Some(GitStatus::Untracked));
    }

    #[test]
    fn repo_status_for_root_keys_relative_to_a_subdir_root() {
        // When the project root is a subdirectory of the repo, lookups are still keyed by the
        // path relative to that root — the repo-relative prefix is handled internally.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        repo_with_files(repo_root, &[("pkg/mod.rs", "before\n"), ("top.rs", "top\n")]);
        std::fs::write(repo_root.join("pkg/mod.rs"), "after\n").unwrap();

        let rs = repo_status_for_root(&repo_root.join("pkg")).expect("subdir is in the repo");
        assert_eq!(rs.status_of("mod.rs"), Some(GitStatus::Modified));
    }

    #[test]
    fn repo_status_for_root_no_repo_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_status_for_root(dir.path()).is_none());
    }
}
