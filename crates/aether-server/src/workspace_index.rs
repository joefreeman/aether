//! Workspace-wide file candidate cache. Single-shot walk for v1 — file watching slots in later
//! as another way to mutate `files`. Uses `ignore::WalkBuilder` so the candidate set respects
//! `.gitignore` / `.ignore` / hidden-file rules out of the box.
//!
//! Designed as a single service that consumers (pickers, buffer manager) attach to. The walk is
//! lazy: the first `files()` call runs it on a blocking task; subsequent calls reuse the cached
//! `Arc`. The cache survives `hide`, so reopening a picker doesn't re-walk. The walk is
//! hidden-*inclusive* (gitignore + `.git` excluded) so the Files picker can surface tracked
//! dot-entries; grep filters hidden files back out in-memory for its default (see
//! `grep::FileFilter`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Resolve `abs` to `(root_index, relative_path)` against the workspace's roots. Returns `None`
/// when `abs` is outside every root. The relative path uses forward slashes — UTF-8 only, which
/// matches every other place we ferry paths over the wire.
///
/// Kept here so the file walker and the buffer-picker candidate builder agree on the shape they
/// hand to the picker for the same on-disk file.
pub fn workspace_relative_parts(abs: &Path, roots: &[PathBuf]) -> Option<(u32, String)> {
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

/// The Buffers picker's row `display` — which is also the fuzzy-match haystack (`match_indices`
/// index into it). This is the *bare* workspace-relative path, matching how Files/Grep ship
/// `relative_path`: root identity is deliberately **not** part of the string, so it isn't part of
/// the fuzzy match and the client is free to prepend a disambiguated `"[root]: "` label as a
/// separate, non-highlighted span (see `aether_client::labels`). A file sitting *at* a root prints
/// the root's basename (an empty relative path is no haystack). Returns `None` if `abs` is outside
/// every root — the caller then falls back to the absolute path.
pub fn workspace_relative_display(abs: &Path, roots: &[PathBuf]) -> Option<String> {
    let (idx, rel) = workspace_relative_parts(abs, roots)?;
    if rel.is_empty() {
        let root = &roots[idx as usize];
        let basename = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
        return Some(basename.to_string());
    }
    Some(rel)
}

/// One file found by the workspace walk. Stores the root index + relative path separately so
/// the client can format the row with its own (disambiguated) root label. The matcher haystack
/// is `relative_path` alone — root identity is not part of the fuzzy match.
#[derive(Debug, Clone)]
pub struct CachedFile {
    /// Canonical absolute path on disk. The `picker/select` action returns this.
    pub abs: String,
    /// Index into the workspace's root list this file lives under.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`, forward-slash separated. Used as both the picker
    /// row's display tail and the matcher haystack.
    pub relative_path: String,
}

pub struct WorkspaceIndex {
    roots: Vec<PathBuf>,
    /// The one memoized walk, hidden-*inclusive* (gitignore + `.git` still excluded). The Files
    /// picker ranks it as-is so tracked dot-entries like `.circleci/workflows.yml` are reachable;
    /// grep filters hidden files back out in-memory for its default (see `grep::FileFilter`).
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

    /// Get the candidate cache (a hidden-*inclusive* walk), walking on first call or after an
    /// invalidation. Concurrent callers wait on the mutex — we don't want two simultaneous walks.
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
    // Hidden-*inclusive* (gitignore + `.git` still excluded) — the single memoized candidate set.
    // Grep hides dot-files back out in-memory for its default; the Files picker keeps them.
    walk_with(roots, false, true)
}

/// The workspace walk with the gitignore / hidden-file exclusions optionally relaxed. The
/// memoized index cache is always built with both exclusions on (`walk`); grep searches carrying
/// the `+ignored` / `+hidden` filters run this directly for a one-shot relaxed file list.
/// `.git` directories stay excluded even with `include_hidden` — searching repo internals is
/// never what those filters mean.
pub fn walk_with(
    roots: &[PathBuf],
    include_ignored: bool,
    include_hidden: bool,
) -> Vec<CachedFile> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_display_is_bare_relative_path() {
        // The row string / match haystack is the bare relative path — no root prefix. The client
        // adds the disambiguated "[root]: " label as a separate span, so multi-root doesn't change
        // the haystack (root identity stays out of the fuzzy match, like Files/Grep).
        let single = vec![PathBuf::from("/home/joe/work/repo")];
        assert_eq!(
            workspace_relative_display(Path::new("/home/joe/work/repo/src/main.rs"), &single)
                .as_deref(),
            Some("src/main.rs")
        );
        let multi = vec![
            PathBuf::from("/home/joe/work/api"),
            PathBuf::from("/home/joe/personal/api"),
        ];
        assert_eq!(
            workspace_relative_display(Path::new("/home/joe/personal/api/lib.rs"), &multi)
                .as_deref(),
            Some("lib.rs")
        );
    }

    #[test]
    fn buffer_display_at_root_uses_basename() {
        // A file whose path *is* a root has an empty relative path; the row falls back to the
        // root's basename so there's still a haystack.
        let roots = vec![PathBuf::from("/home/joe/work/repo")];
        assert_eq!(
            workspace_relative_display(Path::new("/home/joe/work/repo"), &roots).as_deref(),
            Some("repo")
        );
    }

    #[test]
    fn buffer_display_outside_all_roots_is_none() {
        let roots = vec![PathBuf::from("/home/joe/work/repo")];
        assert_eq!(
            workspace_relative_display(Path::new("/etc/hosts"), &roots),
            None
        );
    }
}
