//! File-system watcher. One `notify::RecommendedWatcher` per server (lives in `ServerState`)
//! covers every loaded project's roots recursively; an async task drains events and routes them
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
//! Roots are watched lazily: `project/activate` calls [`watch_project_paths`] for each new
//! project's roots, so cold projects don't waste an inotify slot.

use crate::handlers::{
    collect_buffer_state_pushes, explorer_dirs_in_workdirs, refresh_explorers_for_dirs,
    refresh_git_for_buffer, reload_buffer_locked,
};
use crate::state::{ServerState, SharedState};
use aether_protocol::envelope::Notification;
use aether_protocol::BufferId;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Spawn the per-server watcher task. Stashes the watcher handle in `ServerState::watcher` so
/// `project/activate` can register new roots, and starts an async loop that processes events
/// until the channel closes (when the watcher is dropped on shutdown).
///
/// At startup the watcher has no roots — projects register theirs in `project/activate`.
pub async fn spawn(state: SharedState) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    let handle = Arc::new(Mutex::new(watcher));
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

/// Register a project's roots with the server's live watcher. Called from `project/activate` the
/// first time a project is loaded, and from `project/add_root` for newly-added roots. Each root
/// gets a recursive watch. Errors are logged, not propagated — losing the watcher on one root
/// shouldn't fail the whole activation; the project just won't receive external-change
/// notifications for that root.
pub fn watch_project_paths(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    paths: &[PathBuf],
) {
    let mut watcher = match watcher.lock() {
        Ok(g) => g,
        Err(p) => {
            tracing::warn!("watcher mutex poisoned; skipping registration");
            p.into_inner()
        }
    };
    for path in paths {
        if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
            tracing::warn!(path = %path.display(), error = %e, "failed to watch path");
        }
    }
}

/// Stop watching the given paths. Used by `project/remove_root`. Errors are logged but
/// otherwise ignored — if the watcher had already lost the path (e.g. the directory was deleted
/// out from under us), there's nothing for the caller to recover from.
pub fn unwatch_project_paths(
    watcher: &Arc<Mutex<RecommendedWatcher>>,
    paths: &[PathBuf],
) {
    let mut watcher = match watcher.lock() {
        Ok(g) => g,
        Err(p) => {
            tracing::warn!("watcher mutex poisoned; skipping unregistration");
            p.into_inner()
        }
    };
    for path in paths {
        if let Err(e) = watcher.unwatch(path) {
            tracing::warn!(path = %path.display(), error = %e, "failed to unwatch path");
        }
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

    // Canonicalize the paths to match `buffer.canonical_path`. Remove events can't canonicalize
    // (file no longer exists), so we fall back to the raw path.
    let paths: Vec<PathBuf> = event
        .paths
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();

    let mut pushes: Vec<(mpsc::Sender<Notification>, Notification)> = Vec::new();
    let mut affected_dirs: HashSet<PathBuf> = HashSet::new();
    let mut index_should_invalidate = false;

    {
        let mut s = state.lock().await;

        for path in &paths {
            if let Some(parent) = path.parent() {
                affected_dirs.insert(parent.to_path_buf());
            }
            if matches!(category, Category::Create | Category::Remove) {
                index_should_invalidate = true;
            }

            // Plural: projects with overlapping roots can each have their own buffer for this
            // path, and every one of them needs the reload/flag — not just the first found.
            for buf_id in s.buffers_for_path(path) {
                handle_buffer_event(&mut s, buf_id, path, category, &mut pushes);
            }
        }

        if index_should_invalidate {
            // Invalidate the workspace index for any project whose roots contain one of the
            // affected paths. Cheap — we only have a handful of projects loaded at most.
            for project in s.projects.values() {
                if paths
                    .iter()
                    .any(|p| project.paths.iter().any(|root| p.starts_with(root)))
                {
                    project.workspace_index.invalidate();
                }
            }
        }

        // External Git operations (commit / checkout / stage) touch files under `.git`. Refresh
        // the baseline + hunks of any open buffer in an affected repo so the gutter and inline
        // diff reflect the new HEAD without needing a buffer edit. (Only sees `.git` changes when
        // it's within a watched project root — the common repo-root-is-project-root case.)
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
    pushes: &mut Vec<(mpsc::Sender<Notification>, Notification)>,
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
    use super::git_change_workdir;
    use std::path::{Path, PathBuf};

    #[test]
    fn detects_meaningful_git_files() {
        for inner in ["HEAD", "index", "packed-refs", "refs/heads/main", "refs/tags/v1"] {
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
            "/home/u/proj/.git/index.lock",   // lock temp file
            "/home/u/proj/.git/logs/HEAD",    // reflog
            "/home/u/proj/.git/objects/ab/cd", // object write
            "/home/u/proj/.git/COMMIT_EDITMSG",
            "/home/u/proj/src/main.rs",       // ordinary source file
        ] {
            assert_eq!(git_change_workdir(Path::new(p)), None, "{p} should be ignored");
        }
    }
}
