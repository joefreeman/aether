//! Workspace-wide file candidate cache. Single-shot walk for v1 — file watching slots in later
//! as another way to mutate `files`. Uses `ignore::WalkBuilder` so the candidate set respects
//! `.gitignore` / `.ignore` / hidden-file rules out of the box.
//!
//! Designed as a single service that consumers (pickers, buffer manager) attach to. The walk is
//! lazy: the first `files()` call runs it on a blocking task; subsequent calls reuse the cached
//! `Arc`. The cache survives `hide`, so reopening a picker doesn't re-walk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Format an absolute path as the picker display string: project-relative, prefixed with the
/// root's basename for multi-root projects. Returns `None` if `abs` is outside every root —
/// callers should treat that as "no display available" rather than fall back to the absolute
/// path, since that would be inconsistent with the file walker's output.
///
/// Kept here so the file walker and the buffer-picker candidate builder produce identical
/// display strings for the same on-disk file.
pub fn project_relative_display(abs: &Path, roots: &[PathBuf]) -> Option<String> {
    let multi_root = roots.len() > 1;
    for root in roots {
        let root_name = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if abs == root {
            return Some(root_name.to_string());
        }
        if let Ok(rel) = abs.strip_prefix(root) {
            let rel = rel.to_str()?;
            return Some(if multi_root && !root_name.is_empty() {
                format!("{root_name}/{rel}")
            } else {
                rel.to_string()
            });
        }
    }
    None
}

/// One file found by the workspace walk.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// Canonical absolute path on disk. The `picker/select` action returns this.
    pub abs: String,
    /// Display string used for both rendering and fuzzy matching. Project-relative; for
    /// multi-root projects, prefixed with the root's last path component so two roots'
    /// `lib.rs`es don't collide visually.
    pub display: String,
}

pub struct WorkspaceIndex {
    roots: Vec<PathBuf>,
    cache: Mutex<Option<Arc<Vec<CachedFile>>>>,
    /// Set by the file-watcher when files are created or removed under a root. Consumed by
    /// the next `files()` call, which drops the cache and re-walks. Cheap to set (atomic),
    /// no-op for the common "no FS activity since last access" case.
    invalidated: AtomicBool,
}

impl WorkspaceIndex {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            cache: Mutex::new(None),
            invalidated: AtomicBool::new(false),
        }
    }

    /// Get the candidate cache, walking on first call or after an invalidation. Concurrent
    /// callers wait on the mutex — we don't want two simultaneous walks.
    pub async fn files(&self) -> Arc<Vec<CachedFile>> {
        let mut guard = self.cache.lock().await;
        if self.invalidated.swap(false, Ordering::AcqRel) {
            *guard = None;
        }
        if let Some(arc) = guard.as_ref() {
            return arc.clone();
        }
        let roots = self.roots.clone();
        let walked = tokio::task::spawn_blocking(move || walk(&roots))
            .await
            .unwrap_or_default();
        let arc = Arc::new(walked);
        *guard = Some(arc.clone());
        arc
    }

    /// Mark the cache stale. The next `files()` call re-walks. Sync so it's callable from any
    /// context (in particular the file-watcher event handler, which already holds the
    /// `ServerState` lock and shouldn't take more `await` points than necessary).
    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
    }
}

fn walk(roots: &[PathBuf]) -> Vec<CachedFile> {
    let multi_root = roots.len() > 1;
    let mut out: Vec<CachedFile> = Vec::new();

    for root in roots {
        let root_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if root.is_file() {
            if let Some(abs) = root.to_str() {
                let display = if multi_root && !root_name.is_empty() {
                    root_name.clone()
                } else {
                    root_name.clone()
                };
                out.push(CachedFile {
                    abs: abs.to_string(),
                    display,
                });
            }
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
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let abs_path = entry.path();
            let Some(abs) = abs_path.to_str() else {
                continue;
            };
            let rel = abs_path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .unwrap_or(abs);
            let display = if multi_root && !root_name.is_empty() {
                format!("{root_name}/{rel}")
            } else {
                rel.to_string()
            };
            out.push(CachedFile {
                abs: abs.to_string(),
                display,
            });
        }
    }

    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}
