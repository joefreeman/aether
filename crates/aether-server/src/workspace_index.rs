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

/// Resolve `abs` to `(root_index, relative_path)` against the project's roots. Returns `None`
/// when `abs` is outside every root. The relative path uses forward slashes — UTF-8 only, which
/// matches every other place we ferry paths over the wire.
///
/// Kept here so the file walker and the buffer-picker candidate builder agree on the shape they
/// hand to the picker for the same on-disk file.
pub fn project_relative_parts(abs: &Path, roots: &[PathBuf]) -> Option<(u32, String)> {
    for (i, root) in roots.iter().enumerate() {
        if abs == root {
            return Some((i as u32, String::new()));
        }
        if let Ok(rel) = abs.strip_prefix(root) {
            return Some((i as u32, rel.to_str()?.to_string()));
        }
    }
    None
}

/// Legacy multi-root display string used by the Buffers picker (its protocol still sends a
/// flattened `display`, unlike Files/Grep which now ferry `path_index` + `relative_path`). For
/// multi-root projects, prefixes with the root's basename so two roots' `lib.rs`es don't
/// collide visually; for single-root, returns the bare relative path. Returns `None` if `abs`
/// is outside every root.
pub fn project_relative_display(abs: &Path, roots: &[PathBuf]) -> Option<String> {
    let (idx, rel) = project_relative_parts(abs, roots)?;
    let root = &roots[idx as usize];
    let root_name = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if rel.is_empty() {
        return Some(root_name.to_string());
    }
    if roots.len() > 1 && !root_name.is_empty() {
        Some(format!("{root_name}/{rel}"))
    } else {
        Some(rel)
    }
}

/// One file found by the workspace walk. Stores the root index + relative path separately so
/// the client can format the row with its own (disambiguated) root label. The matcher haystack
/// is `relative_path` alone — root identity is not part of the fuzzy match.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// Canonical absolute path on disk. The `picker/select` action returns this.
    pub abs: String,
    /// Index into the project's root list this file lives under.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`, forward-slash separated. Used as both the picker
    /// row's display tail and the matcher haystack.
    pub relative_path: String,
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
    walk_with(roots, false, false)
}

/// The workspace walk with the gitignore / hidden-file exclusions optionally relaxed. The
/// memoized index cache is always built with both exclusions on (`walk`); grep searches carrying
/// the `+ignored` / `+hidden` filters run this directly for a one-shot relaxed file list.
/// `.git` directories stay excluded even with `include_hidden` — searching repo internals is
/// never what those filters mean.
pub fn walk_with(roots: &[PathBuf], include_ignored: bool, include_hidden: bool) -> Vec<CachedFile> {
    let mut out: Vec<CachedFile> = Vec::new();

    for (path_index, root) in roots.iter().enumerate() {
        let path_index = path_index as u32;
        let root_basename = root.file_name().and_then(|s| s.to_str()).unwrap_or("");

        if root.is_file() {
            if let Some(abs) = root.to_str() {
                out.push(CachedFile {
                    abs: abs.to_string(),
                    path_index,
                    relative_path: root_basename.to_string(),
                });
            }
            continue;
        }

        let walker = ignore::WalkBuilder::new(root)
            .follow_links(false)
            .hidden(!include_hidden)
            .git_ignore(!include_ignored)
            .git_global(!include_ignored)
            .git_exclude(!include_ignored)
            .ignore(!include_ignored)
            .parents(!include_ignored)
            .filter_entry(|e| e.file_name() != ".git")
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let abs_path = entry.path();
            let Some(abs) = abs_path.to_str() else {
                continue;
            };
            let Some(rel) = abs_path.strip_prefix(root).ok().and_then(|p| p.to_str()) else {
                continue;
            };
            out.push(CachedFile {
                abs: abs.to_string(),
                path_index,
                relative_path: rel.to_string(),
            });
        }
    }

    // Sort by (path_index, relative_path) so rows for the same root cluster together. The
    // matcher then ranks within the haystack-by-relative-path; this just makes the empty-query
    // initial view deterministic and groupable.
    out.sort_by(|a, b| {
        a.path_index
            .cmp(&b.path_index)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });
    out
}
