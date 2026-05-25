//! File-system watcher. One global `notify::RecommendedWatcher` covers every project root
//! recursively; an async task drains events and routes them to buffers and pickers:
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

use crate::handlers::{
    collect_buffer_state_pushes, refresh_explorers_for_dirs, reload_buffer_locked,
};
use crate::state::{ServerState, SharedState};
use aether_protocol::envelope::Notification;
use aether_protocol::BufferId;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Spawn the watcher task. Reads `project_paths` from `state`, sets up a recursive watch on
/// each, and starts an async loop that processes events until the channel closes (when the
/// `Watcher` is dropped on shutdown).
pub async fn spawn(state: SharedState) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;

    let project_paths = {
        let s = state.lock().await;
        s.project_paths.clone()
    };
    for path in &project_paths {
        if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
            tracing::warn!(path = %path.display(), error = %e, "failed to watch path");
        }
    }

    tokio::spawn(async move {
        // Keep the watcher alive for the lifetime of this task.
        let _watcher = watcher;
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

            let Some(buf_id) = buffer_for_path(&s, path) else {
                continue;
            };
            handle_buffer_event(&mut s, buf_id, path, category, &mut pushes);
        }

        if index_should_invalidate {
            s.workspace_index.invalidate();
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

fn buffer_for_path(s: &ServerState, path: &Path) -> Option<BufferId> {
    s.buffers.iter().find_map(|(id, b)| {
        if b.canonical_path.as_deref() == Some(path) {
            Some(*id)
        } else {
            None
        }
    })
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
                }
            }
        }
    }
}

