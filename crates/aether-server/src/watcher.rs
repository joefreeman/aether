//! File-system watcher. One `notify::RecommendedWatcher` per server (lives in `ServerState`)
//! covers every loaded workspace's roots; an async task drains events and routes them
//! to buffers and pickers:
//!
//! - A buffer whose canonical path was modified gets either a silent reload (if clean) or
//!   the `externally_modified` flag (if dirty).
//! - A buffer whose canonical path was removed gets the `externally_deleted` flag.
//! - A buffer whose path is recreated has the deleted flag cleared and is treated as modified.
//! - Workspace-index and explorer-picker invalidations come from create/remove anywhere under
//!   a watched root; the picker layer chooses how to react (see `picker_refresh::*`).
//!
//! Self-writes (the server's own `buffer/save`) are filtered out by comparing on-disk mtime
//! against the buffer's recorded `last_modified_unix_ms`.
//!
//! Roots are watched lazily: `workspace/activate` calls [`watch_workspace_paths`] for each new
//! workspace's roots, so cold workspaces don't waste an inotify slot.
//!
//! Watches are **gitignore-aware and per-directory**, not one recursive watch per root. A
//! recursive watch walks *everything* — `target/` alone is >10k directories on this very repo,
//! which made first activation take seconds and every `cargo build` flood the event channel.
//! Instead we walk each root with the same `ignore` rules the workspace index uses and register a
//! NonRecursive watch per kept directory (110 vs 12k dirs here). `.git` internals are excluded
//! from that walk, so the pieces [`classify_git_change`] relies on (`HEAD`, `index`,
//! `packed-refs`, `refs/**`) get targeted watches of their own. In a **linked worktree** those
//! pieces are split across two directories — the worktree's own git dir and the family's common
//! dir — and both are watched; see [`push_git_targets`]. Directories created later are picked up
//! by a debounced re-walk ([`schedule_rescan`]) triggered from create/rename events.

use crate::handlers::PendingPushes;
use crate::handlers::{
    collect_buffer_state_pushes, explorer_dirs_in_workdirs, refresh_explorers_for_dirs,
    refresh_git_for_buffer, reload_buffer_locked,
};
use crate::state::{ServerState, SharedState};
use aether_protocol::BufferId;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;

/// The server's watcher plus the bookkeeping the per-directory scheme needs: which exact paths
/// are registered with the kernel (for idempotent re-walks and subtree unwatch) and the rescan
/// debounce flag.
pub struct WatcherHandle {
    inner: Mutex<WatcherInner>,
    /// True while a [`schedule_rescan`] is pending — collapses event bursts (`mkdir -p`, a git
    /// checkout creating many directories) into one re-walk.
    rescan_pending: AtomicBool,
}

struct WatcherInner {
    watcher: RecommendedWatcher,
    /// Every path currently registered with the kernel: kept directories plus single-file roots.
    watched: HashSet<PathBuf>,
}

impl WatcherHandle {
    fn lock(&self) -> MutexGuard<'_, WatcherInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::warn!("watcher mutex poisoned; continuing");
                p.into_inner()
            }
        }
    }
}

/// Spawn the per-server watcher task. Stashes the watcher handle in `ServerState::watcher` so
/// `workspace/activate` can register new roots, and starts an async loop that processes events
/// until the channel closes (when the watcher is dropped on shutdown).
///
/// At startup the watcher has no roots — workspaces register theirs in `workspace/activate`.
pub async fn spawn(state: SharedState) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    let handle = Arc::new(WatcherHandle {
        inner: Mutex::new(WatcherInner {
            watcher,
            watched: HashSet::new(),
        }),
        rescan_pending: AtomicBool::new(false),
    });
    {
        let mut s = state.lock().await;
        s.watcher = Some(handle);
    }

    tokio::spawn(async move {
        while let Some(res) = rx.recv().await {
            match res {
                Ok(event) => handle_event(&state, event).await,
                Err(e) => tracing::warn!(error = %e, "file watcher error"),
            }
        }
        tracing::debug!("file watcher event stream closed");
    });

    Ok(())
}

/// Register a workspace's roots with the server's live watcher. Called from `workspace/activate`
/// the first time a workspace is loaded, from `workspace/add_root` for newly-added roots, and by
/// [`schedule_rescan`] re-walks. Idempotent — already-registered directories are skipped — and
/// best-effort: losing the watch on one directory shouldn't fail an activation; that directory
/// just won't receive external-change notifications.
///
/// The walk (the potentially slow part, though it skips ignored trees) runs before the watcher
/// mutex is taken, so a re-walk doesn't stall event-side unwatch bookkeeping.
pub fn watch_workspace_paths(handle: &WatcherHandle, paths: &[PathBuf]) {
    let started = std::time::Instant::now();
    let targets = watch_targets(paths);
    let mut inner = handle.lock();
    let (mut added, mut failed) = (0usize, 0usize);
    for target in targets {
        if inner.watched.contains(&target) {
            continue;
        }
        match inner.watcher.watch(&target, RecursiveMode::NonRecursive) {
            Ok(()) => {
                inner.watched.insert(target);
                added += 1;
            }
            Err(e) => {
                failed += 1;
                tracing::debug!(path = %target.display(), error = %e, "failed to watch path");
            }
        }
    }
    if failed > 0 {
        tracing::warn!(
            failed,
            "some directories could not be watched (see debug logs); external changes there won't be noticed"
        );
    }
    tracing::debug!(
        roots = paths.len(),
        added,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "watch registration"
    );
}

/// Stop watching the given paths — each argument path *and* every registered directory under it.
/// Used by `workspace/remove_root`. Errors are logged but otherwise ignored — if the watcher had
/// already lost a path (e.g. the directory was deleted out from under us, which auto-removes the
/// kernel watch), there's nothing for the caller to recover from.
///
/// Overlapping-root caveat: this drops watches another still-loaded workspace may share; callers
/// that can, follow up with [`schedule_rescan`] to re-register anything still needed.
pub fn unwatch_workspace_paths(handle: &WatcherHandle, paths: &[PathBuf]) {
    let mut inner = handle.lock();
    for path in paths {
        let under: Vec<PathBuf> = inner
            .watched
            .iter()
            .filter(|p| p.starts_with(path))
            .cloned()
            .collect();
        for p in under {
            if let Err(e) = inner.watcher.unwatch(&p) {
                tracing::debug!(path = %p.display(), error = %e, "failed to unwatch path");
            }
            inner.watched.remove(&p);
        }
    }
}

/// Watch a single directory (no walk). Used for the parent of a just-opened file-backed buffer:
/// the ignore-aware walk skips gitignored trees, but a buffer the user opened *inside* one (a
/// generated file, say) still needs external-change notifications — silent reload and the
/// `externally_modified` flag route by buffer path, not by workspace. Watching the parent rather
/// than the file itself keeps the atomic-save pattern (write temp + rename over) visible — a
/// file-inode watch dies with the replaced inode. Idempotent and best-effort, like the rest.
pub fn watch_buffer_parent(handle: &WatcherHandle, file: &Path) {
    let Some(dir) = file.parent() else {
        return;
    };
    let mut inner = handle.lock();
    if inner.watched.contains(dir) {
        return;
    }
    match inner.watcher.watch(dir, RecursiveMode::NonRecursive) {
        Ok(()) => {
            inner.watched.insert(dir.to_path_buf());
        }
        Err(e) => {
            tracing::debug!(path = %dir.display(), error = %e, "failed to watch buffer dir");
        }
    }
}

/// Everything under `roots` that should carry a kernel watch: each root's non-ignored directories
/// (same `ignore` semantics as `workspace_index::walk_with` with both exclusions on), the git
/// internals any of those directories host, and single-file roots as themselves.
fn watch_targets(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        if root.is_file() {
            out.push(root.clone());
            continue;
        }
        let walker = ignore::WalkBuilder::new(root)
            .follow_links(false)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = entry.into_path();
            // Any kept directory hosting a repo (the root itself, or a nested one) gets targeted
            // watches on its git internals — the walk above excludes `.git` wholesale.
            push_git_targets(&dir, &mut out);
            out.push(dir);
        }
    }
    out
}

/// The git-internals watches for a directory hosting a repo.
///
/// Ordinary checkout (`.git` is a directory): `.git` itself (catches `HEAD`, `index`,
/// `packed-refs` — everything [`classify_git_change`] keys on at the top level) and the `refs/**`
/// directory tree (branch tips move under it on commit/checkout).
///
/// **Linked worktree (`.git` is a *file*)**: the interesting state is split in two, and watching
/// only one half sees nothing. `HEAD` and `index` live in the worktree's own git dir
/// (`<main>/.git/worktrees/<name>/`), while `refs/**` and `packed-refs` live in the *common* dir
/// shared with every other worktree of the repo. Both get watched. Without this a worktree got **no git-internals
/// watch at all**, so a commit or checkout made outside the editor was invisible in it.
///
/// A submodule's `.git` is a file too and takes the same path; its "common dir" is just its own
/// git dir, so it ends up correctly watched by the same code.
///
/// Caveat: for a linked worktree these targets sit *outside* the workspace root, so
/// [`unwatch_workspace_paths`] (which drops by `starts_with`) won't release them. Watches are
/// already never released in the ordinary case, so this adds no new class of leak.
fn push_git_targets(dir: &Path, out: &mut Vec<PathBuf>) {
    let git = dir.join(".git");
    if git.is_dir() {
        push_git_dir_targets(&git, out);
        return;
    }
    if !git.is_file() {
        return;
    }
    let Some(git_dir) = read_gitdir_pointer(&git) else {
        return;
    };
    // Per-worktree half: HEAD and index live directly in the worktree's git dir.
    out.push(git_dir.clone());
    // Shared half: refs/** and packed-refs live in the common dir.
    if let Some(common) = read_common_dir(&git_dir) {
        push_git_dir_targets(&common, out);
    }
}

/// Watch a git dir itself (top-level `HEAD` / `index` / `packed-refs`) plus its `refs/**` tree.
fn push_git_dir_targets(git_dir: &Path, out: &mut Vec<PathBuf>) {
    let refs = git_dir.join("refs");
    if refs.is_dir() {
        collect_dirs_recursive(&refs, out);
        out.push(refs);
    }
    out.push(git_dir.to_path_buf());
}

/// Resolve a `.git` *file* (`gitdir: <path>`) to the git dir it names.
///
/// The path is normally absolute; a relative one is resolved against the `.git` file's own
/// directory. **Note the `ignore` crate does not do this** — it uses the recorded path verbatim,
/// which is why we never *create* worktrees with `--relative-paths`.
/// Reading one someone else created is still supported.
fn read_gitdir_pointer(git_file: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(git_file).ok()?;
    let rest = content.trim().strip_prefix("gitdir:")?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(resolve_against(Path::new(rest), git_file.parent()?))
}

/// The common dir of a git dir: the `commondir` file's contents (usually the relative `../..`),
/// or the git dir itself when there is no such file (an ordinary, non-worktree repo).
fn read_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let rest = content.trim();
    if rest.is_empty() {
        return None;
    }
    Some(resolve_against(Path::new(rest), git_dir))
}

/// Absolutize `path` against `base` when relative, then canonicalize so `../..` segments collapse.
/// Watch targets and event paths have to compare equal, and events arrive already resolved.
fn resolve_against(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// All directories under `dir`, recursively (excluding `dir` itself). Only used for `refs/**`,
/// which is a handful of directories at most.
fn collect_dirs_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let p = entry.path();
            collect_dirs_recursive(&p, out);
            out.push(p);
        }
    }
}

/// Debounced re-walk of every loaded workspace's roots, registering watches for directories that
/// appeared since the last walk. Triggered by create/rename events (a fresh directory needs its
/// own watch — and anything created *inside* it before that watch attached is only found by
/// re-walking) and after `workspace/remove_root` (to re-register watches an overlapping root
/// still needs). The 300ms debounce collapses bursts — an unzip or `git checkout` creating many
/// directories costs one walk, not one per event. Ignored directories are never registered, so a
/// `cargo build` recreating `target/` triggers exactly one (cheap, ignore-filtered) walk and then
/// goes quiet.
pub fn schedule_rescan(state: SharedState, handle: Arc<WatcherHandle>) {
    if handle.rescan_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.rescan_pending.store(false, Ordering::Release);
        let roots: Vec<PathBuf> = {
            let s = state.lock().await;
            s.workspaces
                .values()
                .flat_map(|w| w.paths.iter().cloned())
                .collect()
        };
        let _ = tokio::task::spawn_blocking(move || watch_workspace_paths(&handle, &roots)).await;
    });
}

/// Drop registry entries (and kernel watches) whose directory no longer exists — deletions and
/// rename-away both leave stale exact-path entries behind. The kernel usually auto-removed the
/// watch already (inotify does on delete), so unwatch failures here are expected and logged at
/// debug. Only entries under one of `event_paths` are considered; the registry itself stays small
/// (hundreds), so the scan is cheap.
fn prune_dead_watches(handle: &WatcherHandle, event_paths: &[PathBuf]) {
    let mut inner = handle.lock();
    let dead: Vec<PathBuf> = inner
        .watched
        .iter()
        .filter(|w| event_paths.iter().any(|p| w.starts_with(p)) && !w.exists())
        .cloned()
        .collect();
    for p in dead {
        if let Err(e) = inner.watcher.unwatch(&p) {
            tracing::debug!(path = %p.display(), error = %e, "failed to unwatch dead path");
        }
        inner.watched.remove(&p);
    }
}

async fn handle_event(state: &SharedState, event: Event) {
    let kind = event.kind;
    // Decide once per event whether this is a create/modify/remove. `notify` gives us
    // sub-kinds (`ModifyKind::Data`, `Metadata`, `Name`...) that we collapse here.
    let category = match kind {
        EventKind::Create(_) => Category::Create,
        EventKind::Remove(_) => Category::Remove,
        EventKind::Modify(_) => Category::Modify,
        _ => return,
    };
    // Renames arrive as `Modify(Name)` — buffers keep seeing them as plain modifies (unchanged
    // behavior), but for the index and the watch registry they change the tree's *structure*,
    // like create/remove.
    let structural = matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );

    // Canonicalize the paths to match `buffer.canonical_path`. Remove events can't canonicalize
    // (file no longer exists), so we fall back to the raw path.
    let paths: Vec<PathBuf> = event
        .paths
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();

    let mut pushes: PendingPushes = Vec::new();
    let mut affected_dirs: HashSet<PathBuf> = HashSet::new();
    let mut index_should_invalidate = false;
    let mut watcher_handle: Option<Arc<WatcherHandle>> = None;

    {
        let mut s = state.lock().await;
        if structural {
            watcher_handle = s.watcher.clone();
        }

        // Drop everything under a repo we're mid-rewrite in: those files are about to be
        // reconciled in one deliberate pass, which both avoids reloading them one at a time and
        // keeps that pass's report of what changed accurate. See `ServerState::git_suppressed`.
        let paths: Vec<PathBuf> = paths
            .iter()
            .filter(|p| !is_suppressed(p, &s.git_suppressed))
            .cloned()
            .collect();
        if paths.is_empty() {
            return;
        }

        for path in &paths {
            if let Some(parent) = path.parent() {
                affected_dirs.insert(parent.to_path_buf());
            }
            if structural {
                index_should_invalidate = true;
            }

            // Buffers for one path share one document (workspaces with overlapping roots attach
            // to the same content), so handle the event once per *document* — the reload/flag
            // helpers fan their pushes out to every attached buffer's viewers. Dedupe by
            // document id defensively in case of a not-yet-unified pair.
            let mut seen_docs: std::collections::HashSet<crate::state::DocumentId> =
                std::collections::HashSet::new();
            for buf_id in s.buffers_for_path(path) {
                let Some(doc_id) = s.buffers.get(&buf_id).map(|b| b.document) else {
                    continue;
                };
                if !seen_docs.insert(doc_id) {
                    continue;
                }
                handle_buffer_event(&mut s, buf_id, path, category, &mut pushes);
            }
        }

        if index_should_invalidate {
            // Invalidate the workspace index for any workspace whose roots contain one of the
            // affected paths. Cheap — we only have a handful of workspaces loaded at most.
            for workspace in s.workspaces.values() {
                if paths
                    .iter()
                    .any(|p| workspace.paths.iter().any(|root| p.starts_with(root)))
                {
                    workspace.workspace_index.invalidate();
                }
            }
        }

        // External Git operations (commit / checkout / stage) touch files under `.git`. Refresh
        // the baseline + hunks of any open buffer in an affected repo so the gutter and inline
        // diff reflect the new HEAD without needing a buffer edit. (Only sees `.git` changes when
        // it's within a watched workspace root — the common repo-root-is-workspace-root case.)
        let git_workdirs: HashSet<PathBuf> =
            paths.iter().flat_map(|p| git_change_workdirs(p)).collect();
        if !git_workdirs.is_empty() {
            let affected: Vec<BufferId> = s
                .git_baseline
                .iter()
                .filter(|(_, b)| {
                    b.repo
                        .as_ref()
                        .is_some_and(|r| git_workdirs.contains(&r.workdir))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in affected {
                pushes.extend(refresh_git_for_buffer(&mut s, id));
            }
            // A commit / stage / checkout changes entry colours without touching any working-tree
            // file, so the parent-dir refresh above wouldn't catch open explorers in the repo.
            // Re-list them too by folding their listed dirs into `affected_dirs`.
            for dir in explorer_dirs_in_workdirs(&s, &git_workdirs) {
                affected_dirs.insert(dir);
            }
        }
        let picker_pushes = refresh_explorers_for_dirs(&mut s, &affected_dirs);
        pushes.extend(picker_pushes);
    }

    // Watch-registry maintenance, outside the state lock (it only takes the watcher mutex):
    // drop stale entries for vanished directories, and re-walk when a directory appeared so it
    // (and anything already created inside it) gets registered.
    if let Some(handle) = watcher_handle {
        prune_dead_watches(&handle, &paths);
        if paths.iter().any(|p| p.is_dir()) {
            schedule_rescan(state.clone(), handle);
        }
    }

    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
}

#[derive(Clone, Copy)]
enum Category {
    Create,
    Modify,
    Remove,
}

/// Is `path` inside a repo whose tree we're currently rewriting ourselves?
///
/// Containment, not equality: a checkout touches files anywhere under the workdir, and the `.git`
/// churn it produces sits under it too — both must be ignored, or the baseline-refresh path fires
/// hundreds of times for one operation.
fn is_suppressed(path: &Path, suppressed: &HashSet<PathBuf>) -> bool {
    suppressed.iter().any(|workdir| path.starts_with(workdir))
}

/// What a change to a path inside a `.git` directory means, before any filesystem lookup.
///
/// Split out from [`git_change_workdirs`] so the classification — which is all the interesting
/// rules — stays pure and unit-testable against synthetic paths, while the part that has to read
/// `worktrees/<name>/gitdir` off disk is a thin resolution step on top.
#[derive(Debug, PartialEq, Eq)]
enum GitChange {
    /// A change in a repo's own git dir. `workdir` is the parent of `.git`.
    Own {
        workdir: PathBuf,
        /// The file lives in the *common* dir, so every linked worktree of this repo sees it too
        /// (`refs/**`, `packed-refs`). `HEAD` and `index` are per-worktree and set this `false`.
        shared: bool,
    },
    /// A change in a linked worktree's private git dir, `<main>/.git/worktrees/<name>/…`. The
    /// worktree's own working directory is recorded on disk, not derivable from this path.
    Linked { main_git_dir: PathBuf, name: String },
}

/// Classify a filesystem event path against the git-internals layout. `None` for anything that
/// isn't a meaningful git file: `*.lock` temp files, `logs/`, `objects/`, `COMMIT_EDITMSG`, and
/// ordinary source files.
///
/// The shared/per-worktree split is git's own, from `common_list[]` in `path.c` —
fn classify_git_change(path: &Path) -> Option<GitChange> {
    let comps: Vec<_> = path.components().collect();
    let git_idx = comps.iter().position(|c| c.as_os_str() == ".git")?;
    let inner: PathBuf = comps[git_idx + 1..].iter().map(|c| c.as_os_str()).collect();
    let inner_str = inner.to_string_lossy();
    if inner_str.ends_with(".lock") {
        return None;
    }

    // `<main>/.git/worktrees/<name>/…` — a linked worktree's private git dir. Checked first: the
    // main repo's `.git` is the one carrying the `.git` component, but the change belongs to the
    // *worktree*, and attributing it to the main workdir is the bug this replaced.
    if let Ok(rest) = inner.strip_prefix("worktrees") {
        let mut rest = rest.components();
        let name = rest.next()?.as_os_str().to_str()?.to_string();
        let tail: PathBuf = rest.map(|c| c.as_os_str()).collect();
        if !is_meaningful_git_file(&tail) {
            return None;
        }
        return Some(GitChange::Linked {
            main_git_dir: comps[..=git_idx].iter().map(|c| c.as_os_str()).collect(),
            name,
        });
    }

    if !is_meaningful_git_file(&inner) {
        return None;
    }
    Some(GitChange::Own {
        workdir: comps[..git_idx].iter().map(|c| c.as_os_str()).collect(),
        shared: inner_str == "packed-refs" || inner.starts_with("refs"),
    })
}

/// The files worth reacting to inside a git dir: what commit / checkout / stage / fetch touch.
fn is_meaningful_git_file(inner: &Path) -> bool {
    let s = inner.to_string_lossy();
    s == "HEAD" || s == "index" || s == "packed-refs" || inner.starts_with("refs")
}

/// Every working directory whose git state a change to `path` invalidates.
///
/// Usually one. Two cases give more or different:
/// - a **shared** file (`refs/**`, `packed-refs`) is seen by the whole worktree family, so a
///   `git fetch` or a commit on a branch checked out elsewhere has to refresh all of them;
/// - a **linked worktree's** `HEAD`/`index` resolves to that worktree's workdir, read from
///   `worktrees/<name>/gitdir`, never the main one whose `.git` the path runs through.
///
/// The returned workdirs are what each buffer's cached `GitRepo.workdir` is keyed on.
fn git_change_workdirs(path: &Path) -> Vec<PathBuf> {
    match classify_git_change(path) {
        None => Vec::new(),
        Some(GitChange::Linked { main_git_dir, name }) => {
            worktree_workdir(&main_git_dir, &name).into_iter().collect()
        }
        Some(GitChange::Own {
            workdir,
            shared: false,
        }) => vec![workdir],
        Some(GitChange::Own {
            workdir,
            shared: true,
        }) => {
            let mut out = linked_worktree_workdirs(&workdir.join(".git"));
            out.push(workdir);
            out
        }
    }
}

/// The working directory of one linked worktree, from its admin entry. `worktrees/<name>/gitdir`
/// holds the path of the worktree's own `.git` *file*, so the workdir is that path's parent — the
/// CLI is keyed by path and libgit2 by admin id, and this file is the only bridge between them.
fn worktree_workdir(main_git_dir: &Path, name: &str) -> Option<PathBuf> {
    let admin = main_git_dir.join("worktrees").join(name);
    let content = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let recorded = content.trim();
    if recorded.is_empty() {
        return None;
    }
    let dot_git = resolve_against(Path::new(recorded), &admin);
    Some(dot_git.parent()?.to_path_buf())
}

/// Every linked worktree of the repo whose git dir is `main_git_dir`. Empty for a repo that has
/// none (no `worktrees/` directory), which is the overwhelmingly common case and costs one failed
/// `read_dir`.
fn linked_worktree_workdirs(main_git_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(main_git_dir.join("worktrees")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            worktree_workdir(main_git_dir, name.to_str()?)
        })
        .collect()
}

fn handle_buffer_event(
    s: &mut ServerState,
    buf_id: BufferId,
    path: &Path,
    category: Category,
    pushes: &mut PendingPushes,
) {
    match category {
        Category::Remove => {
            let Some(buf) = s.try_doc_of_mut(buf_id) else {
                return;
            };
            if buf.externally_deleted {
                return;
            }
            buf.externally_deleted = true;
            pushes.extend(collect_buffer_state_pushes(s, buf_id));
            // Refresh any open buffer picker so its status dot reflects the deletion live.
            pushes.extend(crate::handlers::refresh_buffer_pickers(s));
        }
        Category::Create | Category::Modify => {
            // Self-save filter: if disk mtime matches our recorded one, this is our own write.
            let disk_mtime = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);

            let (recorded_mtime, was_clean, was_deleted) = match s.try_doc_of(buf_id) {
                Some(b) => (b.last_modified_unix_ms, !b.dirty, b.externally_deleted),
                None => return,
            };

            if !was_deleted && disk_mtime.is_some() && disk_mtime == recorded_mtime {
                // Our own save (or a touch that didn't actually change anything).
                return;
            }

            if was_clean {
                match reload_buffer_locked(s, buf_id) {
                    Ok((_, reload_pushes)) => pushes.extend(reload_pushes),
                    Err(e) => {
                        tracing::warn!(?buf_id, error = ?e, "reload after watch event failed");
                    }
                }
            } else {
                let Some(buf) = s.try_doc_of_mut(buf_id) else {
                    return;
                };
                let modified_changed = !buf.externally_modified;
                let deleted_changed = buf.externally_deleted;
                buf.externally_modified = true;
                buf.externally_deleted = false;
                if modified_changed || deleted_changed {
                    pushes.extend(collect_buffer_state_pushes(s, buf_id));
                    // Refresh any open buffer picker so its status dot updates live.
                    pushes.extend(crate::handlers::refresh_buffer_pickers(s));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_git_change, git_change_workdirs, is_suppressed, watch_targets, GitChange,
    };
    use std::path::{Path, PathBuf};

    /// A main repo at `main/` with one linked worktree at `wt/`, wired the way git wires them:
    /// the worktree's `.git` is a *file* pointing at `main/.git/worktrees/<name>`, that admin
    /// directory's `gitdir` file points back at the worktree's `.git` file, and `commondir`
    /// points at the main `.git`. No git binary involved — these four files *are* the linkage.
    fn worktree_fixture(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize up front: macOS tempdirs live under a `/private` symlink, and the
        // resolution path canonicalizes, so the expected values have to as well.
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let main = base.join("main");
        let wt = base.join("wt");
        let admin = main.join(".git/worktrees").join(name);
        std::fs::create_dir_all(main.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        // git writes the relative `../..` here; resolving it is part of what's under test.
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        (dir, main, wt)
    }

    /// Build the directory tree the walk-target tests share:
    ///
    /// ```text
    /// root/
    ///   .git/refs/heads/        (empty `.git` marks the repo — the ignore crate only needs its
    ///   .gitignore  "target/"    presence, not a valid repository)
    ///   src/nested/
    ///   target/debug/
    ///   .hidden/
    /// ```
    fn repo_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        dir
    }

    /// Suppression is by containment: while we're rewriting a repo, *everything* under its
    /// workdir is ours to reconcile — the working-tree files and the `.git` churn alike. A
    /// sibling repo that merely shares a path prefix must keep receiving events.
    #[test]
    fn suppression_covers_a_workdir_subtree_but_not_its_siblings() {
        let suppressed: std::collections::HashSet<PathBuf> =
            [PathBuf::from("/src/aether")].into_iter().collect();

        for inside in [
            "/src/aether/a.rs",
            "/src/aether/crates/server/src/git.rs",
            "/src/aether/.git/HEAD",
            "/src/aether",
        ] {
            assert!(is_suppressed(Path::new(inside), &suppressed), "{inside}");
        }
        // `starts_with` is component-wise, so a sibling whose name merely extends the suppressed
        // one is untouched — the string-prefix bug this would otherwise be.
        for outside in [
            "/src/aether-worktrees/feature/a.rs",
            "/src/aetherium/a.rs",
            "/src",
        ] {
            assert!(!is_suppressed(Path::new(outside), &suppressed), "{outside}");
        }
        assert!(!is_suppressed(
            Path::new("/src/aether/a.rs"),
            &std::collections::HashSet::new()
        ));
    }

    #[test]
    fn watch_targets_skip_ignored_and_hidden_dirs() {
        // The whole point of per-directory watches: `target/` (gitignored) and dot-dirs never
        // get a kernel watch, so activation doesn't walk them and builds don't spam events.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        let targets = watch_targets(std::slice::from_ref(&root));
        for included in [root.clone(), root.join("src"), root.join("src/nested")] {
            assert!(targets.contains(&included), "missing {included:?}");
        }
        for excluded in [
            root.join("target"),
            root.join("target/debug"),
            root.join(".hidden"),
        ] {
            assert!(
                !targets.contains(&excluded),
                "should not watch {excluded:?}"
            );
        }
    }

    #[test]
    fn watch_targets_cover_git_internals() {
        // `.git` is excluded from the ignore-walk, so the bits `git_change_workdir` keys on
        // (`HEAD`/`index`/`packed-refs` live in `.git` itself; branch tips under `refs/**`)
        // need their own targeted watches.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        let targets = watch_targets(std::slice::from_ref(&root));
        for included in [
            root.join(".git"),
            root.join(".git/refs"),
            root.join(".git/refs/heads"),
        ] {
            assert!(targets.contains(&included), "missing {included:?}");
        }
        // But not the noisy internals a recursive watch used to cover.
        assert!(!targets.contains(&root.join(".git/objects")));
    }

    #[test]
    fn watch_targets_single_file_root_is_itself() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "hi\n").unwrap();
        assert_eq!(watch_targets(std::slice::from_ref(&file)), vec![file]);
    }

    #[test]
    fn watch_targets_include_nested_repo_git_internals() {
        // A repo nested inside the workspace root also gets its git-internals watches — buffers
        // in it resolve their baseline against the nested repo, so its HEAD/refs changes matter.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("vendor/lib/.git/refs")).unwrap();
        let targets = watch_targets(std::slice::from_ref(&root));
        assert!(targets.contains(&root.join("vendor/lib/.git")));
        assert!(targets.contains(&root.join("vendor/lib/.git/refs")));
    }

    #[test]
    fn detects_meaningful_git_files() {
        for (inner, shared) in [
            ("HEAD", false),
            ("index", false),
            ("packed-refs", true),
            ("refs/heads/main", true),
            ("refs/tags/v1", true),
        ] {
            let p = PathBuf::from(format!("/home/u/proj/.git/{inner}"));
            assert_eq!(
                classify_git_change(&p),
                Some(GitChange::Own {
                    workdir: PathBuf::from("/home/u/proj"),
                    shared,
                }),
                "{inner} should map to the workdir",
            );
        }
    }

    #[test]
    fn ignores_noise_and_non_git_paths() {
        for p in [
            "/home/u/proj/.git/index.lock",    // lock temp file
            "/home/u/proj/.git/logs/HEAD",     // reflog
            "/home/u/proj/.git/objects/ab/cd", // object write
            "/home/u/proj/.git/COMMIT_EDITMSG",
            "/home/u/proj/src/main.rs", // ordinary source file
            // The worktree admin files themselves: linkage, not repo state.
            "/home/u/proj/.git/worktrees/wt/gitdir",
            "/home/u/proj/.git/worktrees/wt/commondir",
            "/home/u/proj/.git/worktrees/wt/index.lock",
        ] {
            assert_eq!(
                classify_git_change(Path::new(p)),
                None,
                "{p} should be ignored"
            );
        }
    }

    #[test]
    fn classifies_linked_worktree_git_dir_by_name() {
        // The path runs through the *main* repo's `.git`, but the change belongs to the worktree.
        for inner in ["HEAD", "index", "refs/bisect/bad"] {
            let p = PathBuf::from(format!("/home/u/proj/.git/worktrees/feature/{inner}"));
            assert_eq!(
                classify_git_change(&p),
                Some(GitChange::Linked {
                    main_git_dir: PathBuf::from("/home/u/proj/.git"),
                    name: "feature".to_string(),
                }),
                "{inner} should be attributed to the worktree, not the main workdir",
            );
        }
    }

    #[test]
    fn linked_worktree_head_resolves_to_the_worktrees_own_workdir() {
        // The regression this whole split exists for: an agent commits in a worktree, and the
        // editor has to know *which* tree moved. Answering "the main one" is what it used to do.
        let (_dir, main, wt) = worktree_fixture("feature");
        assert_eq!(
            git_change_workdirs(&main.join(".git/worktrees/feature/HEAD")),
            vec![wt],
        );
    }

    #[test]
    fn shared_refs_change_fans_out_to_every_worktree() {
        // `refs/**` lives in the common dir, so a fetch (or a commit on a branch checked out
        // elsewhere) invalidates the whole family — not just the main checkout.
        let (_dir, main, wt) = worktree_fixture("feature");
        let mut got = git_change_workdirs(&main.join(".git/refs/remotes/origin/main"));
        got.sort();
        let mut want = vec![main.clone(), wt];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn per_worktree_files_do_not_fan_out() {
        // HEAD and index are per-worktree: a checkout in the main tree says nothing about the
        // others, and refreshing them would be wasted work on every keystroke-adjacent event.
        let (_dir, main, _wt) = worktree_fixture("feature");
        assert_eq!(
            git_change_workdirs(&main.join(".git/HEAD")),
            vec![main.clone()],
        );
        assert_eq!(git_change_workdirs(&main.join(".git/index")), vec![main]);
    }

    #[test]
    fn watch_targets_cover_a_linked_worktrees_split_git_state() {
        // Before this, `.git`-as-a-file returned early and a worktree got no git watch at all.
        // Both halves are needed: HEAD/index in the worktree's own git dir, refs in the common one.
        let (_dir, main, wt) = worktree_fixture("feature");
        std::fs::write(wt.join("a.rs"), "fn main() {}\n").unwrap();
        let targets = watch_targets(std::slice::from_ref(&wt));
        for included in [
            main.join(".git/worktrees/feature"), // per-worktree: HEAD, index
            main.join(".git"),                   // shared: packed-refs
            main.join(".git/refs"),              // shared: branch tips
            main.join(".git/refs/heads"),
        ] {
            assert!(targets.contains(&included), "missing {included:?}");
        }
    }
}
