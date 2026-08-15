//! On-disk backups of unsaved buffer contents (the hot-exit mechanism — see
//! `docs/unsaved-persistence.md`).
//!
//! A backup file is *exactly* the document's text (LF-normalised, our internal form) — no header,
//! no sidecar metadata. Identity is encoded in the path. **File backups are document-level**:
//! `files/<hash(canonical)>`, one per dirty document, with no workspace in the key — a document
//! shared by several workspaces has exactly one backup, and recover-on-open finds it no matter
//! which workspace opens the path first. **Scratch backups stay per-workspace**:
//! `scratch/<workspace>/<number>` — a scratch's number *is* per-workspace identity and a scratch
//! document is never shared. The two fixed top-level names also mean a workspace named "files"
//! can't collide with the shared directory.
//!
//! External-change detection leans on the backup file's own mtime rather than a stored timestamp:
//! see [`read`] and `docs/unsaved-persistence.md`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Backup path for a file-backed document: `<root>/files/<hash(canonical)>`. Workspace-agnostic —
/// the unsaved content belongs to the document, not to any workspace viewing it. The hash is
/// one-way (we never reverse it — the path comes from the open request or the session entry); a
/// 64-bit key is ample at personal scale and collisions only ever cost a single buffer's recovery.
pub fn file_backup_path(root: &Path, canonical: &Path) -> PathBuf {
    root.join("files").join(path_key(canonical))
}

/// Backup path for a scratch buffer: `<root>/scratch/<workspace>/<number>`. The number is the
/// scratch's stable per-workspace identity for the duration it holds unsaved content.
pub fn scratch_backup_path(root: &Path, workspace: &str, number: u32) -> PathBuf {
    root.join("scratch")
        .join(workspace)
        .join(number.to_string())
}

/// The scratch numbers `workspace` currently holds backups for on disk. Each one is a scratch with
/// unsaved content, whether or not it's loaded — which is what lets the switcher flag unsaved work
/// in a workspace nobody has activated yet (`ServerState::unsaved_buffer_count`; file backups are
/// attributed to workspaces through their session entries instead, since the file hash is one-way).
/// Empty (not an error) when the workspace has no backup directory, the usual case.
pub fn scratch_keys(root: &Path, workspace: &str) -> std::collections::HashSet<u32> {
    std::fs::read_dir(root.join("scratch").join(workspace))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                // Skip a flush's in-flight temp file (`write` renames it into place).
                .filter(|name| !name.starts_with(".tmp-"))
                .filter_map(|n| n.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Deterministic hex key for a canonical path. Uses the std `DefaultHasher` (SipHash with fixed
/// keys — stable within a build, and dependency-free); a hash change across a toolchain upgrade
/// would merely orphan old backups, which recover-on-open re-keys on the next open anyway.
pub fn path_key(canonical: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Write `content` to `path`, creating parent dirs. Atomic against tearing (tmp file + rename) but
/// **not** fsync'd — this runs on a short interval while typing, so durability is traded for cheap
/// writes; the most a crash loses is the last flush interval. Best-effort: errors are returned for
/// the caller to log, never to fail an edit.
pub fn write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    let tmp = path.with_file_name(format!(".tmp-{}-{file_name}", std::process::id()));
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Read a backup's content and its on-disk mtime (unix ms), or `None` if absent/unreadable. The
/// mtime is the external-change reference: a source file whose mtime is *newer* than this was
/// written externally since the backup was taken.
pub fn read(path: &Path) -> Option<(String, u64)> {
    let content = std::fs::read_to_string(path).ok()?;
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some((content, mtime))
}

/// Whether a backup exists at `path`. Used at restore to decide if a `Scratch` session entry still
/// has content worth bringing back.
pub fn exists(path: &Path) -> bool {
    path.exists()
}

/// Remove a backup, ignoring a missing file. Called on save / close / undo-to-clean.
pub fn delete(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_key_is_deterministic_and_path_specific() {
        let a = Path::new("/work/src/main.rs");
        let b = Path::new("/work/src/lib.rs");
        assert_eq!(path_key(a), path_key(a), "same path → same key");
        assert_ne!(path_key(a), path_key(b), "different paths → different keys");
        assert_eq!(path_key(a).len(), 16, "fixed-width hex key");
    }

    #[test]
    fn file_and_scratch_paths_live_in_distinct_subdirs() {
        let root = Path::new("/state/backups");
        let f = file_backup_path(root, Path::new("/work/a.rs"));
        let s = scratch_backup_path(root, "work", 3);
        assert!(f.starts_with(root.join("files")));
        assert_eq!(s, root.join("scratch").join("work").join("3"));
        // The fixed top-level names mean a workspace literally named "files" can't collide with
        // the shared file-backup directory: its scratches live under scratch/files/.
        assert_eq!(
            scratch_backup_path(root, "files", 1),
            root.join("scratch").join("files").join("1")
        );
    }

    #[test]
    fn write_read_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path exercises the create-parent branch.
        let path = file_backup_path(dir.path(), Path::new("/work/a.rs"));
        assert!(!exists(&path));
        write(&path, "hello\nworld\n").unwrap();
        assert!(exists(&path));
        let (content, mtime) = read(&path).expect("backup readable");
        assert_eq!(content, "hello\nworld\n");
        assert!(mtime > 0, "an mtime is captured");
        delete(&path);
        assert!(!exists(&path));
        assert!(read(&path).is_none(), "deleted backup reads as None");
    }

    #[test]
    fn write_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = scratch_backup_path(dir.path(), "work", 1);
        write(&path, "first").unwrap();
        write(&path, "second").unwrap();
        assert_eq!(read(&path).unwrap().0, "second");
        // No stray tmp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file cleaned up by rename");
    }
}
