//! On-disk backups of unsaved buffer contents (the hot-exit mechanism — see
//! `docs/unsaved-persistence.md`).
//!
//! A backup file is *exactly* the buffer's text (LF-normalised, our internal form) — no header, no
//! sidecar metadata. Identity is encoded in the path: file backups live under `<workspace>/files/`
//! keyed by a hash of the canonical path; scratch backups under `<workspace>/scratch/` keyed by the
//! per-workspace number. The `files/` vs `scratch/` split is the file-vs-scratch discriminant.
//!
//! External-change detection leans on the backup file's own mtime rather than a stored timestamp:
//! see [`read`] and `docs/unsaved-persistence.md`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Backup path for a file-backed buffer: `<root>/<workspace>/files/<hash(canonical)>`. The hash is
/// one-way (we never reverse it — the path comes from the open request or the session entry); a
/// 64-bit key is ample at personal scale and collisions only ever cost a single buffer's recovery.
pub fn file_backup_path(root: &Path, workspace: &str, canonical: &Path) -> PathBuf {
    root.join(workspace).join("files").join(path_key(canonical))
}

/// Backup path for a scratch buffer: `<root>/<workspace>/scratch/<number>`. The number is the
/// scratch's stable per-workspace identity for the duration it holds unsaved content.
pub fn scratch_backup_path(root: &Path, workspace: &str, number: u32) -> PathBuf {
    root.join(workspace)
        .join("scratch")
        .join(number.to_string())
}

/// Deterministic hex key for a canonical path. Uses the std `DefaultHasher` (SipHash with fixed
/// keys — stable within a build, and dependency-free); a hash change across a toolchain upgrade
/// would merely orphan old backups, which recover-on-open re-keys on the next open anyway.
fn path_key(canonical: &Path) -> String {
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
        let f = file_backup_path(root, "work", Path::new("/work/a.rs"));
        let s = scratch_backup_path(root, "work", 3);
        assert!(f.starts_with(root.join("work").join("files")));
        assert_eq!(s, root.join("work").join("scratch").join("3"));
    }

    #[test]
    fn write_read_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path exercises the create-parent branch.
        let path = file_backup_path(dir.path(), "work", Path::new("/work/a.rs"));
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
