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
//! from that walk, so the pieces `git_change_workdir` relies on (`HEAD`, `index`, `packed-refs`,
//! `refs/**`) get targeted watches of their own. Directories created later are picked up by a
//! debounced re-walk ([`schedule_rescan`]) triggered from create/rename events.

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

/// The git-internals watches for a directory hosting a `.git` dir: `.git` itself (catches `HEAD`,
/// `index`, `packed-refs` — everything `git_change_workdir` keys on at the top level) and the
/// `refs/**` directory tree (branch tips move under it on commit/checkout). Skipped when `.git`
/// is a file (worktrees/submodules point elsewhere; refreshing those repos' buffers still happens
/// on edit, just not on external git operations).
fn push_git_targets(dir: &Path, out: &mut Vec<PathBuf>) {
    let git = dir.join(".git");
    if !git.is_dir() {
        return;
    }
    let refs = git.join("refs");
    if refs.is_dir() {
        collect_dirs_recursive(&refs, out);
        out.push(refs);
    }
    out.push(git);
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

        for path in &paths {
            if let Some(parent) = path.parent() {
                affected_dirs.insert(parent.to_path_buf());
            }
            if structural {
                index_should_invalidate = true;
            }

            // Plural: workspaces with overlapping roots can each have their own buffer for this
            // path, and every one of them needs the reload/flag — not just the first found.
            for buf_id in s.buffers_for_path(path) {
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
            paths.iter().filter_map(|p| git_change_workdir(p)).collect();
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

/// If `path` is a meaningful file inside a `.git` directory — `HEAD`, `index`, `packed-refs`, or
/// anything under `refs/` (the things commit/checkout/stage touch) — return the repo's working
/// directory (the parent of `.git`). Ignores `*.lock` temp files and noise like `logs/` and
/// `objects/`. The returned workdir is what each buffer's cached `GitRepo.workdir` is keyed on.
fn git_change_workdir(path: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = path.components().collect();
    let git_idx = comps.iter().position(|c| c.as_os_str() == ".git")?;
    let inner: PathBuf = comps[git_idx + 1..].iter().map(|c| c.as_os_str()).collect();
    let inner_str = inner.to_string_lossy();
    if inner_str.ends_with(".lock") {
        return None;
    }
    let meaningful = inner_str == "HEAD"
        || inner_str == "index"
        || inner_str == "packed-refs"
        || inner.starts_with("refs");
    if !meaningful {
        return None;
    }
    Some(comps[..git_idx].iter().map(|c| c.as_os_str()).collect())
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
            let Some(buf) = s.buffers.get_mut(&buf_id) else {
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

            let (recorded_mtime, was_clean, was_deleted) = match s.buffers.get(&buf_id) {
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
                let Some(buf) = s.buffers.get_mut(&buf_id) else {
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
    use super::{git_change_workdir, watch_targets};
    use std::path::{Path, PathBuf};

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

    #[test]
    fn watch_targets_skip_ignored_and_hidden_dirs() {
        // The whole point of per-directory watches: `target/` (gitignored) and dot-dirs never
        // get a kernel watch, so activation doesn't walk them and builds don't spam events.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        let targets = watch_targets(&[root.clone()]);
        for included in [
            root.clone(),
            root.join("src"),
            root.join("src/nested"),
        ] {
            assert!(targets.contains(&included), "missing {included:?}");
        }
        for excluded in [
            root.join("target"),
            root.join("target/debug"),
            root.join(".hidden"),
        ] {
            assert!(!targets.contains(&excluded), "should not watch {excluded:?}");
        }
    }

    #[test]
    fn watch_targets_cover_git_internals() {
        // `.git` is excluded from the ignore-walk, so the bits `git_change_workdir` keys on
        // (`HEAD`/`index`/`packed-refs` live in `.git` itself; branch tips under `refs/**`)
        // need their own targeted watches.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        let targets = watch_targets(&[root.clone()]);
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
        assert_eq!(watch_targets(&[file.clone()]), vec![file]);
    }

    #[test]
    fn watch_targets_include_nested_repo_git_internals() {
        // A repo nested inside the workspace root also gets its git-internals watches — buffers
        // in it resolve their baseline against the nested repo, so its HEAD/refs changes matter.
        let dir = repo_fixture();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("vendor/lib/.git/refs")).unwrap();
        let targets = watch_targets(&[root.clone()]);
        assert!(targets.contains(&root.join("vendor/lib/.git")));
        assert!(targets.contains(&root.join("vendor/lib/.git/refs")));
    }

    #[test]
    fn detects_meaningful_git_files() {
        for inner in [
            "HEAD",
            "index",
            "packed-refs",
            "refs/heads/main",
            "refs/tags/v1",
        ] {
            let p = PathBuf::from(format!("/home/u/proj/.git/{inner}"));
            assert_eq!(
                git_change_workdir(&p),
                Some(PathBuf::from("/home/u/proj")),
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
        ] {
            assert_eq!(
                git_change_workdir(Path::new(p)),
                None,
                "{p} should be ignored"
            );
        }
    }
}
