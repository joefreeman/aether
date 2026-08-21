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

use aether_protocol::git::{
    BlameInfo, CommitInfo, ConflictSide, GitHead, GitRepoOperation, GitStatus, GitUpstreamStatus,
};
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

/// Which revision a repo is diffed against instead of HEAD (`git/set_baseline`), keyed by
/// canonicalized workdir. Threaded into [`load_baseline`] because the repo a path belongs to is
/// only known *after* discovery, so the caller can't look the override up in advance.
pub type BaselineRevs = HashMap<PathBuf, BaselineRev>;

/// A repo's non-HEAD diff baseline: what the user asked for, and the commit it resolved to.
///
/// The commit is **pinned at set time** rather than re-resolved per file. `git diff main`
/// re-resolves, but a gutter is ambient: having it shift under you mid-review because someone
/// pushed to `main` is worse than it going slightly stale, and pinning also means a ref deleted
/// while you're reading doesn't blank the comparison. `label` is kept for display so the status
/// bar can say `main` rather than a hash the user never typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineRev {
    /// What the user typed: `main`, `v1.0`, `HEAD~3`.
    pub label: String,
    /// The commit it resolved to, short form.
    pub commit: String,
}

/// Cached Git baseline for a buffer: where it lives in a repo (if anywhere) and the committed
/// content to diff against — HEAD normally, or a pinned revision when one is set for the repo.
/// Resolved on open and refreshed when HEAD changes — *not* on every edit.
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
    /// Divergence from the branch's upstream, cached here for the same reason `branch` is: it's a
    /// repo-level read that changes only when HEAD or a ref moves, which is exactly when this
    /// baseline is reloaded. A fetch writes `refs/remotes/**`, the watcher keys on `refs/`, and
    /// the refresh recomputes this — so the count follows a terminal `git fetch` too.
    pub upstream: Option<GitUpstreamStatus>,
    /// Staged diff (HEAD → index), computed once here since it's independent of the live buffer and
    /// only changes when HEAD or the index does (i.e. on the same refresh trigger as the blobs).
    pub staged_hunks: Vec<DiffHunk>,
    /// Set when this repo is being diffed against something other than HEAD. The staged/unstaged
    /// split is meaningless then — there's no index relationship to an arbitrary commit — so both
    /// blobs hold that commit's content and the whole change set reads as unstaged. See
    /// [`load_baseline`].
    pub rev: Option<BaselineRev>,
    /// A merge/rebase/cherry-pick the repo is stopped in the middle of. Cached here beside `branch`
    /// and `upstream` for the same reason: a repo-level read that changes when `.git` does, which
    /// is exactly when this baseline reloads — so a rebase started in a *terminal* reaches the
    /// status bar too.
    pub operation: Option<GitRepoOperation>,
    /// True when this file's repo is a **linked worktree** rather than the main checkout.
    ///
    /// Read here beside `branch` because it is the same kind of fact — a property of the checkout
    /// the file lives in, resolved while the repo is already open. The status bar needs it because
    /// the branch alone cannot say it: git allows one checkout per branch per family, so "on
    /// `test1`" looks identical whether that is the main tree or a worktree.
    pub worktree: bool,
    /// This *file* is left conflicted by that operation — it has index stages 1–3 and no stage 0.
    ///
    /// When true both blobs hold HEAD's content and `staged_hunks` is empty: nothing can be staged
    /// while the index holds conflict stages, and the whole change set reads as unstaged against
    /// HEAD — "what this merge commit will contain". The conflict blocks themselves are masked out
    /// of that diff by [`mask_conflicts`], so the diff and conflict decorations never share a line.
    /// See [`load_baseline`].
    pub conflicted: bool,
}

/// Resolve a path's repo and read its HEAD baseline. The expensive part — discovery plus reading
/// and decompressing the committed blob — so it runs on open and on external Git changes, never
/// per edit. Synchronous and `!Send`-clean (every libgit2 object is dropped before returning).
pub fn load_baseline(path: &Path, revs: &BaselineRevs) -> GitBaseline {
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

    // Read HEAD once and derive both views of it: the flattened label for the status bar, and the
    // upstream divergence. Both are repo-level rather than file-level, and both are wanted whether
    // or not a pinned baseline is in play — how far you are from `origin/main` is a fact about the
    // branch, not about what the gutter happens to be diffing against.
    let head = head_state(&repo);
    let branch = head.as_ref().map(branch_label);
    let upstream = head
        .as_ref()
        .and_then(|head| upstream_divergence(&repo, head));
    // Read while the repo is open. A stopped rebase detaches HEAD, so `branch` above is about to
    // become a bare hash and `upstream` `None` — this is the only thing that will explain why.
    let operation = state_operation(&repo);
    // Cheap (`git_repository_is_worktree` reads a flag set at open time) and asked for every
    // baseline, so it rides along rather than costing a second discovery later.
    let worktree = repo.is_worktree();

    // A conflicted file has no stage-0 entry, so the index blob reads as absent — which would make
    // the staged diff `HEAD → ""` and paint the whole file as a staged deletion. Point *both* blobs
    // at HEAD instead (the same move the pinned-revision branch below makes): the staged half comes
    // out empty, which is true — nothing is staged while the index holds conflict stages — and the
    // unstaged half becomes `HEAD → buffer`, which is exactly "what this merge commit will contain
    // for this file".
    //
    // That diff is meaningful everywhere *except* inside the conflict blocks, where it would only
    // be describing markers; those lines are masked out where the hunks are computed
    // (`mask_conflicts`), so the two decorations never land on the same line. Suppressing the whole
    // file instead would leave a file the user had just resolved by hand showing nothing at all.
    //
    // Checked before the pinned revision, in the rare case both apply: mid-merge, what the gutter
    // *would* have been comparing against is the least of what the user needs to know.
    if index_has_conflict(&repo, &rel_path) {
        let head = head_blob_bytes(&repo, &rel_path).map(normalize_lf);
        return GitBaseline {
            worktree,
            repo: Some(GitRepo { workdir, rel_path }),
            blob: head.clone(),
            index_blob: head,
            branch,
            upstream,
            operation,
            conflicted: true,
            ..Default::default()
        };
    }

    // Diffing against a pinned revision instead of HEAD: point *both* blobs at that commit's
    // content. The existing pipeline then produces exactly the right thing with no special cases
    // downstream — staged (blob → index) comes out empty, unstaged (index → buffer) is the whole
    // "changed since that commit" set, and the gutter, hunk navigation and revert all follow.
    // A file absent from that commit has no blob, so it reads as wholly added, which is true.
    if let Some(rev) = revs.get(&workdir) {
        let bytes = rev_blob_bytes(&repo, &rev.commit, &rel_path).map(normalize_lf);
        return GitBaseline {
            worktree,
            repo: Some(GitRepo { workdir, rel_path }),
            blob: bytes.clone(),
            index_blob: bytes,
            branch,
            upstream,
            staged_hunks: Vec::new(),
            rev: Some(rev.clone()),
            operation,
            conflicted: false,
        };
    }

    let blob = head_blob_bytes(&repo, &rel_path).map(normalize_lf);
    let index_blob = index_blob_bytes(&repo, &rel_path).map(normalize_lf);
    // Staged diff is HEAD → index; absent sides count as empty (a staged add has no HEAD side, a
    // staged whole-file delete has no index side).
    let staged_hunks = hunks_from_buffers(
        blob.as_deref().unwrap_or(b""),
        index_blob.as_deref().unwrap_or(b""),
    );
    GitBaseline {
        worktree,
        repo: Some(GitRepo { workdir, rel_path }),
        blob,
        index_blob,
        branch,
        upstream,
        staged_hunks,
        rev: None,
        operation,
        conflicted: false,
    }
}

/// `rel`'s content at `rev`, or `None` when the path doesn't exist there (or `rev` no longer
/// resolves — a pinned commit that has since been garbage-collected).
fn rev_blob_bytes(repo: &git2::Repository, rev: &str, rel: &Path) -> Option<Vec<u8>> {
    let tree = repo
        .revparse_single(rev)
        .ok()?
        .peel_to_commit()
        .ok()?
        .tree()
        .ok()?;
    let entry = tree.get_path(rel).ok()?;
    let blob = entry.to_object(repo).ok()?.peel_to_blob().ok()?;
    Some(blob.content().to_vec())
}

/// The paths currently in the index that differ from HEAD, with git's own status word — what
/// `git status` lists under "Changes to be committed", in the same vocabulary, so the commit
/// template reads like the terminal.
///
/// Index-side flags only: a file modified in the working tree but not staged is not going to be
/// committed and has no business in the message.
pub fn staged_files(workdir: &Path) -> Vec<(String, &'static str)> {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in statuses.iter() {
        let st = entry.status();
        // Order matters: a path can carry several index bits (staged-new then staged-renamed);
        // report the one git would name first.
        let word = if st.contains(git2::Status::INDEX_NEW) {
            "new file"
        } else if st.contains(git2::Status::INDEX_RENAMED) {
            "renamed"
        } else if st.contains(git2::Status::INDEX_DELETED) {
            "deleted"
        } else if st.contains(git2::Status::INDEX_TYPECHANGE) {
            "typechange"
        } else if st.contains(git2::Status::INDEX_MODIFIED) {
            "modified"
        } else {
            continue;
        };
        if let Ok(path) = entry.path() {
            out.push((path.to_string(), word));
        }
    }
    out.sort();
    out
}

/// The commits between `from` and `to` (exclusive of `to`), newest first — what a reset to `to`
/// would unwind, so the client can name what the user just took back.
///
/// Empty when `to` isn't an ancestor of `from` (a sideways reset onto a divergent branch): those
/// commits aren't "undone" in any sense worth reporting, and pretending otherwise would be worse
/// than saying nothing.
pub fn commits_between(workdir: &Path, from: &str, to: &str) -> Vec<CommitInfo> {
    let repo = GitRepo {
        workdir: workdir.to_path_buf(),
        rel_path: PathBuf::new(),
    };
    let Ok(git_repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let (Ok(from_oid), Ok(to_oid)) = (
        git_repo
            .revparse_single(from)
            .and_then(|o| o.peel_to_commit()),
        git_repo
            .revparse_single(to)
            .and_then(|o| o.peel_to_commit()),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut walk = from_oid;
    // Bounded: a reset that isn't a small step back is a mistake to report, not a history to
    // enumerate, so stop rather than walking a whole repo.
    for _ in 0..64 {
        if walk.id() == to_oid.id() {
            return out;
        }
        if let Some(info) = commit_info(&repo, &walk.id().to_string()) {
            out.push(info);
        }
        match walk.parent(0) {
            Ok(parent) => walk = parent,
            Err(_) => break,
        }
    }
    Vec::new()
}

/// HEAD's full commit message, for prefilling an amend. `None` on an unborn branch (nothing to
/// amend) or any libgit2 error.
pub fn head_message(workdir: &Path) -> Option<String> {
    let repo = git2::Repository::open(workdir).ok()?;
    let commit = repo.head().ok()?.peel_to_commit().ok()?;
    commit.message().ok().map(|m| m.to_string())
}

/// Resolve `rev` (a branch, tag, hash, `HEAD~3`, …) to a short commit hash, for pinning at
/// `git/set_baseline` time. `None` when it names nothing in this repo.
pub fn resolve_rev(workdir: &Path, rev: &str) -> Option<String> {
    let repo = git2::Repository::open(workdir).ok()?;
    let commit = repo.revparse_single(rev).ok()?.peel_to_commit().ok()?;
    let s = commit.id().to_string();
    Some(s[..7.min(s.len())].to_string())
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

/// Branch name, or a short commit hash when HEAD is detached — the flattened, display-oriented
/// view of [`head_state`], which is what the status bar wants. Anything that needs to *act* on
/// HEAD wants the three-way state instead.
fn branch_label(head: &GitHead) -> String {
    match head {
        GitHead::Branch { name, .. } | GitHead::Unborn { name } => name.clone(),
        GitHead::Detached { oid } => oid.clone(),
    }
}

/// How far HEAD's branch has diverged from its configured upstream.
///
/// **Local refs only** — `graph_ahead_behind` is a revwalk between two commits already in the
/// object store, so this costs no network and says nothing about the remote's current state. It is
/// the same comparison `git status` prints, and it goes stale in exactly the same way: only a
/// fetch moves the upstream ref.
///
/// `None` whenever there is nothing to compare against — a detached or unborn HEAD, a branch with
/// no upstream configured, or an upstream ref that no longer resolves (a deleted remote branch that
/// hasn't been pruned). Those are distinct from "level with upstream", which is `Some` with zeros.
fn upstream_divergence(repo: &git2::Repository, head: &GitHead) -> Option<GitUpstreamStatus> {
    let GitHead::Branch {
        name,
        upstream: Some(upstream_name),
    } = head
    else {
        return None;
    };
    let branch = repo.find_branch(name, git2::BranchType::Local).ok()?;
    let local_oid = branch.get().target()?;
    let upstream_oid = branch.upstream().ok()?.get().target()?;
    let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid).ok()?;
    Some(GitUpstreamStatus {
        name: upstream_name.clone(),
        ahead: ahead as u32,
        behind: behind as u32,
    })
}

/// Where HEAD points, as the three cases that admit different operations (see [`GitHead`]).
/// `None` only when HEAD can't be read at all.
///
/// The unborn case is checked *last*: a repo with commits has a resolvable `head()`, and only a
/// fresh one falls through to reading HEAD's symbolic target directly.
pub(crate) fn head_state(repo: &git2::Repository) -> Option<GitHead> {
    if let Ok(head) = repo.head() {
        if head.is_branch() {
            let name = head.shorthand().ok()?.to_string();
            // The upstream lookup is best-effort and expected to fail for a never-pushed branch;
            // `None` there is the "push needs --set-upstream" signal, not an error.
            let upstream = repo
                .find_branch(&name, git2::BranchType::Local)
                .ok()
                .and_then(|b| b.upstream().ok())
                .and_then(|u| u.name().ok().flatten().map(String::from));
            return Some(GitHead::Branch { name, upstream });
        }
        if let Some(oid) = head.target() {
            let s = oid.to_string();
            return Some(GitHead::Detached {
                oid: s[..7.min(s.len())].to_string(),
            });
        }
    }
    // Unborn branch: HEAD is a symbolic ref to a branch that has no commit yet.
    let name = repo
        .find_reference("HEAD")
        .ok()?
        .symbolic_target()
        .ok()
        .flatten()
        .and_then(|t| t.strip_prefix("refs/heads/").map(String::from))?;
    Some(GitHead::Unborn { name })
}

/// A repo's identity and current HEAD — everything `git/repos` reports about one repo except which
/// workspace roots reached it (the caller knows that, this doesn't).
///
/// `workdir` is the [`RepoId`]; see there for why identity is the working directory and not the
/// git dir. `git_dir` and `common_dir` differ only for a linked worktree, and that difference is
/// the whole reason both are carried: HEAD and the index are per-worktree, refs and objects are
/// shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub workdir: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub head: GitHead,
}

/// Resolve the repo containing `path` (a file or a directory) to its identity, or `None` when the
/// path is in no repo — or in a bare one, which has no working tree for an editor to point at.
///
/// Discovery walks upward, so a workspace root nested inside a repo resolves to the repo's
/// top level, not the root: [`RepoIdentity::workdir`] is deliberately not the path passed in.
/// Paths are canonicalized so two roots reaching one repo by different symlinks produce one id.
pub fn discover_repo(path: &Path) -> Option<RepoIdentity> {
    let canonical = path.canonicalize().ok()?;
    let repo = git2::Repository::discover(&canonical).ok()?;
    let workdir = repo.workdir()?.canonicalize().ok()?;
    // `path()`/`commondir()` are the same directory for an ordinary checkout and diverge for a
    // linked worktree. Canonicalized so ids and grouping compare as strings.
    let git_dir = repo.path().canonicalize().ok()?;
    let common_dir = repo.commondir().canonicalize().unwrap_or(git_dir.clone());
    Some(RepoIdentity {
        workdir,
        git_dir,
        common_dir,
        head: head_state(&repo)?,
    })
}

/// One local branch, as the branch picker's rows need it.
///
/// Read-only and libgit2-side (`docs/git-phase-2.md` decision 1: reads stay in-process, writes
/// shell out). Everything here is cheap — a ref walk plus one commit lookup each — so the list is
/// rebuilt per `picker/view` rather than cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    /// Shorthand name (`main`), not the full `refs/heads/main`.
    pub name: String,
    /// This branch is the current worktree's HEAD.
    pub is_head: bool,
    /// The tip commit's summary line; empty when it can't be read.
    pub subject: String,
    /// Tip commit's author time, Unix seconds — the client formats it (as blame does).
    pub timestamp: i64,
    /// Configured upstream (`origin/main`), or `None` for a branch never pushed.
    pub upstream: Option<String>,
    /// Commits this branch has that its upstream doesn't, and vice versa. Both `0` without an
    /// upstream. Computed locally, so they're only as fresh as the last fetch — stage 3's job.
    pub ahead: u32,
    pub behind: u32,
    /// The checkout in this family holding this branch, when one does. Git permits a branch in only
    /// one checkout at a time, so this is at most one — and its presence is what makes a branch row
    /// a *worktree* row in the merged picker.
    pub checkout: Option<BranchCheckout>,
    /// This row is a **detached** worktree rather than a branch: `name` is the tree's admin name
    /// (the only thing about it worth typing — a commit id isn't) and this is its short commit id.
    ///
    /// Such a row has no branch, so it supports neither checkout nor branch deletion; Enter opens
    /// the tree and `Ctrl-d` removes it. It exists because a branch-keyed list otherwise has
    /// nowhere to put a tree that is on no branch, and silently omitting one would make it
    /// unreachable — including for removal.
    pub detached_at: Option<String>,
}

/// The checkout holding a branch: which tree it is, and what can be done to it.
///
/// One type rather than the flat `checked_out_in` / `checked_out_in_main` pair it replaces, because
/// the merged branch picker needs more than "somewhere else has it" — it needs the admin name (to
/// bind, and to remove) and the lock/prune state (to know what `Ctrl-d` may do). Those only ever
/// make sense together, and a row either has a checkout or doesn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCheckout {
    /// Working directory of the checkout.
    pub path: String,
    /// It is the repo's **main** working tree rather than a linked worktree. Different sentence for
    /// the user, and different consequences: the main tree has no admin name and can't be removed.
    pub is_main: bool,
    /// Admin name of the linked worktree. **Empty for the main tree**, which has none — the same
    /// convention `GitWorktreeRow::name` uses, and what `workspace/bind_worktree` reads as "unbind".
    pub worktree: String,
    /// This is the checkout the caller is standing in. Not a refusal: the row for the branch you
    /// are on is the ordinary "already here" case, distinct from a branch held by another tree.
    pub is_current: bool,
    /// `git worktree lock` is holding it — never removed, not even with force.
    pub locked: bool,
    /// The admin entry outlived its directory (`rm -rf` rather than `git worktree remove`). The
    /// only condition under which pruning is offered.
    pub prunable: bool,
}

/// Every local branch of the repo at `workdir`, checked-out ones first (see [`checkout_rank`]) and
/// then most-recently-committed first.
///
/// That ordering is the useful default for a fuzzy picker: with no query you want the branch you're
/// on and the ones you've touched lately, not an alphabetical list where `main` sits under `feat/…`.
///
/// Each row carries the checkout holding it, when one does — which is what makes this list the
/// *whole* merged picker rather than half of it. A branch in a worktree is not a separate kind of
/// row; it is a branch row that knows where it lives. The one thing this list can't produce is a
/// **detached** worktree, which holds no branch: see [`detached_worktrees`].
///
/// Best-effort throughout — a branch whose tip can't be peeled still lists, with an empty subject.
/// An unborn HEAD (fresh repo, no commits) has no branches at all and correctly returns empty.
pub fn list_branches(workdir: &Path) -> Vec<BranchRow> {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let head_name = match head_state(&repo) {
        Some(GitHead::Branch { name, .. }) => Some(name),
        _ => None,
    };
    let mut checkouts = checkouts_by_branch(workdir);

    let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (branch, _) in branches.flatten() {
        let Some(name) = branch.name().ok().flatten().map(String::from) else {
            continue;
        };
        let commit = branch.get().peel_to_commit().ok();
        let upstream = branch.upstream().ok();
        // `graph_ahead_behind` wants both tips; without an upstream there's nothing to compare to.
        let (ahead, behind) = match (
            branch.get().target(),
            upstream.as_ref().and_then(|u| u.get().target()),
        ) {
            (Some(local), Some(up)) => repo.graph_ahead_behind(local, up).unwrap_or((0, 0)),
            _ => (0, 0),
        };
        out.push(BranchRow {
            is_head: head_name.as_deref() == Some(name.as_str()),
            subject: commit
                .as_ref()
                .and_then(|c| c.summary().ok().flatten().map(String::from))
                .unwrap_or_default(),
            timestamp: commit.as_ref().map(|c| c.time().seconds()).unwrap_or(0),
            upstream: upstream.and_then(|u| u.name().ok().flatten().map(String::from)),
            ahead: ahead as u32,
            behind: behind as u32,
            checkout: checkouts.remove(&name),
            detached_at: None,
            name,
        });
    }
    // Checked-out branches pinned above the rest, then newest-tip first with the name as tiebreak
    // so the order is stable when several branches share a commit (a just-created branch and its
    // base).
    out.sort_by(|a, b| {
        checkout_rank(a)
            .cmp(&checkout_rank(b))
            .then(b.timestamp.cmp(&a.timestamp))
            .then(a.name.cmp(&b.name))
    });
    out
}

/// Sort bucket for [`list_branches`]: where you are, the main checkout, the family's other
/// worktrees, then branches no tree holds.
///
/// Current-first is how Buffers and Workspaces already land their selection — the default highlight
/// is index 0, so "where you are" being first makes Enter-on-open a no-op without any extra
/// mechanism. Pinning rather than sectioning is deliberate: splitting the list into "has a tree" and
/// "doesn't" scatters the branches you are looking for across two places, where pinning keeps them
/// in one familiar recency order underneath a short block of trees.
fn checkout_rank(row: &BranchRow) -> u8 {
    match &row.checkout {
        Some(c) if c.is_current => 0,
        Some(c) if c.is_main => 1,
        Some(_) => 2,
        None => 3,
    }
}

/// Every row of the merged branch picker: [`list_branches`] plus the trees a branch cannot
/// represent ([`detached_worktrees`]), in one ordering.
///
/// Sorted as a whole rather than appended, so "where you are" stays at index 0 even when where you
/// are is a detached tree — the default highlight is index 0, which is how Buffers and Workspaces
/// already land their selection, and it makes Enter-on-open a no-op.
///
/// Kept separate from `list_branches` so that function stays what its name says. Callers that want
/// branches (the checkout pre-flight, tests) want branches; only the picker wants both.
pub fn branch_picker_rows(workdir: &Path) -> Vec<BranchRow> {
    let mut rows = list_branches(workdir);
    rows.extend(detached_worktrees(workdir));
    rows.sort_by(|a, b| {
        checkout_rank(a)
            .cmp(&checkout_rank(b))
            .then(b.timestamp.cmp(&a.timestamp))
            .then(a.name.cmp(&b.name))
    });
    rows
}

/// Rows for the worktrees that hold no branch, which [`list_branches`] therefore cannot produce: a
/// tree on a **detached** HEAD, and a **prunable** one whose directory is gone (for which
/// [`crate::worktree::list`] reports no head at all).
///
/// Both have to appear or they are unreachable — including for the `Ctrl-d` that removes them,
/// which in the prunable case is the only thing left to do with the entry.
///
/// The main checkout is never among these. Detached or not, it cannot be removed and has no admin
/// name to identify a row by, so a row for it could carry no verb.
pub fn detached_worktrees(workdir: &Path) -> Vec<BranchRow> {
    crate::worktree::list(workdir)
        .into_iter()
        .filter(|row| {
            !row.is_main
                && !matches!(
                    row.head,
                    Some(GitHead::Branch { .. }) | Some(GitHead::Unborn { .. })
                )
        })
        .map(|row| BranchRow {
            // Empty for a prunable tree, which has no head to read at all — the row still needs to
            // exist, and the client renders it on `prunable` rather than on this.
            detached_at: Some(match &row.head {
                Some(GitHead::Detached { oid }) => oid.clone(),
                _ => String::new(),
            }),
            checkout: Some(BranchCheckout {
                path: row.path,
                is_main: false,
                worktree: row.name.clone(),
                is_current: row.is_current,
                locked: row.locked,
                prunable: row.prunable,
            }),
            // The admin name *is* the row's name here: a commit id is not something anyone types to
            // find a tree, and a prunable entry has not even got one of those.
            name: row.name,
            // `is_head` is a statement about a branch, and this row has none. "You are here" is
            // carried by `checkout.is_current`, which is what the ordering reads.
            is_head: false,
            subject: String::new(),
            timestamp: 0,
            upstream: None,
            ahead: 0,
            behind: 0,
        })
        .collect()
}

/// Every checkout in this family, keyed by the branch it holds.
///
/// Built from [`crate::worktree::list`] rather than re-walking `worktrees()` here: that function
/// already covers both directions (a linked worktree asking this question must still see the main
/// checkout, which `worktrees()` never lists) and already carries the admin name and lock/prune
/// state the merged picker needs. Two walks producing subtly different views of one family is
/// exactly the drift the merge is meant to remove.
///
/// Includes the **current** checkout, unlike the `branches_checked_out_elsewhere` it replaces. The
/// merged picker needs the branch you're on to be a worktree row like any other; callers who
/// specifically want *another* tree filter on [`BranchCheckout::is_current`].
///
/// `Unborn` counts as holding its branch — git refuses a second checkout of one just the same —
/// even though such a branch has no ref yet and so no row in [`list_branches`].
fn checkouts_by_branch(workdir: &Path) -> HashMap<String, BranchCheckout> {
    let mut map = HashMap::new();
    for row in crate::worktree::list(workdir) {
        let name = match &row.head {
            Some(GitHead::Branch { name, .. }) | Some(GitHead::Unborn { name }) => name.clone(),
            // A detached tree holds no branch. It still gets a picker row, built separately —
            // there is no branch for it to hang off.
            Some(GitHead::Detached { .. }) | None => continue,
        };
        map.entry(name).or_insert_with(|| BranchCheckout {
            path: row.path.clone(),
            is_main: row.is_main,
            worktree: row.name.clone(),
            is_current: row.is_current,
            locked: row.locked,
            prunable: row.prunable,
        });
    }
    map
}

/// The workdir of another worktree holding `branch`, when one does — the checkout pre-flight's
/// question, asked without listing every branch.
///
/// Git permits a branch in only one worktree at a time, so this is a refusal the server can make
/// (and explain, naming the worktree) before spawning a `git checkout` that could only fail. The
/// tree you are standing in is excluded: it holding the branch means you are already on it, which
/// is not a refusal.
pub fn branch_checked_out_elsewhere(workdir: &Path, branch: &str) -> Option<String> {
    checkouts_by_branch(workdir)
        .remove(branch)
        .filter(|c| !c.is_current)
        .map(|c| c.path)
}

/// Whether `branch` is fully merged into HEAD — the check `git branch -d` makes before refusing.
///
/// Done here, from libgit2, rather than by reading git's refusal: the client escalates to a
/// force-delete confirm on this specific outcome, and `docs/git-phase-2.md` decision 1 rules out
/// parsing stderr into structured variants. `true` when the answer can't be determined (an unborn
/// HEAD, an unpeelable tip) so the caller falls through to git, which is authoritative anyway.
pub fn branch_is_merged(workdir: &Path, branch: &str) -> bool {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return true;
    };
    let tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .ok()
        .and_then(|b| b.get().target());
    let head = repo.head().ok().and_then(|h| h.target());
    match (tip, head) {
        (Some(tip), Some(head)) => {
            repo.graph_descendant_of(head, tip).unwrap_or(true) || tip == head
        }
        _ => true,
    }
}

/// Whether the repo has any remote configured — the gate `git/fetch` checks before spawning.
///
/// Asked of libgit2 rather than read out of git's complaint, for the usual reason, plus one
/// specific to the periodic fetcher: this is the failure that can never succeed on retry, so it
/// has to be distinguishable from a network blip that will. `false` when the repo can't be opened,
/// which folds an unreadable repo into "nothing to fetch" rather than an error.
pub fn has_remote(workdir: &Path) -> bool {
    !remote_names(workdir).is_empty()
}

/// The repo's configured remote names, in libgit2's order.
///
/// `git/push` needs the *list*, not just "is there one": a branch with no upstream can be published
/// to a lone remote without asking, but with several the choice is the user's — pushing to the
/// wrong one in a fork workflow publishes work where it wasn't meant to go.
pub fn remote_names(workdir: &Path) -> Vec<String> {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let Ok(remotes) = repo.remotes() else {
        return Vec::new();
    };
    // `StringArray` yields `Result<Option<&str>, _>` per entry — a name that isn't valid UTF-8
    // reads as `Ok(None)`. Both empty cases are simply skipped: a remote we can't name is one we
    // can't push to either.
    remotes
        .iter()
        .filter_map(|entry| entry.ok().flatten().map(String::from))
        .collect()
}

/// [`upstream_divergence`] for a repo rather than a buffer — what `git/fetch` reports back once
/// the refs have moved. `None` when the repo can't be opened or HEAD has no upstream.
pub fn repo_upstream(workdir: &Path) -> Option<GitUpstreamStatus> {
    let repo = git2::Repository::open(workdir).ok()?;
    let head = head_state(&repo)?;
    upstream_divergence(&repo, &head)
}

/// The commit HEAD points at, as a hex oid. `None` for an unborn HEAD or an unreadable repo.
///
/// A string rather than a `git2::Oid` so the handlers can hold one across an `await` without the
/// git2 types leaking out of this module — it is an opaque token to them, compared and passed back
/// to [`head_move`], never inspected.
pub fn head_oid(workdir: &Path) -> Option<String> {
    let repo = git2::Repository::open(workdir).ok()?;
    let oid = repo.head().ok()?.peel_to_commit().ok()?.id().to_string();
    Some(oid)
}

/// How local history changed while an operation ran — what a pull actually *did*.
///
/// Read from the commit graph rather than from git's summary line, which is the same discipline
/// every other status here follows. The three moves are genuinely different things to have
/// happened: catching up, gaining a merge commit, or having your commits rewritten onto new bases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadMove {
    /// HEAD is where it was. Also the answer when the move can't be read at all — reporting "up to
    /// date" for an unreadable repo is the harmless direction to be wrong in, and the caller has
    /// already established that git succeeded.
    Unchanged,
    /// HEAD moved onto a descendant of where it was, without a merge: nothing local was in the way.
    FastForward,
    /// A merge commit was created with the old HEAD among its parents.
    Merge,
    /// The old HEAD is no longer an ancestor of the new one, which is what rewriting means —
    /// `pull.rebase` replaying local commits onto the upstream.
    Rewritten,
}

/// Classify the move from `before` to the repo's current HEAD. `before` is a [`head_oid`] taken
/// before the operation ran.
pub fn head_move(workdir: &Path, before: Option<&str>) -> HeadMove {
    let Some(after) = head_oid(workdir) else {
        return HeadMove::Unchanged;
    };
    let Some(before) = before else {
        // Unborn before, committed now: the branch caught up from nothing, which is the fast-forward
        // a pull into a fresh clone performs.
        return HeadMove::FastForward;
    };
    if before == after {
        return HeadMove::Unchanged;
    }
    let Ok(repo) = git2::Repository::open(workdir) else {
        return HeadMove::Unchanged;
    };
    let (Ok(before_oid), Ok(after_oid)) =
        (git2::Oid::from_str(before), git2::Oid::from_str(&after))
    else {
        return HeadMove::Unchanged;
    };

    // Merge before fast-forward: a merge commit is a descendant of the old HEAD too, so the
    // descendant test alone would call every merge a fast-forward.
    if let Ok(commit) = repo.find_commit(after_oid) {
        if commit.parent_count() > 1 && commit.parent_ids().any(|p| p == before_oid) {
            return HeadMove::Merge;
        }
    }
    if repo
        .graph_descendant_of(after_oid, before_oid)
        .unwrap_or(false)
    {
        return HeadMove::FastForward;
    }
    HeadMove::Rewritten
}

/// What multi-step operation the repo is stopped part-way through, if any.
///
/// libgit2's `Repository::state` reads the same `.git` markers git does (`MERGE_HEAD`,
/// `rebase-merge/`, `CHERRY_PICK_HEAD`…), so this sees an operation started from a terminal exactly
/// as well as one of ours. The rebase flavours are folded together: they differ in how git resumes,
/// which nothing here acts on.
pub fn repo_operation(workdir: &Path) -> Option<GitRepoOperation> {
    state_operation(&git2::Repository::open(workdir).ok()?)
}

/// [`repo_operation`] for a caller that already has the repo open — `load_baseline` does, and
/// paying for a second discovery on every baseline reload to learn a flag would be silly.
fn state_operation(repo: &git2::Repository) -> Option<GitRepoOperation> {
    use git2::RepositoryState as S;
    match repo.state() {
        S::Clean => None,
        S::Merge => Some(GitRepoOperation::Merge),
        S::Revert | S::RevertSequence => Some(GitRepoOperation::Revert),
        S::CherryPick | S::CherryPickSequence => Some(GitRepoOperation::CherryPick),
        S::Bisect => Some(GitRepoOperation::Bisect),
        S::Rebase | S::RebaseInteractive | S::RebaseMerge => Some(GitRepoOperation::Rebase),
        S::ApplyMailbox | S::ApplyMailboxOrRebase => Some(GitRepoOperation::ApplyMailbox),
    }
}

/// Whether this one path is left conflicted — the guard `git/apply_hunk` checks before writing an
/// index entry.
///
/// Asked per path rather than by scanning [`conflicted_paths`] because it runs on every stage
/// keystroke, and a conflicted *repo* says nothing about the file the cursor is in. Stage 2 is
/// "ours", the side that exists in every conflict flavour except a delete/modify where we deleted;
/// the ancestor and "theirs" slots cover the rest.
pub fn path_has_conflict(workdir: &Path, rel: &Path) -> bool {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return false;
    };
    index_has_conflict(&repo, rel)
}

/// [`path_has_conflict`] for a caller that already has the repo open — [`load_baseline`] does, and
/// it asks this of every file it loads.
fn index_has_conflict(repo: &git2::Repository, rel: &Path) -> bool {
    let Ok(index) = repo.index() else {
        return false;
    };
    // Cheap exit: `has_conflicts` is a flag on the index, so an unconflicted repo — every repo,
    // nearly all of the time — never reaches the per-path lookups.
    if !index.has_conflicts() {
        return false;
    }
    (1..=3).any(|stage| index.get_path(rel, stage).is_some())
}

/// Repo-relative paths left conflicted in the index — where a merge or rebase stopped.
///
/// The index is the authority git itself uses (`git status` reads the same entries), so this needs
/// no parsing of the `CONFLICT (content):` lines git prints. Sorted, because the index iterates in
/// its own order and this list is shown to the user.
pub fn conflicted_paths(workdir: &Path) -> Vec<String> {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let Ok(index) = repo.index() else {
        return Vec::new();
    };
    let Ok(conflicts) = index.conflicts() else {
        return Vec::new();
    };
    let mut out: Vec<String> = conflicts
        .filter_map(|c| c.ok())
        // "Ours" first: a modify/delete conflict has only one side, and either names the same path.
        .filter_map(|c| c.our.or(c.their).or(c.ancestor))
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One conflict left in a buffer by a merge, rebase, cherry-pick or `stash pop`: the marker block
/// git wrote, in **0-based buffer line** coordinates.
///
/// **Read from the buffer's markers, not from the index.** The index is the authority on *which
/// files* are conflicted ([`path_has_conflict`]) and gates this parse; it cannot say where inside
/// one, and it stops describing the file the moment the user starts resolving. The markers are what
/// they are looking at and editing, so they are the only thing that stays true — and they are
/// written the same way whatever produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// The `<<<<<<<` line.
    pub start_line: u32,
    /// The `>>>>>>>` line. Inclusive: the whole region is `start_line..=end_line`.
    pub end_line: u32,
    /// Content lines of our side, between the `<<<<<<<` and the `|||||||`/`=======` that follows.
    /// Empty when our side deleted the region.
    pub ours: std::ops::Range<u32>,
    /// The common-ancestor section, present only under `merge.conflictstyle = diff3` / `zdiff3`.
    /// Dropped by every resolution — it is context, never an outcome.
    pub base: Option<std::ops::Range<u32>>,
    /// Content lines of their side, between the `=======` and the `>>>>>>>`.
    pub theirs: std::ops::Range<u32>,
    /// What the markers name the two sides: `HEAD` and a branch, or a commit subject mid-rebase.
    /// Empty when the marker carried no label.
    pub ours_label: String,
    pub theirs_label: String,
}

impl ConflictRegion {
    /// Whether `line` falls anywhere in the region, markers included.
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start_line && line <= self.end_line
    }

    /// Whether the region overlaps the inclusive line span `lo..=hi`.
    pub fn overlaps(&self, lo: u32, hi: u32) -> bool {
        self.start_line <= hi && self.end_line >= lo
    }
}

/// Git's default `conflict-marker-size`. A per-path override via gitattributes is possible and not
/// supported here: a marker of another length simply isn't recognised, which leaves the file
/// undecorated rather than mis-parsed.
const MARKER_LEN: usize = 7;

/// Find every conflict marker block in `text`.
///
/// A pure function of the buffer, so it is cheap enough for the per-edit path (a line scan, the same
/// order as the diff it replaces) and testable without a repo. Callers gate it on the index saying
/// this path is conflicted — otherwise a file that merely *documents* conflict markers, like this
/// repo's own notes, would light up.
///
/// Malformed blocks are dropped rather than guessed at: a region with no `=======`, or one still
/// open at end of buffer, describes nothing that can be resolved by taking a side. A nested
/// `<<<<<<<` restarts the region — recursive merges can produce one, and the inner block is the
/// resolvable one.
pub fn conflict_regions(text: &ropey::Rope) -> Vec<ConflictRegion> {
    /// The part of a region seen so far, waiting for its `>>>>>>>`.
    struct Open {
        start: u32,
        ours_label: String,
        base_start: Option<u32>,
        separator: Option<u32>,
    }

    let mut out = Vec::new();
    let mut open: Option<Open> = None;

    for (i, line) in text.lines().enumerate() {
        let i = i as u32;
        let Some((marker, label)) = marker_of(line) else {
            continue;
        };
        match marker {
            '<' => {
                open = Some(Open {
                    start: i,
                    ours_label: label,
                    base_start: None,
                    separator: None,
                })
            }
            // Only the *first* of each divider counts, exactly as git's own re-parse does: a
            // `=======` inside our side (a heading underline, say) would otherwise re-split the
            // region around it.
            '|' => {
                if let Some(o) = open.as_mut() {
                    if o.base_start.is_none() && o.separator.is_none() {
                        o.base_start = Some(i);
                    }
                }
            }
            '=' => {
                if let Some(o) = open.as_mut() {
                    if o.separator.is_none() {
                        o.separator = Some(i);
                    }
                }
            }
            '>' => {
                let Some(o) = open.take() else { continue };
                // No separator: not a conflict block, whatever else it is.
                let Some(separator) = o.separator else {
                    continue;
                };
                let ours_end = o.base_start.unwrap_or(separator);
                out.push(ConflictRegion {
                    start_line: o.start,
                    end_line: i,
                    ours: o.start + 1..ours_end,
                    base: o.base_start.map(|b| b + 1..separator),
                    theirs: separator + 1..i,
                    ours_label: o.ours_label,
                    theirs_label: label,
                });
            }
            _ => {}
        }
    }
    out
}

/// Drop the diff hunks that fall inside a conflict block.
///
/// Those lines belong to the conflict decoration, and what the diff has to say about them is only
/// that markers were inserted — noise on top of the thing the user is actually reading. Everything
/// outside a block keeps its hunk, so a block resolved by hand immediately reads as an ordinary
/// change against HEAD (gutter, inline diff, changes picker) even though the *file* is still
/// conflicted in the index until it's marked resolved.
///
/// A hunk that straddles a block boundary is dropped whole. In practice hunks don't straddle: git
/// writes the markers around the disputed lines, so the diff against HEAD lands inside the block.
pub fn mask_conflicts(hunks: Vec<DiffHunk>, regions: &[ConflictRegion]) -> Vec<DiffHunk> {
    if regions.is_empty() {
        return hunks;
    }
    hunks
        .into_iter()
        .filter(|h| {
            // A pure deletion occupies no new-side lines; it sits *on* its anchor.
            let hi = h.anchor_line + h.new_lines.saturating_sub(1);
            !regions.iter().any(|r| r.overlaps(h.anchor_line, hi))
        })
        .collect()
}

/// What the Git-changes picker lists for a conflicted file: one row per conflict block, plus the
/// ordinary HEAD→content diff for everything the blocks don't cover.
///
/// The blocks are what a picker is for mid-merge — "where are the conflicts" — and the rest is
/// what the resolution has changed so far. Without the block rows a conflicted file would list as
/// one enormous hunk (no index entry ⇒ the whole file reads as changed); without the diff rows a
/// file resolved but not yet marked would vanish from the list entirely.
///
/// Each block is reported whole, markers included, so its row previews the `<<<<<<<` line — which
/// names the side and is unmistakably a conflict — and the picker's query greps both sides.
pub fn conflict_change_hunks(text: &ropey::Rope, diff: Vec<DiffHunk>) -> Vec<DiffHunk> {
    let regions = conflict_regions(text);
    let mut out = mask_conflicts(diff, &regions);
    out.extend(regions.iter().map(|r| DiffHunk {
        kind: ChangeKind::Modified,
        anchor_line: r.start_line,
        new_lines: r.end_line - r.start_line + 1,
        deleted: Vec::new(),
        old_start: r.start_line,
        stage: DiffStage::Unstaged,
    }));
    out.sort_by_key(|h| h.anchor_line);
    out
}

/// Rewrite `text` with each of `regions` reduced to one side, markers and all scenery removed.
///
/// `regions` must be in ascending order and non-overlapping — as [`conflict_regions`] returns them,
/// filtered by the caller to the ones the cursor or selection addresses. Every other line is
/// reproduced exactly, including the file's line endings and any missing final newline, because the
/// rope's own lines are what get copied.
pub fn resolve_conflicts(
    text: &ropey::Rope,
    regions: &[&ConflictRegion],
    side: ConflictSide,
) -> String {
    let mut out = String::new();
    let copy = |range: std::ops::Range<u32>, out: &mut String| {
        for line in range {
            if (line as usize) < text.len_lines() {
                out.extend(text.line(line as usize).chunks());
            }
        }
    };

    let mut next = 0u32;
    for r in regions {
        copy(next..r.start_line, &mut out);
        // The diff3 base section is dropped by every side: it is what the lines *were*, never an
        // outcome the user is choosing.
        match side {
            ConflictSide::Ours => copy(r.ours.clone(), &mut out),
            ConflictSide::Theirs => copy(r.theirs.clone(), &mut out),
            // File order, so "both" reads the way the block did.
            ConflictSide::Both => {
                copy(r.ours.clone(), &mut out);
                copy(r.theirs.clone(), &mut out);
            }
        }
        next = r.end_line + 1;
    }
    copy(next..text.len_lines() as u32, &mut out);
    out
}

/// Recognise a conflict marker line: exactly seven of `<`, `|`, `=` or `>`, then end of line or a
/// space and a label. Returns the marker character and the label (empty when there is none).
///
/// The length test is exact in both directions. An eighth marker character means this is a rule or
/// a heading underline, not a marker — the common false positive in prose and in generated files.
fn marker_of(line: ropey::RopeSlice<'_>) -> Option<(char, String)> {
    let mut chars = line.chars();
    let marker = match chars.next()? {
        c @ ('<' | '|' | '=' | '>') => c,
        _ => return None,
    };
    for _ in 1..MARKER_LEN {
        if chars.next()? != marker {
            return None;
        }
    }
    match chars.next() {
        None | Some('\n') | Some('\r') => Some((marker, String::new())),
        Some(' ') => {
            let label: String = chars.collect();
            Some((marker, label.trim_end().to_string()))
        }
        Some(_) => None,
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

// ---- intra-line diff --------------------------------------------------------------------------
//
// Word-grain emphasis for the inline diff view: given the old and new text of one paired line in
// a Modified hunk, pick out the sub-line ranges that actually changed. libgit2 stops at line
// grain, so this is our own three-stage pass: trim the common prefix/suffix, LCS over word
// tokens on what's left, then merge nearby runs and snap them outward to word boundaries so the
// result reads as changed *words*, not confetti.

/// Byte ranges (start, exclusive end) within one side's text. Char-boundary aligned, sorted,
/// non-overlapping.
pub(crate) type EmphasisSpans = Vec<(u32, u32)>;

/// Lines longer than this skip intra-line analysis (minified JS etc.) — the whole-line tint is
/// already right, and the LCS isn't worth the cycles on the render path.
const INTRALINE_MAX_BYTES: usize = 4096;
/// Token-pair budget for the LCS table; above it the changed middle is emitted as one span per
/// side rather than diffed further.
const INTRALINE_MAX_CELLS: usize = 10_000;
/// Unchanged gaps of at most this many bytes between two changed runs are absorbed, so
/// `a.b` → `x.y` emphasizes once, not twice.
const INTRALINE_JOIN_GAP: usize = 2;

/// Compare one old/new line pair and return the changed byte ranges on each side.
///
/// `None` means "no usable intra-line story": the pair changed too much (rewritten lines render
/// as today's whole-line tint rather than near-total emphasis) or is too long to analyse. Either
/// side's spans may be empty — a pure in-line insertion has nothing to emphasize on the old side.
pub(crate) fn intraline_emphasis(old: &str, new: &str) -> Option<(EmphasisSpans, EmphasisSpans)> {
    if old.len() > INTRALINE_MAX_BYTES || new.len() > INTRALINE_MAX_BYTES {
        return None;
    }
    let prefix = common_prefix_bytes(old, new);
    let suffix = common_suffix_bytes(&old[prefix..], &new[prefix..]);
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    if old_mid.is_empty() && new_mid.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }

    let old_tokens = tokenize(old_mid);
    let new_tokens = tokenize(new_mid);
    let whole = |mid: &str| -> Vec<(usize, usize)> {
        if mid.is_empty() {
            Vec::new()
        } else {
            vec![(0, mid.len())]
        }
    };
    let (mut old_spans, mut new_spans) = if old_tokens.len() * new_tokens.len()
        > INTRALINE_MAX_CELLS
    {
        // Middle too busy to diff — one span per side is still better than nothing.
        (whole(old_mid), whole(new_mid))
    } else {
        let (old_matched, new_matched) = lcs_matches(old_mid, &old_tokens, new_mid, &new_tokens);
        (
            changed_spans_of(&old_tokens, &old_matched),
            changed_spans_of(&new_tokens, &new_matched),
        )
    };
    merge_close(&mut old_spans, INTRALINE_JOIN_GAP);
    merge_close(&mut new_spans, INTRALINE_JOIN_GAP);

    // Mostly-rewritten pair: near-total emphasis is noisier than the plain tint. Bail while the
    // spans are still tight (pre-snap), using whole-line lengths so a long shared prefix/suffix
    // counts in the pair's favour.
    let changed: usize = old_spans
        .iter()
        .chain(new_spans.iter())
        .map(|(s, e)| e - s)
        .sum();
    if changed * 5 > (old.len() + new.len()) * 3 {
        return None;
    }

    let offset_and_snap = |spans: Vec<(usize, usize)>, text: &str| -> EmphasisSpans {
        let mut out: Vec<(usize, usize)> = spans
            .into_iter()
            .map(|(s, e)| (s + prefix, e + prefix))
            .collect();
        snap_to_words(&mut out, text);
        out.into_iter().map(|(s, e)| (s as u32, e as u32)).collect()
    };
    Some((
        offset_and_snap(old_spans, old),
        offset_and_snap(new_spans, new),
    ))
}

fn common_prefix_bytes(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

fn common_suffix_bytes(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().rev().zip(b.chars().rev()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte range of each token: a run of word chars, a run of whitespace, or a single other char.
fn tokenize(text: &str) -> Vec<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if is_word_char(c) {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut prev_class = Class::Other;
    for (i, c) in text.char_indices() {
        let cls = class(c);
        match tokens.last_mut() {
            Some((_, end)) if cls == prev_class && cls != Class::Other => {
                *end = i + c.len_utf8();
            }
            _ => tokens.push((i, i + c.len_utf8())),
        }
        prev_class = cls;
    }
    tokens
}

/// Longest common subsequence over tokens (compared by text), returning per-token "kept" flags
/// for each side. Classic O(n·m) table — bounded by [`INTRALINE_MAX_CELLS`] at the call site.
fn lcs_matches(
    old_text: &str,
    old_tokens: &[(usize, usize)],
    new_text: &str,
    new_tokens: &[(usize, usize)],
) -> (Vec<bool>, Vec<bool>) {
    let n = old_tokens.len();
    let m = new_tokens.len();
    fn tok(text: &str, (s, e): (usize, usize)) -> &str {
        &text[s..e]
    }
    let mut table = vec![0u16; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[idx(i, j)] = if tok(old_text, old_tokens[i]) == tok(new_text, new_tokens[j]) {
                table[idx(i + 1, j + 1)] + 1
            } else {
                table[idx(i + 1, j)].max(table[idx(i, j + 1)])
            };
        }
    }
    let mut old_matched = vec![false; n];
    let mut new_matched = vec![false; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if tok(old_text, old_tokens[i]) == tok(new_text, new_tokens[j]) {
            old_matched[i] = true;
            new_matched[j] = true;
            i += 1;
            j += 1;
        } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (old_matched, new_matched)
}

/// Byte spans of the unmatched-token runs, adjacent unmatched tokens coalesced.
fn changed_spans_of(tokens: &[(usize, usize)], matched: &[bool]) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (t, &(s, e)) in tokens.iter().enumerate() {
        if matched[t] {
            continue;
        }
        match spans.last_mut() {
            Some((_, end)) if *end == s => *end = e,
            _ => spans.push((s, e)),
        }
    }
    spans
}

/// Absorb unchanged gaps of at most `gap` bytes between consecutive spans.
fn merge_close(spans: &mut Vec<(usize, usize)>, gap: usize) {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for &(s, e) in spans.iter() {
        match merged.last_mut() {
            Some((_, end)) if s <= *end + gap => *end = (*end).max(e),
            _ => merged.push((s, e)),
        }
    }
    *spans = merged;
}

/// Extend each span outward to word boundaries (a span edge inside `foo|bar` grows to cover the
/// whole word), then re-merge anything the growth made overlap. Keeps the emphasis word-grain
/// when the prefix/suffix trim split a word.
fn snap_to_words(spans: &mut Vec<(usize, usize)>, text: &str) {
    for (s, e) in spans.iter_mut() {
        while *s > 0 {
            let prev = text[..*s].chars().next_back();
            let cur = text[*s..].chars().next();
            match (prev, cur) {
                (Some(p), Some(c)) if is_word_char(p) && is_word_char(c) => {
                    *s -= p.len_utf8();
                }
                _ => break,
            }
        }
        while *e < text.len() {
            let prev = text[..*e].chars().next_back();
            let cur = text[*e..].chars().next();
            match (prev, cur) {
                (Some(p), Some(c)) if is_word_char(p) && is_word_char(c) => {
                    *e += c.len_utf8();
                }
                _ => break,
            }
        }
    }
    merge_close(spans, 0);
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

/// One entry of the stash reflog, as the stash picker's rows need it.
#[derive(Debug, Clone)]
pub struct StashRow {
    /// Position in `refs/stash` at listing time — `stash@{index}`, which is how the git CLI
    /// addresses it. Shifts as entries are added or dropped, which is why every *action* re-resolves
    /// it from `oid` rather than trusting a listed index.
    pub index: usize,
    /// The stash commit's hash: the stable identity, and what `git/show` previews.
    pub oid: String,
    /// git's own line — `WIP on main: abc1234 subject`, or the message the user gave. Carries the
    /// branch, so nothing here re-derives it.
    pub message: String,
    pub timestamp: i64,
}

/// The repo's stash entries, newest first (`stash@{0}` leads).
///
/// `refs/stash` is **shared across worktrees**, so a linked worktree lists the whole repo's
/// stashes — that's git's own model, not an approximation.
pub fn list_stashes(workdir: &Path) -> Vec<StashRow> {
    let Ok(mut repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let mut rows: Vec<(usize, String, String)> = Vec::new();
    // The callback can't borrow `repo` (it's held mutably), so commit times are looked up after.
    let _ = repo.stash_foreach(|index, message, oid| {
        rows.push((index, oid.to_string(), message.to_string()));
        true
    });
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|(index, oid, message)| {
            let timestamp = git2::Oid::from_str(&oid)
                .ok()
                .and_then(|o| repo.find_commit(o).ok())
                .map(|c| c.time().seconds())
                .unwrap_or(0);
            StashRow {
                index,
                oid,
                message,
                timestamp,
            }
        })
        .collect()
}

/// The current `stash@{n}` position of the entry with this hash, or `None` if it's gone.
///
/// Every stash mutation re-resolves this immediately before shelling out: the CLI addresses stashes
/// by position, positions shift as entries are dropped, and a stale one would act on the *wrong*
/// stash rather than failing — the one outcome worth engineering against here.
pub fn stash_index_of(workdir: &Path, oid: &str) -> Option<usize> {
    list_stashes(workdir)
        .into_iter()
        .find(|s| s.oid == oid)
        .map(|s| s.index)
}

/// One row of the log picker: a commit reduced to what the list renders and matches on.
pub struct LogCommit {
    pub hash: String,
    pub short_hash: String,
    pub subject: String,
    pub author: String,
    pub timestamp: i64,
}

/// A repo's history from HEAD, newest first — the log picker's candidate set. `path` narrows to
/// commits that touched it (repo-relative); `None` walks everything.
///
/// **Bounded by commits *examined*, not rows produced.** For the whole-repo walk the two are the
/// same, but a path filter has to diff each commit against its parent to know whether the path was
/// touched, so a narrow path in a deep history could otherwise walk forever to fill a screen. The
/// bool in the return says the walk stopped early, which the picker must surface: the query filters
/// what was loaded, so a silent cap turns "older than the cap" into an indistinguishable "no
/// matches".
///
/// Path narrowing is a plain pathspec, not git's `--follow`: history ends at a rename. Rename
/// detection is a separate mechanism, and inferring it here would make the walk cost per commit
/// jump again.
pub fn log_commits(
    repo_path: &Path,
    path: Option<&str>,
    max_examined: usize,
) -> (Vec<LogCommit>, bool) {
    let mut out = Vec::new();
    let Ok(repo) = git2::Repository::discover(repo_path) else {
        return (out, false);
    };
    let Ok(mut walk) = repo.revwalk() else {
        return (out, false);
    };
    // Time order matches what `git log` shows by default, and keeps the newest-first reading the
    // picker's rows imply.
    let _ = walk.set_sorting(git2::Sort::TIME);
    if walk.push_head().is_err() {
        return (out, false); // unborn HEAD: no history yet, not an error
    }

    let mut truncated = false;
    for (examined, oid) in walk.enumerate() {
        if examined >= max_examined {
            truncated = true;
            break;
        }
        let Ok(oid) = oid else { continue };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if let Some(path) = path {
            if !commit_touches_path(&repo, &commit, path) {
                continue;
            }
        }
        let sig = commit.author();
        out.push(LogCommit {
            hash: commit.id().to_string(),
            short_hash: short_hash(&commit.id().to_string()),
            subject: commit
                .summary()
                .ok()
                .flatten()
                .unwrap_or_default()
                .to_string(),
            author: sig.name().unwrap_or("(unknown)").to_string(),
            timestamp: sig.when().seconds(),
        });
    }
    (out, truncated)
}

/// Whether `commit` changed `path` relative to its first parent — the pathspec test behind the
/// file-scoped log. A root commit counts as touching every path it introduces.
fn commit_touches_path(repo: &git2::Repository, commit: &git2::Commit, path: &str) -> bool {
    let Ok(new_tree) = commit.tree() else {
        return false;
    };
    let old_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = git2::DiffOptions::new();
    opts.pathspec(path);
    // `diff_tree_to_tree` with a pathspec is the cheap form: libgit2 prunes to the path rather than
    // diffing the whole tree and filtering after.
    repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
        .map(|d| d.deltas().len() > 0)
        .unwrap_or(false)
}

/// The abbreviated form of a commit hash, as git prints it in logs and as the client already
/// renders blame (`commit.chars().take(7)`).
fn short_hash(hash: &str) -> String {
    hash.chars().take(7).collect()
}

/// The content of a revision, materialised for a read-only virtual buffer (`git/show`).
pub struct RevisionContent {
    /// Buffer title — `abc1234 — subject` for a commit, `abc1234:src/main.rs` for a file. Git's own
    /// syntax for the file form, so it reads the way you'd type it.
    pub title: String,
    pub text: String,
    /// Detected from the path for a file; `None` for a commit's patch (no diff grammar is
    /// bundled — the patch renders unhighlighted, which is legible enough to defer).
    pub language: Option<String>,
}

/// A commit as `git show` prints it: metadata and message, then the patch against its first parent
/// (against the empty tree for a root commit, so the initial commit shows as all additions).
///
/// libgit2 rather than the CLI, per decision 1 — this is a read, and the patch text libgit2's
/// printer emits is the same format. Merge commits diff against the *first* parent only, which is
/// what `git show` does too.
pub fn show_commit(repo_path: &Path, rev: &str) -> Result<RevisionContent, String> {
    let repo = git2::Repository::discover(repo_path).map_err(|e| e.message().to_string())?;
    let commit = repo
        .revparse_single(rev)
        .and_then(|o| o.peel_to_commit())
        .map_err(|e| e.message().to_string())?;
    let sig = commit.author();
    let subject = commit
        .summary()
        .ok()
        .flatten()
        .unwrap_or("(no subject)")
        .to_string();
    let short = short_hash(&commit.id().to_string());

    let mut text = String::new();
    text.push_str(&format!("commit {}\n", commit.id()));
    text.push_str(&format!(
        "Author: {} <{}>\n",
        sig.name().unwrap_or("(unknown)"),
        sig.email().unwrap_or_default()
    ));
    text.push_str(&format!("Date:   {}\n\n", format_commit_time(sig.when())));
    // Indented four spaces, as git prints a message body.
    for line in commit.message().unwrap_or_default().trim_end().lines() {
        text.push_str(&format!("    {line}\n"));
    }
    text.push('\n');

    let new_tree = commit.tree().map_err(|e| e.message().to_string())?;
    let old_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let diff = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)
        .map_err(|e| e.message().to_string())?;
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        // The origin character is part of the patch for content lines but not for headers;
        // libgit2 hands it over separately either way.
        if matches!(line.origin(), '+' | '-' | ' ') {
            text.push(line.origin());
        }
        text.push_str(&String::from_utf8_lossy(line.content()));
        true
    })
    .map_err(|e| e.message().to_string())?;

    Ok(RevisionContent {
        title: format!("{short} — {subject}"),
        text,
        language: None,
    })
}

/// One file's content as of `rev` — `git show <rev>:<path>`. `path` is repo-relative. Binary
/// content is refused rather than dumped into a text buffer.
pub fn show_file(repo_path: &Path, rev: &str, path: &str) -> Result<RevisionContent, String> {
    let repo = git2::Repository::discover(repo_path).map_err(|e| e.message().to_string())?;
    let commit = repo
        .revparse_single(rev)
        .and_then(|o| o.peel_to_commit())
        .map_err(|e| e.message().to_string())?;
    let entry = commit
        .tree()
        .and_then(|t| t.get_path(Path::new(path)))
        .map_err(|_| format!("{path} does not exist at {rev}"))?;
    let blob = entry
        .to_object(&repo)
        .and_then(|o| o.peel_to_blob())
        .map_err(|_| format!("{path} is not a file at {rev}"))?;
    if blob.is_binary() {
        return Err(format!("{path} is binary at {rev}"));
    }
    let short = short_hash(&commit.id().to_string());
    Ok(RevisionContent {
        title: format!("{short}:{path}"),
        text: String::from_utf8_lossy(blob.content()).into_owned(),
        // Detected from the path, so a file at a revision highlights exactly like its working-tree
        // twin — the whole point of showing it in the editor rather than a pager.
        language: crate::syntax::config_for_path(Path::new(path)).map(|c| c.name.to_string()),
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

/// One changed file in a repo: its combined staged+unstaged hunks vs HEAD (anchor order) plus
/// the LF-normalized working-tree bytes, so the caller can pull each add/modify hunk's preview
/// line without re-reading the file. `rel_path` is **repo**-relative, forward-slash — the changes
/// picker is scoped to a repo, so a changed file needn't sit under any workspace root.
pub struct ChangedFile {
    pub rel_path: String,
    pub hunks: Vec<DiffHunk>,
    pub working: Vec<u8>,
    /// True when the file has no committed (HEAD) blob *and* no index blob — i.e. wholly untracked.
    /// A staged-new file has an index blob, so it reads as tracked (`false`). Used by the
    /// Git-changes picker's `hide_untracked` filter.
    pub untracked: bool,
}

/// Diff every changed file in the repo at `repo_path` against HEAD (combined staged+unstaged),
/// opening the repo **once** — discovery, the HEAD tree, and the index are resolved a single time
/// and reused for every file, instead of re-discovering the repo per file (the slow part when a
/// repo has many changes). Untracked directories are not recursed: a wholly-new directory collapses
/// to one entry (git's default `git status`), which is a directory and skipped — only individual
/// changed files are diffable. Files with no net change are dropped. Best-effort: empty on any
/// libgit2 error.
///
/// **Repo-scoped, not root-scoped** (docs/git-phase-2.md decision 2). This used to take a workspace
/// root and drop every change outside that root's subtree, which silently hid a repo's changes
/// whenever a root was a subdirectory of it — the one place the root/repo ambiguity still lived.
/// `repo_path` is normally the workdir itself; discovery still runs, so a subdirectory resolves to
/// the same repo (and a linked worktree to its own).
pub fn changed_files_in_repo(repo_path: &Path) -> Vec<ChangedFile> {
    let mut out = Vec::new();
    let Ok(canonical) = repo_path.canonicalize() else {
        return out;
    };
    let Ok(repo) = git2::Repository::discover(&canonical) else {
        return out;
    };
    let Some(workdir) = repo.workdir().and_then(|w| w.canonicalize().ok()) else {
        return out;
    };

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
        let rel_path: String = repo_rel
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
        // A conflicted file is diffed against HEAD (its index entry is the conflict stages, not a
        // blob) and listed as a row per conflict block plus whatever the resolution has changed
        // outside them — see [`conflict_change_hunks`].
        let both = if entry.status().contains(git2::Status::CONFLICTED) {
            conflict_change_hunks(
                &ropey::Rope::from_str(&String::from_utf8_lossy(&working)),
                hunks_from_buffers(head.as_deref().unwrap_or(b""), &working),
            )
        } else {
            compose_both(&staged, &unstaged)
        };
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

    // ---- intra-line emphasis --------------------------------------------------------------------

    #[test]
    fn intraline_single_word_change() {
        let (old, new) = intraline_emphasis("let count = 1;", "let total = 1;").unwrap();
        assert_eq!(old, vec![(4, 9)]); // "count"
        assert_eq!(new, vec![(4, 9)]); // "total"
    }

    #[test]
    fn intraline_insertion_has_empty_old_side() {
        let (old, new) = intraline_emphasis("foo(a, c)", "foo(a, b, c)").unwrap();
        assert_eq!(old, vec![]);
        assert_eq!(new, vec![(7, 10)]); // "b, " (", b" re-anchored by the suffix trim)
    }

    #[test]
    fn intraline_deletion_has_empty_new_side() {
        let (old, new) = intraline_emphasis("foo(a, b, c)", "foo(a, c)").unwrap();
        assert_eq!(new, vec![]);
        assert_eq!(old.len(), 1);
    }

    #[test]
    fn intraline_multiple_ranges() {
        let (old, new) = intraline_emphasis("if alpha and gamma:", "if beta and delta:").unwrap();
        assert_eq!(old, vec![(3, 8), (13, 18)]); // "alpha", "gamma"
        assert_eq!(new, vec![(3, 7), (12, 17)]); // "beta", "delta"
    }

    #[test]
    fn intraline_small_gaps_merge() {
        // Both idents around the unchanged "." change: one merged range, not two.
        let (old, new) = intraline_emphasis("return a.b;", "return xx.yy;").unwrap();
        assert_eq!(old, vec![(7, 10)]); // "a.b" as one run
        assert_eq!(new, vec![(7, 12)]); // "xx.yy" as one run
    }

    #[test]
    fn intraline_rewritten_line_bails() {
        assert_eq!(
            intraline_emphasis("return compute(items)", "self.cache.clear()"),
            None,
            "mostly-rewritten pairs render as the plain whole-line tint"
        );
    }

    #[test]
    fn intraline_identical_pair_has_no_emphasis() {
        assert_eq!(
            intraline_emphasis("same text", "same text"),
            Some((vec![], vec![]))
        );
    }

    #[test]
    fn intraline_overlong_line_bails() {
        let long = "x".repeat(INTRALINE_MAX_BYTES + 1);
        assert_eq!(intraline_emphasis(&long, "x"), None);
    }

    #[test]
    fn intraline_snaps_to_word_boundaries() {
        // Shared prefix "user_" would otherwise leave a mid-word emphasis start.
        let (old, new) = intraline_emphasis("let user_name = 1;", "let user_email = 1;").unwrap();
        assert_eq!(old, vec![(4, 13)]); // the whole "user_name"
        assert_eq!(new, vec![(4, 14)]); // the whole "user_email"
    }

    #[test]
    fn intraline_multibyte_ranges_stay_on_char_boundaries() {
        // Appending "s" to "café": the prefix trim stops mid-word after the 2-byte "é", and the
        // word snap must grow the span back over it without splitting the char.
        let (old, new) = intraline_emphasis("greet café now", "greet cafés now").unwrap();
        assert_eq!(
            old,
            vec![],
            "pure insertion: nothing removed on the old side"
        );
        assert_eq!(new, vec![(6, 12)]); // the whole "cafés"
        for (s, e) in &new {
            assert!("greet cafés now".is_char_boundary(*s as usize));
            assert!("greet cafés now".is_char_boundary(*e as usize));
        }
    }

    #[test]
    fn intraline_whitespace_only_change() {
        // Widened gap: the two inserted spaces are the emphasis; nothing changed on the old side.
        let (old, new) = intraline_emphasis("a b", "a   b").unwrap();
        assert_eq!(old, vec![]);
        assert_eq!(new, vec![(2, 4)]);
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
        let repo = load_baseline(&file, &BaselineRevs::new())
            .repo
            .expect("repo resolved");

        write_index_blob(&repo, b"one\nTWO\n").expect("index write");

        let baseline = load_baseline(&file, &BaselineRevs::new());
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
        let repo = load_baseline(&file, &BaselineRevs::new())
            .repo
            .expect("repo resolved");
        assert!(
            load_baseline(&file, &BaselineRevs::new())
                .index_blob
                .is_none(),
            "untracked → no entry yet"
        );

        write_index_blob(&repo, b"hello\n").expect("index write");

        let baseline = load_baseline(&file, &BaselineRevs::new());
        assert_eq!(baseline.index_blob.as_deref(), Some(&b"hello\n"[..]));
        assert!(baseline.blob.is_none(), "still not in HEAD");
    }

    #[test]
    fn denormalize_crlf_round_trips_normalize() {
        let crlf = b"one\r\ntwo\r\n".to_vec();
        let lf = normalize_lf(crlf.clone());
        assert_eq!(denormalize_crlf(&lf), crlf);
    }

    // ---- conflict_regions (marker parsing) ------------------------------------------------------

    /// Lines of a region, as the resolver will read them.
    fn side(text: &str, range: std::ops::Range<u32>) -> Vec<String> {
        let rope = rope(text);
        range
            .map(|i| rope.line(i as usize).to_string().trim_end().to_string())
            .collect()
    }

    #[test]
    fn conflict_regions_reads_a_plain_block() {
        let text = "keep\n\
                    <<<<<<< HEAD\n\
                    mine\n\
                    =======\n\
                    theirs\n\
                    also theirs\n\
                    >>>>>>> feature/x\n\
                    tail\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 1);
        let r = &regions[0];
        assert_eq!((r.start_line, r.end_line), (1, 6));
        assert_eq!(side(text, r.ours.clone()), vec!["mine"]);
        assert_eq!(side(text, r.theirs.clone()), vec!["theirs", "also theirs"]);
        assert_eq!(r.base, None);
        // The labels are the only thing on screen that says whose side is whose.
        assert_eq!(r.ours_label, "HEAD");
        assert_eq!(r.theirs_label, "feature/x");
    }

    #[test]
    fn conflict_regions_reads_the_diff3_base_section() {
        // `merge.conflictstyle = diff3` adds the common ancestor. It is context, so it gets its own
        // range and is dropped by every resolution — but the sides either side of it must still
        // come out right, which is what a `|||||||`-unaware parser would get wrong.
        let text = "<<<<<<< HEAD\n\
                    mine\n\
                    ||||||| merged common ancestors\n\
                    original\n\
                    =======\n\
                    theirs\n\
                    >>>>>>> other\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 1);
        let r = &regions[0];
        assert_eq!(side(text, r.ours.clone()), vec!["mine"]);
        assert_eq!(
            side(text, r.base.clone().expect("base section")),
            vec!["original"]
        );
        assert_eq!(side(text, r.theirs.clone()), vec!["theirs"]);
    }

    #[test]
    fn conflict_regions_allows_an_empty_side() {
        // Their side deleted the lines: a real conflict flavour, and the one an off-by-one in the
        // range arithmetic turns into a panic or a stolen marker line.
        let text = "<<<<<<< HEAD\nmine\n=======\n>>>>>>> other\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 1);
        assert!(regions[0].theirs.is_empty());
        assert_eq!(side(text, regions[0].ours.clone()), vec!["mine"]);
    }

    #[test]
    fn conflict_regions_finds_every_block() {
        let text = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n\
                    middle\n\
                    <<<<<<< HEAD\nc\n=======\nd\n>>>>>>> x\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[1].start_line, 6);
    }

    #[test]
    fn conflict_regions_drops_an_unterminated_block() {
        // Half-resolved by hand: the closing marker is gone, so there is no longer a "their side"
        // to take. Reporting it anyway would offer a resolution that deletes the rest of the file.
        let text = "<<<<<<< HEAD\nmine\n=======\ntheirs\n";
        assert!(conflict_regions(&rope(text)).is_empty());
        // Nor is a block with no separator a conflict.
        assert!(conflict_regions(&rope("<<<<<<< HEAD\nmine\n>>>>>>> other\n")).is_empty());
    }

    #[test]
    fn conflict_regions_restarts_on_a_nested_marker() {
        // A recursive merge can nest one block inside another. The inner block is the one that can
        // be resolved by taking a side, so it wins.
        let text = "<<<<<<< outer\n\
                    stale\n\
                    <<<<<<< inner\n\
                    mine\n\
                    =======\n\
                    theirs\n\
                    >>>>>>> other\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].ours_label, "inner");
    }

    #[test]
    fn conflict_regions_ignores_rules_that_are_not_markers() {
        // The false positive that matters: prose. A setext heading underline and a horizontal rule
        // are longer than seven characters, which is exactly how git tells them apart too.
        let text = "Heading\n========\n<<<<<<<< not a marker\n>>>>>>>>\n";
        assert!(conflict_regions(&rope(text)).is_empty());
        // Seven with something glued on is not a marker either.
        assert!(conflict_regions(&rope("<<<<<<<x\nmine\n=======\nb\n>>>>>>> o\n")).is_empty());
    }

    #[test]
    fn conflict_regions_accepts_bare_markers() {
        // `git checkout --conflict=merge` and several merge drivers write markers with no label.
        let text = "<<<<<<<\nmine\n=======\ntheirs\n>>>>>>>\n";
        let regions = conflict_regions(&rope(text));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].ours_label, "");
    }

    // ---- resolve_conflicts (taking a side) ------------------------------------------------------

    /// Resolve every block in `text` by taking `side`.
    fn take(text: &str, side: ConflictSide) -> String {
        let rope = rope(text);
        let regions = conflict_regions(&rope);
        let selected: Vec<&ConflictRegion> = regions.iter().collect();
        resolve_conflicts(&rope, &selected, side)
    }

    const BLOCK: &str = "keep\n\
                         <<<<<<< HEAD\n\
                         mine\n\
                         =======\n\
                         theirs\n\
                         >>>>>>> other\n\
                         tail\n";

    #[test]
    fn taking_a_side_keeps_that_side_and_deletes_the_scenery() {
        assert_eq!(take(BLOCK, ConflictSide::Ours), "keep\nmine\ntail\n");
        assert_eq!(take(BLOCK, ConflictSide::Theirs), "keep\ntheirs\ntail\n");
        // Both keeps file order, so the result reads the way the block did.
        assert_eq!(
            take(BLOCK, ConflictSide::Both),
            "keep\nmine\ntheirs\ntail\n"
        );
    }

    #[test]
    fn the_diff3_base_section_is_never_kept() {
        let text = "<<<<<<< HEAD\nmine\n|||||||\noriginal\n=======\ntheirs\n>>>>>>> other\n";
        for side in [ConflictSide::Ours, ConflictSide::Theirs, ConflictSide::Both] {
            let out = take(text, side);
            assert!(
                !out.contains("original") && !out.contains("|||"),
                "the ancestor is context, not an outcome: {side:?} gave {out:?}"
            );
        }
    }

    #[test]
    fn resolving_several_blocks_keeps_the_text_between_them() {
        let text = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n\
                    middle\n\
                    <<<<<<< HEAD\nc\n=======\nd\n>>>>>>> x\n\
                    end\n";
        assert_eq!(take(text, ConflictSide::Ours), "a\nmiddle\nc\nend\n");
    }

    #[test]
    fn resolving_only_the_selected_blocks_leaves_the_others_alone() {
        // What a cursor-scoped take does: the second block is untouched, markers and all.
        let text =
            "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n<<<<<<< HEAD\nc\n=======\nd\n>>>>>>> x\n";
        let rope = rope(text);
        let regions = conflict_regions(&rope);
        let out = resolve_conflicts(&rope, &[&regions[0]], ConflictSide::Ours);
        assert_eq!(out, "a\n<<<<<<< HEAD\nc\n=======\nd\n>>>>>>> x\n");
    }

    #[test]
    fn an_empty_side_resolves_to_a_deletion() {
        // Their side deleted the lines; taking theirs must remove ours rather than keep a marker.
        let text = "before\n<<<<<<< HEAD\nmine\n=======\n>>>>>>> other\nafter\n";
        assert_eq!(take(text, ConflictSide::Theirs), "before\nafter\n");
    }

    #[test]
    fn resolving_preserves_a_missing_final_newline() {
        // The rope's own lines are copied, so a file that doesn't end in a newline still doesn't.
        let text = "<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> other\nlast line";
        assert_eq!(take(text, ConflictSide::Ours), "mine\nlast line");
    }

    // ---- mask_conflicts / conflict_change_hunks -------------------------------------------------

    /// A file conflicted in its first block, with an unrelated edit further down: the shape a
    /// half-resolved merge has, and the one the masking rule exists for.
    const PART_RESOLVED: &str = "<<<<<<< HEAD\n\
                                 mine\n\
                                 =======\n\
                                 theirs\n\
                                 >>>>>>> other\n\
                                 unchanged\n\
                                 edited by hand\n";

    #[test]
    fn masking_drops_the_diff_inside_a_block_and_keeps_the_rest() {
        let rope = rope(PART_RESOLVED);
        // What a diff against HEAD produces: the whole block reads as changed, plus the later edit.
        let head = b"mine\nunchanged\noriginal\n";
        let diff = hunks_from_buffers(head, PART_RESOLVED.as_bytes());
        assert!(
            diff.iter().any(|h| h.anchor_line < 5),
            "precondition: the block itself diffs against HEAD"
        );

        let masked = mask_conflicts(diff, &conflict_regions(&rope));
        assert!(
            masked.iter().all(|h| h.anchor_line > 4),
            "nothing inside the block survives: {masked:?}"
        );
        assert!(
            !masked.is_empty(),
            "but the hand edit below it does — that's what makes a resolved region show up"
        );
    }

    #[test]
    fn a_file_with_no_conflicts_left_keeps_its_whole_diff() {
        // The state after taking a side: markers gone, index still conflicted. Masking must be a
        // no-op here, or the resolution would be invisible in the gutter and the changes picker.
        let text = "mine\nunchanged\nresolved by hand\n";
        let diff = hunks_from_buffers(b"mine\nunchanged\noriginal\n", text.as_bytes());
        assert_eq!(
            mask_conflicts(diff.clone(), &conflict_regions(&rope(text))),
            diff
        );
    }

    #[test]
    fn the_changes_picker_lists_blocks_and_the_diff_around_them_in_order() {
        let rope = rope(PART_RESOLVED);
        let diff = hunks_from_buffers(b"mine\nunchanged\noriginal\n", PART_RESOLVED.as_bytes());
        let rows = conflict_change_hunks(&rope, diff);

        // One row for the block (anchored on its `<<<<<<<`), then the hand edit below it.
        assert_eq!(rows[0].anchor_line, 0);
        assert_eq!(rows[0].new_lines, 5, "the block is reported whole");
        assert!(
            rows.len() >= 2,
            "the diff outside the block rides along: {rows:?}"
        );
        assert!(
            rows.windows(2)
                .all(|w| w[0].anchor_line <= w[1].anchor_line),
            "rows are in line order"
        );
    }

    // ---- conflicted files have no diff baseline -------------------------------------------------

    /// Replace `name`'s stage-0 index entry with the three stages a stopped merge leaves.
    fn conflict_the_index(repo: &git2::Repository, name: &str, ours: &str, theirs: &str) {
        let entry = |id, stage: u16| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id,
            // The stage lives in bits 12–13 of the entry flags; that is what makes these conflict
            // entries rather than an ordinary staged file.
            flags: stage << 12,
            flags_extended: 0,
            path: name.as_bytes().to_vec(),
        };
        let mut index = repo.index().expect("index");
        index.remove_path(Path::new(name)).expect("drop stage 0");
        let ours = repo.blob(ours.as_bytes()).expect("blob");
        let theirs = repo.blob(theirs.as_bytes()).expect("blob");
        index.add(&entry(ours, 2)).expect("stage 2");
        index.add(&entry(theirs, 3)).expect("stage 3");
        index.write().expect("index write");
    }

    #[test]
    fn a_conflicted_path_is_diffed_against_head_with_nothing_staged() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\n");
        let repo = git2::Repository::open(dir.path()).unwrap();
        conflict_the_index(&repo, "src.rs", "mine\n", "theirs\n");

        let baseline = load_baseline(&file, &BaselineRevs::new());
        assert!(baseline.conflicted);
        // Both blobs hold HEAD, so the whole change set reads as unstaged against it. The
        // regression this guards is the alternative: with the index blob left absent the staged
        // diff would be `HEAD → ""`, painting the file as a staged whole-file deletion.
        assert_eq!(baseline.blob.as_deref(), Some(&b"one\ntwo\n"[..]));
        assert_eq!(baseline.index_blob, baseline.blob, "nothing is staged");
        assert!(
            baseline.staged_hunks.is_empty(),
            "no staged deletion of the whole file"
        );
        // The repo is still resolved — the resolve verbs and mark-resolved need it.
        assert!(baseline.repo.is_some());
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
        let baseline = load_baseline(file, &BaselineRevs::new());
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

        let baseline = load_baseline(&file, &BaselineRevs::new());
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
        let baseline = load_baseline(&file, &BaselineRevs::new());
        assert!(baseline.repo.is_some(), "repo discovered");
        assert!(baseline.blob.is_none(), "untracked → no committed blob");
        assert!(diff_hunks(baseline.blob.as_deref(), &rope("hello\nworld\n")).is_empty());
    }

    #[test]
    fn no_repo_resolves_to_empty_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("loose.rs");
        std::fs::write(&file, "hello\n").unwrap();
        let baseline = load_baseline(&file, &BaselineRevs::new());
        assert!(baseline.repo.is_none());
        assert!(baseline.blob.is_none());
    }

    // ---- compute_blame --------------------------------------------------------------------------

    #[test]
    fn blame_attributes_committed_lines_and_flags_edits() {
        let dir = tempfile::tempdir().unwrap();
        let file = repo_with_committed_file(dir.path(), "src.rs", "one\ntwo\nthree\n");
        let repo = load_baseline(&file, &BaselineRevs::new())
            .repo
            .expect("repo resolved");

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
        assert!(load_baseline(&file, &BaselineRevs::new()).repo.is_none());
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
    fn changed_files_in_repo_diffs_each_file_and_collapses_untracked_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(root, &[("a.rs", "one\ntwo\nthree\n"), ("clean.rs", "x\n")]);
        std::fs::write(root.join("a.rs"), "one\nTWO\nthree\n").unwrap(); // a modification
                                                                         // A wholly-new directory with several files (must collapse), plus a lone new file.
        std::fs::create_dir_all(root.join("junk")).unwrap();
        std::fs::write(root.join("junk/x.rs"), "x\n").unwrap();
        std::fs::write(root.join("junk/y.rs"), "y\n").unwrap();
        std::fs::write(root.join("loose.rs"), "new\n").unwrap();

        let mut changed = changed_files_in_repo(root);
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
    fn changed_files_in_repo_no_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        assert!(changed_files_in_repo(dir.path()).is_empty());
    }

    /// Repo-scoped, not root-scoped: asked about a repo, it reports every changed file in it —
    /// including ones a workspace root nested inside the repo would previously have hidden. Paths
    /// come back repo-relative, which is what the picker renders.
    #[test]
    fn changed_files_in_repo_reports_changes_outside_a_nested_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        repo_with_files(
            root,
            &[("sub/inside.rs", "one\n"), ("other/outside.rs", "two\n")],
        );
        std::fs::write(root.join("sub/inside.rs"), "ONE\n").unwrap();
        std::fs::write(root.join("other/outside.rs"), "TWO\n").unwrap();

        // Discovery from a subdirectory resolves the same repo, and the answer is the same:
        // whole-repo, repo-relative.
        for from in [root.to_path_buf(), root.join("sub")] {
            let mut changed = changed_files_in_repo(&from);
            changed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
            let paths: Vec<&str> = changed.iter().map(|c| c.rel_path.as_str()).collect();
            assert_eq!(
                paths,
                vec!["other/outside.rs", "sub/inside.rs"],
                "asked from {}",
                from.display()
            );
        }
    }

    #[test]
    fn repo_status_for_root_no_repo_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_status_for_root(dir.path()).is_none());
    }

    // ---- list_branches (branch picker) ----------------------------------------------------------

    /// Create `name` at HEAD without switching to it — a second branch to list.
    fn branch_at_head(dir: &Path, name: &str) {
        let repo = git2::Repository::open(dir).expect("open repo");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch(name, &head, false).expect("create branch");
    }

    #[test]
    fn list_branches_marks_head_and_reads_the_tip_subject() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_committed_file(dir.path(), "src.rs", "one\n");
        branch_at_head(dir.path(), "feature");

        let rows = list_branches(dir.path());
        assert_eq!(rows.len(), 2, "both local branches listed");

        let head: Vec<&BranchRow> = rows.iter().filter(|r| r.is_head).collect();
        assert_eq!(head.len(), 1, "exactly one branch is HEAD");
        assert!(rows[0].is_head, "HEAD sorts first regardless of name");
        assert_eq!(head[0].subject, "init", "tip commit's summary line");
        assert!(head[0].timestamp > 0);

        let feature = rows.iter().find(|r| r.name == "feature").unwrap();
        assert!(!feature.is_head);
        assert_eq!(
            feature.upstream, None,
            "a never-pushed branch has no upstream"
        );
        assert_eq!((feature.ahead, feature.behind), (0, 0));
        assert!(
            feature.checkout.is_none(),
            "no linked worktrees in this fixture"
        );
    }

    #[test]
    fn list_branches_unborn_head_has_no_branches() {
        // A fresh repo with no commit: the branch picker's empty state, and the case the
        // "+ Create" row exists for. Must be empty rather than an error.
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).expect("init");
        assert!(list_branches(dir.path()).is_empty());
    }

    #[test]
    fn list_branches_detached_head_marks_nothing_current() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_committed_file(dir.path(), "src.rs", "one\n");
        let repo = git2::Repository::open(dir.path()).unwrap();
        let oid = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(oid).expect("detach");

        let rows = list_branches(dir.path());
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].is_head,
            "detached HEAD is on no branch, so no row is current"
        );
    }

    #[test]
    fn list_branches_no_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_branches(dir.path()).is_empty());
    }

    #[test]
    fn list_branches_flags_a_branch_checked_out_in_a_linked_worktree() {
        // Git refuses the same branch in two worktrees, so the picker has to show this *before*
        // the user presses Enter on it.
        let dir = tempfile::tempdir().unwrap();
        repo_with_committed_file(dir.path(), "src.rs", "one\n");
        branch_at_head(dir.path(), "feature");

        let repo = git2::Repository::open(dir.path()).unwrap();
        let wt_path = dir.path().join("wt-feature");
        let branch_ref = repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        let reference = branch_ref.into_reference();
        opts.reference(Some(&reference));
        repo.worktree("feature", &wt_path, Some(&opts))
            .expect("add worktree");

        let rows = list_branches(dir.path());
        let feature = rows.iter().find(|r| r.name == "feature").unwrap();
        let held = feature
            .checkout
            .as_ref()
            .expect("feature is checked out in the linked worktree");
        assert!(
            held.path.ends_with("wt-feature"),
            "names the worktree holding it, got {}",
            held.path
        );
        assert_eq!(
            held.worktree, "feature",
            "carries the admin name, which is what binding and removal are keyed on"
        );
        assert!(!held.is_main);
        assert!(!held.is_current, "we are standing in the main checkout");

        // ...and the reverse direction: from inside the linked worktree, the *main* checkout's
        // branch must show as taken. `worktrees()` lists only linked ones, so this is the case
        // that would silently regress.
        let from_worktree = list_branches(&wt_path);
        let main_row = rows.iter().find(|r| r.is_head).unwrap();
        let seen = from_worktree
            .iter()
            .find(|r| r.name == main_row.name)
            .unwrap();
        let seen_checkout = seen
            .checkout
            .as_ref()
            .expect("the main worktree's branch reads as taken from the linked worktree");
        assert!(seen_checkout.is_main);
        assert!(
            seen_checkout.worktree.is_empty(),
            "the main checkout has no admin name — which is what `bind_worktree` reads as unbind"
        );

        // And the tree we are standing in reports itself, which the old
        // `branches_checked_out_elsewhere` deliberately skipped. A branch-keyed picker needs the
        // branch you are on to be a worktree row like any other, so "already here" and "another
        // tree has it" can be different sentences.
        let here = from_worktree
            .iter()
            .find(|r| r.name == "feature")
            .unwrap()
            .checkout
            .as_ref()
            .expect("the tree we are in holds `feature`");
        assert!(here.is_current);
    }

    // ---- branch_is_merged (delete pre-flight) ---------------------------------------------------

    #[test]
    fn branch_is_merged_is_true_for_a_branch_at_head() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_committed_file(dir.path(), "src.rs", "one\n");
        branch_at_head(dir.path(), "feature");
        assert!(
            branch_is_merged(dir.path(), "feature"),
            "same commit as HEAD — nothing would be lost"
        );
    }

    #[test]
    fn branch_is_merged_is_false_once_the_branch_has_its_own_commit() {
        let dir = tempfile::tempdir().unwrap();
        repo_with_committed_file(dir.path(), "src.rs", "one\n");
        let repo = git2::Repository::open(dir.path()).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        // A commit that only `feature` points at: deleting it would lose work.
        let tree = repo.find_tree(head.tree_id()).unwrap();
        let oid = repo
            .commit(None, &sig, &sig, "unmerged work", &tree, &[&head])
            .unwrap();
        repo.branch("feature", &repo.find_commit(oid).unwrap(), false)
            .unwrap();

        assert!(!branch_is_merged(dir.path(), "feature"));
    }
}
