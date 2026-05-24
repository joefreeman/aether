//! Workspace-wide file candidate cache. Single-shot walk for v1 — file watching slots in later
//! as another way to mutate `files`. Uses `ignore::WalkBuilder` so the candidate set respects
//! `.gitignore` / `.ignore` / hidden-file rules out of the box.
//!
//! Designed as a single service that consumers (pickers, buffer manager) attach to. The walk is
//! lazy: the first `files()` call runs it on a blocking task; subsequent calls reuse the cached
//! `Arc`. The cache survives `hide`, so reopening a picker doesn't re-walk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

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
    files: OnceCell<Arc<Vec<CachedFile>>>,
}

impl WorkspaceIndex {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots, files: OnceCell::new() }
    }

    /// Get the candidate cache, walking on first call. Subsequent calls hand back the same `Arc`.
    pub async fn files(&self) -> Arc<Vec<CachedFile>> {
        self.files
            .get_or_init(|| async {
                let roots = self.roots.clone();
                let walked = tokio::task::spawn_blocking(move || walk(&roots))
                    .await
                    .unwrap_or_default();
                Arc::new(walked)
            })
            .await
            .clone()
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
                out.push(CachedFile { abs: abs.to_string(), display });
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
            let Some(abs) = abs_path.to_str() else { continue };
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
            out.push(CachedFile { abs: abs.to_string(), display });
        }
    }

    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}
