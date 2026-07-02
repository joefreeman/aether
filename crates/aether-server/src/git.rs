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
//! ## Staged vs unstaged
//! [`load_baseline`] caches both blobs: HEAD (`git diff HEAD`, the default gutter base) and the
//! index (`git diff`, the unstaged-only base), plus the staged HEAD→index hunks for the status
//! bar. Hunk-wise staging/unstaging/reverting builds on the same hunks via [`merge_selected`] —
//! stage and unstage rewrite the file's index entry ([`write_index_blob`]); revert is an ordinary
//! buffer edit driven by the handler.

use aether_protocol::git::{BlameInfo, CommitInfo, GitStatus};
use aether_protocol::viewport::DiffStage;
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
    /// Where this hunk sits on the **old (baseline) side**, 0-based: the first removed baseline
    /// line, or — for a pure addition — the baseline line the new text is inserted *before*.
    /// Lets [`merge_selected`] splice hunks back into the baseline without re-walking the patch.
    pub old_start: u32,
    /// Display tag for the combined staged+unstaged view. Hunks straight from a diff are
    /// `Unstaged` (the renderer's single-colour default); [`compose_both`] tags the HEAD→index
    /// hunks it folds in as `Staged`. Where the two layers overlap, the marker/row builders
    /// resolve in favour of the unstaged side (the top layer).
    pub stage: DiffStage,
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
    let staged_hunks = hunks_from_buffers(
        blob.as_deref().unwrap_or(b""),
        index_blob.as_deref().unwrap_or(b""),
    );
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
            return head.shorthand().ok().map(String::from);
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
        .ok()
        .flatten()
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
/// is left untouched. `pub(crate)` so the Git-changes picker can normalise working-tree bytes
/// read off disk the same way buffers and baselines are.
pub(crate) fn normalize_lf(bytes: Vec<u8>) -> Vec<u8> {
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
/// [`compute_hunks`] so it's testable without touching a repo. `pub(crate)` so the Git-changes
/// picker can diff an index blob against working-tree / buffer content off the keystroke path.
pub(crate) fn hunks_from_buffers(old: &[u8], new: &[u8]) -> Vec<DiffHunk> {
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
        let Ok((hunk, _)) = patch.hunk(h) else {
            continue;
        };
        let line_count = patch.num_lines_in_hunk(h).unwrap_or(0);

        let mut deleted = Vec::new();
        for l in 0..line_count {
            let Ok(line) = patch.line_in_hunk(h, l) else {
                continue;
            };
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
        // Same trick on the old side: for a pure addition `old_start` is the 1-based line the
        // insertion sits after == the 0-based line it sits before.
        let old_start = if hunk.old_lines() == 0 {
            hunk.old_start()
        } else {
            hunk.old_start().saturating_sub(1)
        };

        hunks.push(DiffHunk {
            kind,
            anchor_line,
            new_lines,
            deleted,
            old_start,
            stage: DiffStage::Unstaged,
        });
    }
    hunks
}

/// Which changes a hunk operation (stage / unstage / revert) targets, in **new-side** 0-based
/// line coordinates of the diff being operated on (buffer lines for stage/revert, index lines
/// for unstage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkSelection {
    /// Bare cursor: the single hunk covering this line. `Added`/`Modified` hunks cover their
    /// new-side lines; a pure deletion belongs to the line its phantom rows render above
    /// (`anchor_line`), or to the last content line when it sits at end-of-buffer.
    WholeHunkAt(u32),
    /// An inclusive line span (a selection snapped to whole lines). Added lines are taken
    /// individually; a hunk's removed block is all-or-nothing, owned by the line it renders above.
    Lines { lo: u32, hi: u32 },
}

/// Merge the old and new sides of a diff, taking the **selected** changes from one side and
/// leaving everything else as the other: with `keep_selected`, selected changes are applied onto
/// `old` (staging); without, selected changes are *excluded* from `new` (unstaging, reverting).
/// Unchanged regions are identical either way.
///
/// Returns `None` when the selection covers no change at all — the caller reports "nothing here"
/// rather than rewriting content with a no-op.
///
/// Both sides are treated as LF text (the baselines are stored LF-normalized and buffers are LF
/// internally); a missing trailing newline is preserved exactly, and gets repaired to `\n` when
/// content is spliced in after a final newline-less line.
pub fn merge_selected(
    old: &[u8],
    new: &[u8],
    sel: &HunkSelection,
    keep_selected: bool,
) -> Option<Vec<u8>> {
    let hunks = hunks_from_buffers(old, new);
    let new_content_lines = content_line_count(new);

    // Bare-cursor mode resolves to exactly one hunk up front; no hunk under the cursor → None.
    let target: Option<usize> = match sel {
        HunkSelection::WholeHunkAt(line) => Some(
            hunks
                .iter()
                .position(|h| hunk_covers_line(h, *line, new_content_lines))?,
        ),
        HunkSelection::Lines { .. } => None,
    };

    let old_lines: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
    let new_lines: Vec<&[u8]> = new.split_inclusive(|&b| b == b'\n').collect();

    // Whether this hunk's removed block / one of its added lines is selected.
    let del_selected = |i: usize, h: &DiffHunk| match (sel, target) {
        (HunkSelection::WholeHunkAt(_), t) => t == Some(i),
        (HunkSelection::Lines { lo, hi }, _) => {
            deletion_covered(h.anchor_line, *lo, *hi, new_content_lines)
        }
    };
    let add_selected = |i: usize, line: u32| match (sel, target) {
        (HunkSelection::WholeHunkAt(_), t) => t == Some(i),
        (HunkSelection::Lines { lo, hi }, _) => *lo <= line && line <= *hi,
    };

    let mut emitted: Vec<&[u8]> = Vec::with_capacity(old_lines.len().max(new_lines.len()));
    let mut old_pos = 0usize;
    let mut any_selected = false;
    for (i, h) in hunks.iter().enumerate() {
        // Lines untouched by any hunk are common to both sides; emit them from the old side.
        emitted.extend(&old_lines[old_pos..h.old_start as usize]);
        old_pos = h.old_start as usize;

        let removed = h.deleted.len();
        if removed > 0 {
            let selected = del_selected(i, h);
            any_selected |= selected;
            // The removal is applied when (selected ⊕ !keep_selected); otherwise the old lines
            // survive. Slices come from `old` itself, not `h.deleted`, to preserve exact newlines.
            if selected != keep_selected {
                emitted.extend(&old_lines[old_pos..old_pos + removed]);
            }
            old_pos += removed;
        }
        for line in h.anchor_line..h.anchor_line + h.new_lines {
            let selected = add_selected(i, line);
            any_selected |= selected;
            if selected == keep_selected {
                emitted.push(new_lines[line as usize]);
            }
        }
    }
    emitted.extend(&old_lines[old_pos..]);

    if !any_selected {
        return None;
    }

    // Join, repairing a missing `\n` on any line that is no longer final (only an original final
    // line can lack one).
    let mut out = Vec::with_capacity(old.len().max(new.len()));
    for (i, line) in emitted.iter().enumerate() {
        out.extend_from_slice(line);
        if i + 1 < emitted.len() && !line.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    Some(out)
}

/// Whether a bare cursor on `line` addresses this hunk.
fn hunk_covers_line(h: &DiffHunk, line: u32, new_content_lines: u32) -> bool {
    if h.new_lines > 0 {
        h.anchor_line <= line && line < h.anchor_line + h.new_lines
    } else {
        deletion_covered(h.anchor_line, line, line, new_content_lines)
    }
}

/// Whether the inclusive line span `[lo, hi]` covers a removed block anchored at `anchor`. The
/// block belongs to the line it renders above; an end-of-buffer deletion has no line below it, so
/// it belongs to the last content line instead (a cursor on the trailing empty line also counts).
fn deletion_covered(anchor: u32, lo: u32, hi: u32, new_content_lines: u32) -> bool {
    if anchor >= new_content_lines {
        hi + 1 >= new_content_lines
    } else {
        lo <= anchor && anchor <= hi
    }
}

/// Number of content lines in `bytes` — what `split_inclusive` yields, i.e. a trailing newline
/// does **not** open a final empty line (unlike ropey's `len_lines`).
fn content_line_count(bytes: &[u8]) -> u32 {
    bytes.split_inclusive(|&b| b == b'\n').count() as u32
}

/// Map a 0-based line in new-side coordinates to the old side, given the hunks between them.
/// A line inside a changed region clamps to the region's old span — its start, or its end with
/// `round_up` — so mapping a span's endpoints covers every old line the span overlaps. Used to
/// carry a buffer-coordinate selection into index coordinates for unstaging.
pub fn map_line_to_old(hunks: &[DiffHunk], line: u32, round_up: bool) -> u32 {
    let mut shift: i64 = 0; // new minus old, accumulated over hunks fully above `line`
    for h in hunks {
        if line < h.anchor_line {
            break;
        }
        let removed = h.deleted.len() as i64;
        if h.new_lines > 0 && line < h.anchor_line + h.new_lines {
            return if round_up {
                (h.old_start as i64 + (removed - 1).max(0)) as u32
            } else {
                h.old_start
            };
        }
        shift += h.new_lines as i64 - removed;
    }
    ((line as i64) - shift).max(0) as u32
}

/// The inverse of [`map_line_to_old`]: map a 0-based old-side line to new-side coordinates. A
/// line inside a changed region clamps to the region's new span (start, or end with `round_up`;
/// a pure deletion has no new span, so both clamp onto its anchor). Used to place the staged
/// (HEAD→index) hunks — which live in index coordinates — onto buffer lines, across the
/// unstaged (index→buffer) diff.
pub fn map_line_to_new(hunks: &[DiffHunk], line: u32, round_up: bool) -> u32 {
    let mut shift: i64 = 0; // new minus old, accumulated over hunks fully above `line`
    for h in hunks {
        let removed = h.deleted.len() as u32;
        if line < h.old_start {
            break;
        }
        if removed > 0 && line < h.old_start + removed {
            return if round_up && h.new_lines > 0 {
                h.anchor_line + h.new_lines - 1
            } else {
                h.anchor_line
            };
        }
        shift += h.new_lines as i64 - removed as i64;
    }
    ((line as i64) + shift).max(0) as u32
}

/// Compose the combined staged+unstaged hunk list (what the gutter / inline diff renders): the unstaged (index→buffer) hunks verbatim,
/// plus the staged (HEAD→index) hunks carried from index into buffer coordinates and tagged
/// `Staged`. Sorted by anchor with staged-first ties, so phantom rows sharing an anchor stack
/// oldest layer (HEAD's text) on top. Per-line classification falls out exactly: a buffer line in
/// an unstaged hunk is unstaged whatever sits beneath it; a staged hunk's span maps onto the
/// buffer lines it still corresponds to, clamped to the enclosing unstaged block where the region
/// was re-modified (those lines then read as plain unstaged — the top layer wins).
pub fn compose_both(staged: &[DiffHunk], unstaged: &[DiffHunk]) -> Vec<DiffHunk> {
    let mut out: Vec<DiffHunk> = Vec::with_capacity(staged.len() + unstaged.len());
    for h in staged {
        let mut mapped = h.clone();
        mapped.stage = DiffStage::Staged;
        if h.new_lines > 0 {
            let start = map_line_to_new(unstaged, h.anchor_line, false);
            let end = map_line_to_new(unstaged, h.anchor_line + h.new_lines - 1, true);
            mapped.anchor_line = start;
            mapped.new_lines = end.saturating_sub(start) + 1;
        } else {
            // A staged pure deletion anchors above an index line; its buffer anchor is wherever
            // that line ended up (or the unstaged block that replaced it).
            mapped.anchor_line = map_line_to_new(unstaged, h.anchor_line, false);
        }
        out.push(mapped);
    }
    out.extend(unstaged.iter().cloned());
    // Stable sort: equal anchors keep staged (pushed first) ahead of unstaged.
    out.sort_by_key(|h| h.anchor_line);
    out
}

/// Replace the file's index (staged) entry with `content`, creating the entry when the file is
/// untracked. The caller is responsible for line-ending fidelity (CRLF files want CRLF content —
/// the in-memory baselines are LF-normalized). `None` on any libgit2 failure.
pub fn write_index_blob(repo: &GitRepo, content: &[u8]) -> Option<()> {
    let git_repo = git2::Repository::open(&repo.workdir).ok()?;
    let mut index = git_repo.index().ok()?;
    let entry = match index.get_path(&repo.rel_path, 0) {
        Some(e) => e,
        // Untracked: a minimal regular-file entry. `add_frombuffer` fills in the blob id; the
        // zeroed stat fields just force git to content-compare against the working tree.
        None => git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100_644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: git2::Oid::ZERO_SHA1,
            flags: 0,
            flags_extended: 0,
            path: repo.rel_path.to_str()?.as_bytes().to_vec(),
        },
    };
    index.add_frombuffer(&entry, content).ok()?;
    index.write().ok()?;
    Some(())
}

/// LF → CRLF, for writing index content of a file that was loaded with CRLF endings (the inverse
/// of [`normalize_lf`], mirroring `Buffer::save_to_disk`).
pub fn denormalize_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
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
/// bucket rather than a walk of thousands of files. The one status that does NOT aggregate is
/// `Ignored`: it only colours the entry it names — a tracked folder containing ignored
/// descendants isn't itself ignored.
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
        let Ok(path) = entry.path() else { continue };
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
        if status == GitStatus::Ignored {
            // Unlike real changes, ignored-ness never aggregates upward: a clean tracked
            // folder whose only status entries are ignored *descendants* (`__pycache__/`
            // somewhere beneath it — clean files produce no entries at all) is not itself
            // ignored, and greying it reads as "this folder is gitignored". Only the entry
            // that IS the listed child colours.
            if suffix.components().nth(1).is_some() {
                continue;
            }
            // libgit2 reports two different things as IGNORED that `is_path_ignored` (a
            // rule-by-path query) disagrees with, because it won't consult a `.gitignore`
            // *inside* the directory: (a) bare untracked *empty* dirs git can't track, which
            // we must NOT grey, and (b) self-ignoring dirs whose only ignore rule is a
            // contained `.gitignore` of `*` (pytest/ruff caches), which we DO want greyed.
            // Tell them apart by emptiness: drop only the empty-no-rule false positive.
            if !repo.is_path_ignored(Path::new(path)).unwrap_or(true)
                && dir_is_empty(&canonical.join(child))
            {
                continue;
            }
        }
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

/// Whether `path` is a directory with no entries. A non-directory or unreadable path counts as
/// non-empty so callers fall back to their default (keeping a flagged entry rather than dropping
/// it). Short-circuits on the first entry — never a full walk.
fn dir_is_empty(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

/// A repo's per-file status, scoped to one workspace root, for the Files picker. Holds the root's
/// own path within the repo plus a `repo-relative path → status` map, so a file's status is a
/// single lookup keyed by its root-relative path (no per-file repo discovery or canonicalisation).
pub struct RepoStatus {
    /// The workspace root's path relative to the repo workdir (empty when the root *is* the repo
    /// root). Joined with a file's root-relative path to form its repo-relative key.
    root_rel: PathBuf,
    map: HashMap<PathBuf, GitStatus>,
}

impl RepoStatus {
    /// Status of a file given its path relative to the workspace root (forward-slash separated, as
    /// stored in the workspace index). `None` when the file is clean.
    pub fn status_of(&self, root_rel_path: &str) -> Option<GitStatus> {
        self.map.get(&self.root_rel.join(root_rel_path)).copied()
    }
}

/// Resolve the Git status of every changed file under `root` in one `statuses()` pass, for the
/// Files picker. Untracked directories are recursed so each untracked file is reported
/// individually (the picker colours individual files); ignored files are excluded — the workspace
/// walker already skips them. Best-effort: `None` when `root` isn't in a repo or any libgit2 call
/// fails.
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
        if let Ok(path) = entry.path() {
            if let Some(status) = classify_status(entry.status()) {
                map.insert(PathBuf::from(path), status);
            }
        }
    }
    Some(RepoStatus { root_rel, map })
}

/// One changed file under a root: its combined staged+unstaged hunks vs HEAD (anchor order) plus
/// the LF-normalized working-tree bytes, so the caller can pull each add/modify hunk's preview
/// line without re-reading the file. `rel_path` is root-relative, forward-slash.
pub struct ChangedFile {
    pub rel_path: String,
    pub hunks: Vec<DiffHunk>,
    pub working: Vec<u8>,
    /// True when the file has no committed (HEAD) blob *and* no index blob — i.e. wholly untracked.
    /// A staged-new file has an index blob, so it reads as tracked (`false`). Used by the
    /// Git-changes picker's `hide_untracked` filter.
    pub untracked: bool,
}

/// Diff every changed file under `root` against HEAD (combined staged+unstaged), opening the repo
/// **once** — discovery, the HEAD tree, and the index are resolved a single time and reused for
/// every file, instead of re-discovering the repo per file (the slow part when a workspace has many
/// changes). Untracked directories are not recursed: a wholly-new directory collapses to one entry
/// (git's default `git status`), which is a directory and skipped — only individual changed files
/// are diffable. Files with no net change are dropped. Best-effort: empty on any libgit2 error.
pub fn changed_files_with_hunks(root: &Path) -> Vec<ChangedFile> {
    let mut out = Vec::new();
    let Ok(canonical) = root.canonicalize() else {
        return out;
    };
    let Ok(repo) = git2::Repository::discover(&canonical) else {
        return out;
    };
    let Some(workdir) = repo.workdir().and_then(|w| w.canonicalize().ok()) else {
        return out;
    };
    let Ok(root_rel) = canonical.strip_prefix(&workdir) else {
        return out;
    };
    let root_rel = root_rel.to_path_buf();

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return out;
    };

    // Resolve HEAD's tree and the index once; every file's baseline reads from these.
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let index = repo.index().ok();

    for entry in statuses.iter() {
        let Ok(path) = entry.path() else { continue };
        // A collapsed untracked directory (recurse off) reports with a trailing slash — not a
        // diffable file.
        if path.ends_with('/') {
            continue;
        }
        if classify_status(entry.status()).is_none() {
            continue;
        }
        let repo_rel = Path::new(path);
        let Ok(rel) = repo_rel.strip_prefix(&root_rel) else {
            continue; // a change in another root of the same repo
        };
        let rel_path: String = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel_path.is_empty() {
            continue;
        }

        // HEAD + index blobs straight from the already-open repo (no per-file discovery).
        let head = head_tree
            .as_ref()
            .and_then(|t| t.get_path(repo_rel).ok())
            .and_then(|e| e.to_object(&repo).ok())
            .and_then(|o| o.peel_to_blob().ok())
            .map(|b| normalize_lf(b.content().to_vec()));
        let index_blob = index
            .as_ref()
            .and_then(|ix| ix.get_path(repo_rel, 0))
            .and_then(|e| repo.find_blob(e.id).ok())
            .map(|b| normalize_lf(b.content().to_vec()));
        // Working-tree side: live disk content, or empty for a deleted file (diffs as a deletion).
        let working = std::fs::read(workdir.join(repo_rel))
            .map(normalize_lf)
            .unwrap_or_default();

        let staged = hunks_from_buffers(
            head.as_deref().unwrap_or(b""),
            index_blob.as_deref().unwrap_or(b""),
        );
        let unstaged = hunks_from_buffers(index_blob.as_deref().unwrap_or(b""), &working);
        let both = compose_both(&staged, &unstaged);
        if both.is_empty() {
            continue;
        }
        out.push(ChangedFile {
            rel_path,
            hunks: both,
            working,
            untracked: head.is_none() && index_blob.is_none(),
        });
    }
    out
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
        assert_eq!(
            h.anchor_line, 1,
            "deleted block renders above 0-based line 1 (d)"
        );
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

    // ---- merge_selected (stage / unstage / revert core) -----------------------------------------

    fn merged_str(old: &str, new: &str, sel: HunkSelection, keep: bool) -> Option<String> {
        merge_selected(old.as_bytes(), new.as_bytes(), &sel, keep)
            .map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn merge_stages_whole_hunk_under_cursor() {
        // Two hunks; cursor on the first only stages the first.
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nextra\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(1), true).unwrap();
        assert_eq!(
            got, "a\nB\nc\n",
            "modification staged, trailing addition left out"
        );
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(3), true).unwrap();
        assert_eq!(
            got, "a\nb\nc\nextra\n",
            "addition staged, modification left out"
        );
    }

    #[test]
    fn merge_no_hunk_under_cursor_is_none() {
        assert!(merged_str("a\nb\n", "a\nB\n", HunkSelection::WholeHunkAt(0), true).is_none());
        // Identical sides: nothing anywhere.
        assert!(merged_str("a\n", "a\n", HunkSelection::WholeHunkAt(0), true).is_none());
    }

    #[test]
    fn merge_stages_line_subset_of_added_block() {
        // Lines x,y,z added; selecting y..z stages just those.
        let old = "a\n";
        let new = "a\nx\ny\nz\n";
        let got = merged_str(old, new, HunkSelection::Lines { lo: 2, hi: 3 }, true).unwrap();
        assert_eq!(got, "a\ny\nz\n");
    }

    #[test]
    fn merge_selection_not_touching_any_hunk_is_none() {
        let got = merged_str(
            "a\nb\nc\n",
            "a\nb\nC\n",
            HunkSelection::Lines { lo: 0, hi: 1 },
            true,
        );
        assert!(got.is_none());
    }

    #[test]
    fn merge_stages_deletion_via_anchor_line() {
        // b removed; the deletion belongs to the line below (c, buffer line 1).
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(1), true).unwrap();
        assert_eq!(got, "a\nc\n");
        // Cursor on `a` does not address it.
        assert!(merged_str(old, new, HunkSelection::WholeHunkAt(0), true).is_none());
    }

    #[test]
    fn merge_stages_eof_deletion_from_last_content_line() {
        // Trailing b removed: anchored past the last content line, so the last line owns it.
        let old = "a\nb\n";
        let new = "a\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(0), true).unwrap();
        assert_eq!(got, "a\n");
        // A line-span selection ending on the last content line also covers it.
        let got = merged_str(old, new, HunkSelection::Lines { lo: 0, hi: 0 }, true).unwrap();
        assert_eq!(got, "a\n");
    }

    #[test]
    fn merge_unselected_takes_new_side_when_not_keeping() {
        // Revert/unstage orientation: selected hunks roll back to old, others keep the new side.
        let old = "a\nb\nc\n";
        let new = "A\nb\nC\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(0), false).unwrap();
        assert_eq!(got, "a\nb\nC\n", "first hunk reverted, second untouched");
    }

    #[test]
    fn merge_revert_reinserts_deleted_block() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nd\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(1), false).unwrap();
        assert_eq!(got, "a\nb\nc\nd\n");
    }

    #[test]
    fn merge_repairs_missing_trailing_newline_when_splicing_after_it() {
        // Old final line has no newline; staging only the added line after it must not glue them.
        let old = "a";
        let new = "a\nb\n";
        // The diff reads this as a modification of `a` plus addition — select only line 1 (`b`).
        let got = merged_str(old, new, HunkSelection::Lines { lo: 1, hi: 1 }, true).unwrap();
        assert!(
            got == "a\nb\n" || got == "a\nb",
            "lines must stay separate, got {got:?}"
        );
    }

    #[test]
    fn merge_preserves_missing_trailing_newline_on_revert() {
        let old = "a\nb"; // no trailing newline
        let new = "a\n";
        let got = merged_str(old, new, HunkSelection::WholeHunkAt(0), false).unwrap();
        assert_eq!(got, "a\nb", "exact baseline bytes restored");
    }

    // ---- map_line_to_old ------------------------------------------------------------------------

    #[test]
    fn map_line_shifts_past_hunks_and_clamps_inside() {
        // old: a b c d e ; new: a X Y c e   (b -> X,Y modified; d deleted)
        let old = b"a\nb\nc\nd\ne\n";
        let new = b"a\nX\nY\nc\ne\n";
        let hunks = hunks_from_buffers(old, new);
        assert_eq!(map_line_to_old(&hunks, 0, false), 0, "before any hunk");
        assert_eq!(
            map_line_to_old(&hunks, 1, false),
            1,
            "inside hunk clamps to old start"
        );
        assert_eq!(
            map_line_to_old(&hunks, 2, true),
            1,
            "round_up clamps to old end"
        );
        assert_eq!(
            map_line_to_old(&hunks, 3, false),
            2,
            "after +1 hunk shifts back"
        );
        assert_eq!(
            map_line_to_old(&hunks, 4, false),
            4,
            "after the deletion shifts forward"
        );
    }

    // ---- map_line_to_new / compose_both (combined staged+unstaged view) -------------------------

    #[test]
    fn map_line_to_new_shifts_and_clamps() {
        // old: a b c d e ; new: a X Y c e   (b -> X,Y modified; d deleted above e)
        let hunks = hunks_from_buffers(b"a\nb\nc\nd\ne\n", b"a\nX\nY\nc\ne\n");
        assert_eq!(map_line_to_new(&hunks, 0, false), 0, "before any hunk");
        assert_eq!(
            map_line_to_new(&hunks, 1, false),
            1,
            "inside clamps to new start"
        );
        assert_eq!(
            map_line_to_new(&hunks, 1, true),
            2,
            "round_up clamps to new end"
        );
        assert_eq!(
            map_line_to_new(&hunks, 2, false),
            3,
            "after a +1 hunk shifts forward"
        );
        assert_eq!(
            map_line_to_new(&hunks, 3, false),
            4,
            "deleted old line clamps to its anchor"
        );
        assert_eq!(
            map_line_to_new(&hunks, 4, false),
            4,
            "after the deletion shifts back"
        );
    }

    #[test]
    fn compose_keeps_disjoint_hunks_in_order() {
        // HEAD a b c ; index a B c (staged mod) ; buffer a B c d (unstaged add)
        let staged = hunks_from_buffers(b"a\nb\nc\n", b"a\nB\nc\n");
        let unstaged = hunks_from_buffers(b"a\nB\nc\n", b"a\nB\nc\nd\n");
        let both = compose_both(&staged, &unstaged);
        assert_eq!(both.len(), 2);
        assert_eq!((both[0].anchor_line, both[0].stage), (1, DiffStage::Staged));
        assert_eq!(
            (both[1].anchor_line, both[1].stage),
            (3, DiffStage::Unstaged)
        );
    }

    #[test]
    fn compose_clamps_remodified_staged_hunk_onto_unstaged_block() {
        // HEAD a b c ; index a B c ; buffer a Z c — line 1 staged then modified again.
        let staged = hunks_from_buffers(b"a\nb\nc\n", b"a\nB\nc\n");
        let unstaged = hunks_from_buffers(b"a\nB\nc\n", b"a\nZ\nc\n");
        let both = compose_both(&staged, &unstaged);
        assert_eq!(both.len(), 2);
        // Staged-first at the shared anchor, both covering buffer line 1.
        assert_eq!(
            (both[0].anchor_line, both[0].new_lines, both[0].stage),
            (1, 1, DiffStage::Staged)
        );
        assert_eq!(
            (both[1].anchor_line, both[1].new_lines, both[1].stage),
            (1, 1, DiffStage::Unstaged)
        );
    }

    #[test]
    fn compose_carries_staged_deletion_anchor_across_unstaged_insert() {
        // HEAD a b c ; index a c (staged deletion of b, anchored above index line 1) ;
        // buffer x a c — an unstaged line added at the top pushes the anchor to buffer line 2.
        let staged = hunks_from_buffers(b"a\nb\nc\n", b"a\nc\n");
        let unstaged = hunks_from_buffers(b"a\nc\n", b"x\na\nc\n");
        let both = compose_both(&staged, &unstaged);
        let staged_del = both.iter().find(|h| h.stage == DiffStage::Staged).unwrap();
        assert_eq!(staged_del.kind, ChangeKind::Deleted);
        assert_eq!(
            staged_del.anchor_line, 2,
            "anchor shifted by the unstaged insert above"
        );
        assert_eq!(staged_del.deleted, vec!["b".to_string()]);
    }

    #[test]
    fn compose_preserves_eof_staged_deletion_anchor() {
        // HEAD a b ; index a (staged EOF deletion, anchor past the last content line) ; buffer a.
        let staged = hunks_from_buffers(b"a\nb\n", b"a\n");
        let unstaged: Vec<DiffHunk> = Vec::new();
        let both = compose_both(&staged, &unstaged);
        assert_eq!(both.len(), 1);
        assert_eq!(
            both[0].anchor_line, 1,
            "EOF anchor preserved (past last content line)"
        );
        assert_eq!(both[0].stage, DiffStage::Staged);
    }

    // ---- write_index_blob -----------------------------------------------------------------------

    #[test]
    fn write_index_blob_updates_tracked_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\n");
        let repo = load_baseline(&file).repo.expect("repo resolved");

        write_index_blob(&repo, b"one\nTWO\n").expect("index write");

        let baseline = load_baseline(&file);
        assert_eq!(baseline.index_blob.as_deref(), Some(&b"one\nTWO\n"[..]));
        assert_eq!(
            baseline.blob.as_deref(),
            Some(&b"one\ntwo\n"[..]),
            "HEAD untouched"
        );
        assert_eq!(
            baseline.staged_hunks.len(),
            1,
            "staged diff now has the change"
        );
    }

    #[test]
    fn write_index_blob_creates_entry_for_untracked_file() {
        let dir = tempfile::tempdir().unwrap();
        // Repo with one commit so HEAD exists, plus an untracked file.
        repo_with_committed_file(dir.path(), "other.rs", "x\n");
        let file = dir.path().join("new.rs");
        std::fs::write(&file, "hello\n").unwrap();
        let repo = load_baseline(&file).repo.expect("repo resolved");
        assert!(
            load_baseline(&file).index_blob.is_none(),
            "untracked → no entry yet"
        );

        write_index_blob(&repo, b"hello\n").expect("index write");

        let baseline = load_baseline(&file);
        assert_eq!(baseline.index_blob.as_deref(), Some(&b"hello\n"[..]));
        assert!(baseline.blob.is_none(), "still not in HEAD");
    }

    #[test]
    fn denormalize_crlf_round_trips_normalize() {
        let crlf = b"one\r\ntwo\r\n".to_vec();
        let lf = normalize_lf(crlf.clone());
        assert_eq!(denormalize_crlf(&lf), crlf);
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
        let tree = repo
            .find_tree(index.write_tree().expect("write_tree"))
            .unwrap();
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
        assert_eq!(
            st.get("clean.rs"),
            None,
            "unchanged tracked file is uncoloured"
        );
        assert_eq!(st.get("mod.rs"), Some(&GitStatus::Modified));
        assert_eq!(
            st.get("sub"),
            Some(&GitStatus::Modified),
            "folder inherits a descendant's change (aggregation)"
        );
        assert_eq!(st.get("new.rs"), Some(&GitStatus::Untracked));
        assert_eq!(st.get("debug.log"), Some(&GitStatus::Ignored));
    }

    /// libgit2 reports untracked empty directories as IGNORED; a tracked folder containing
    /// one must not grey out (no ignore rule matches it).
    #[test]
    fn dir_statuses_ignores_empty_dir_false_positives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(root, &[("sub/code.rs", "fine\n")]);
        std::fs::create_dir(root.join("sub/empty")).unwrap();
        std::fs::create_dir(root.join("hollow")).unwrap();

        let st = dir_statuses(root);
        assert_eq!(
            st.get("sub"),
            None,
            "clean tracked folder with an empty subdir stays uncoloured"
        );
        assert_eq!(
            st.get("hollow"),
            None,
            "a bare empty directory is not 'ignored'"
        );

        // A real ignore rule still reports — including for directories.
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir(root.join("build")).unwrap();
        std::fs::write(root.join("build/out.o"), "obj\n").unwrap();
        let st = dir_statuses(root);
        assert_eq!(st.get("build"), Some(&GitStatus::Ignored));
    }

    /// A directory ignored only by a `.gitignore` *inside itself* (a single `*`, as pytest and
    /// ruff write into their cache dirs) must grey out. libgit2 reports it as IGNORED but
    /// `is_path_ignored` returns false — no ancestor rule names it — so the entry is kept on the
    /// strength of being non-empty, distinguishing it from an empty-dir false positive.
    #[test]
    fn dir_statuses_self_ignoring_dir_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(root, &[("code.rs", "fine\n")]);
        // The pytest/ruff idiom: the cache dir carries its own `.gitignore` of `*`.
        std::fs::create_dir(root.join(".pytest_cache")).unwrap();
        std::fs::write(root.join(".pytest_cache/.gitignore"), "*\n").unwrap();
        std::fs::write(root.join(".pytest_cache/CACHEDIR.TAG"), "x").unwrap();

        let st = dir_statuses(root);
        assert_eq!(
            st.get(".pytest_cache"),
            Some(&GitStatus::Ignored),
            "a dir ignored by its own contained .gitignore greys out"
        );
        assert_eq!(st.get("code.rs"), None);
    }

    /// The `__pycache__` case: a clean tracked folder whose only status entries are ignored
    /// *descendants* must not grey — ignored-ness doesn't aggregate upward (clean files
    /// produce no status entries, so the ignored ones would otherwise win the bucket).
    #[test]
    fn dir_statuses_ignored_descendants_dont_grey_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(
            root,
            &[
                ("databricks/src/main.py", "code\n"),
                (".gitignore", "__pycache__/\n"),
            ],
        );
        std::fs::create_dir_all(root.join("databricks/src/__pycache__")).unwrap();
        std::fs::write(root.join("databricks/src/__pycache__/main.pyc"), "x").unwrap();

        let st = dir_statuses(root);
        assert_eq!(
            st.get("databricks"),
            None,
            "clean folder with only-ignored descendants stays uncoloured"
        );

        // Listing where the ignored directory is an immediate child: it does grey there.
        let st = dir_statuses(&root.join("databricks/src"));
        assert_eq!(st.get("__pycache__"), Some(&GitStatus::Ignored));
        assert_eq!(st.get("main.py"), None);
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
        assert_eq!(
            st.get("top.rs"),
            None,
            "a sibling outside the listed dir is absent"
        );
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
        repo_with_files(root, &[("clean.rs", "clean\n"), ("sub/mod.rs", "before\n")]);
        std::fs::write(root.join("sub/mod.rs"), "after\n").unwrap(); // modified, nested
        std::fs::write(root.join("new.rs"), "new\n").unwrap(); // untracked at root

        let rs = repo_status_for_root(root).expect("root is in a repo");
        // Keyed by the path relative to the workspace root (which == repo root here).
        assert_eq!(rs.status_of("clean.rs"), None, "clean file has no status");
        assert_eq!(rs.status_of("sub/mod.rs"), Some(GitStatus::Modified));
        assert_eq!(rs.status_of("new.rs"), Some(GitStatus::Untracked));
    }

    #[test]
    fn repo_status_for_root_keys_relative_to_a_subdir_root() {
        // When the workspace root is a subdirectory of the repo, lookups are still keyed by the
        // path relative to that root — the repo-relative prefix is handled internally.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        repo_with_files(
            repo_root,
            &[("pkg/mod.rs", "before\n"), ("top.rs", "top\n")],
        );
        std::fs::write(repo_root.join("pkg/mod.rs"), "after\n").unwrap();

        let rs = repo_status_for_root(&repo_root.join("pkg")).expect("subdir is in the repo");
        assert_eq!(rs.status_of("mod.rs"), Some(GitStatus::Modified));
    }

    // ---- changed_files_with_hunks (Git-changes picker) ------------------------------------------

    #[test]
    fn changed_files_with_hunks_diffs_each_file_and_collapses_untracked_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(root, &[("a.rs", "one\ntwo\nthree\n"), ("clean.rs", "x\n")]);
        std::fs::write(root.join("a.rs"), "one\nTWO\nthree\n").unwrap(); // a modification
                                                                         // A wholly-new directory with several files (must collapse), plus a lone new file.
        std::fs::create_dir_all(root.join("junk")).unwrap();
        std::fs::write(root.join("junk/x.rs"), "x\n").unwrap();
        std::fs::write(root.join("junk/y.rs"), "y\n").unwrap();
        std::fs::write(root.join("loose.rs"), "new\n").unwrap();

        let mut changed = changed_files_with_hunks(root);
        changed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let paths: Vec<&str> = changed.iter().map(|c| c.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["a.rs", "loose.rs"],
            "the modification and the lone new file, but nothing inside junk/"
        );

        // The modification carries one Modified hunk on line 1 with the old text recorded.
        let a = &changed[0];
        assert_eq!(a.hunks.len(), 1);
        assert_eq!(a.hunks[0].kind, ChangeKind::Modified);
        assert_eq!(a.hunks[0].anchor_line, 1);
        assert_eq!(a.hunks[0].deleted, vec!["two".to_string()]);
        assert_eq!(a.working, b"one\nTWO\nthree\n");

        // The lone new file is a whole-file addition.
        let loose = &changed[1];
        assert_eq!(loose.hunks.len(), 1);
        assert_eq!(loose.hunks[0].kind, ChangeKind::Added);
    }

    #[test]
    fn changed_files_with_hunks_no_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        assert!(changed_files_with_hunks(dir.path()).is_empty());
    }

    #[test]
    fn repo_status_for_root_no_repo_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_status_for_root(dir.path()).is_none());
    }
}
