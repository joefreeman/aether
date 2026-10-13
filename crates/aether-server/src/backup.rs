//! On-disk backups of unsaved buffer contents (the hot-exit mechanism).
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
//! see [`read`].

use std::path::{Path, PathBuf};

/// Backup path for a file-backed document: `<root>/files/<hash(canonical)>`. Workspace-agnostic —
/// the unsaved content belongs to the document, not to any workspace viewing it. The hash is
/// one-way (we never reverse it — the path comes from the open request or the session entry); a
/// 64-bit key is ample at personal scale and collisions only ever cost a single buffer's recovery.
///
/// A backup written under the key older builds used ([`legacy_path_key`]) is moved to the current
/// key here, so every caller sees it at the path it computes, whichever it does first.
pub fn file_backup_path(root: &Path, canonical: &Path) -> PathBuf {
    let files = root.join("files");
    let path = files.join(path_key(canonical));
    let legacy = files.join(legacy_path_key(canonical));
    if legacy.exists() && !path.exists() {
        match std::fs::rename(&legacy, &path) {
            Ok(()) => {
                let _ = std::fs::rename(base_path(&legacy), base_path(&path));
            }
            Err(e) => {
                tracing::warn!(
                    path = %canonical.display(),
                    error = %e,
                    "could not re-key a file backup"
                )
            }
        }
    }
    path
}

/// Backup path for a scratch buffer: `<root>/scratch/<workspace>/<number>`. The number is the
/// scratch's stable per-workspace identity for the duration it holds unsaved content.
pub fn scratch_backup_path(root: &Path, workspace: &str, number: u32) -> PathBuf {
    root.join("scratch")
        .join(workspace)
        .join(number.to_string())
}

/// Backup path for a shell: `<root>/shell/<workspace>/<number>`. What it holds is not a document
/// but a snapshot of the whole shell — transcript, runs, directory, assignments, the input — as
/// JSON, keyed the way a scratch is: by the per-workspace number that is the shell's identity.
pub fn shell_backup_path(root: &Path, workspace: &str, number: u32) -> PathBuf {
    root.join("shell").join(workspace).join(number.to_string())
}

/// Backup path for an agent conversation: `<root>/agent/<workspace>/<number>`. Holds a snapshot of
/// the whole conversation — its blocks and their text, the unsent input, and which agent and ACP
/// session it belonged to — as JSON, keyed the way a shell's is.
pub fn agent_backup_path(root: &Path, workspace: &str, number: u32) -> PathBuf {
    root.join("agent").join(workspace).join(number.to_string())
}

/// Move a renamed workspace's per-workspace backups — scratches, shells, conversations — to its new
/// name. Restore looks them up by the workspace's current name, so left behind they vanished on the
/// next start: still on disk, but listed nowhere and counted as nobody's unsaved work. File backups
/// are keyed by path, not workspace, and stay put.
///
/// A number already present under the new name (left by an earlier workspace of that name) is
/// kept, and the old one left where it is rather than overwriting it.
pub fn rename_workspace(root: &Path, old: &str, new: &str) -> std::io::Result<()> {
    for kind in ["scratch", "shell", "agent"] {
        let from = root.join(kind).join(old);
        if !from.exists() {
            continue;
        }
        let to = root.join(kind).join(new);
        if !to.exists() {
            std::fs::rename(&from, &to)?;
            continue;
        }
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if !target.exists() {
                std::fs::rename(entry.path(), target)?;
            }
        }
    }
    Ok(())
}

/// The conversation numbers `workspace` holds snapshots for on disk.
pub fn agent_keys(root: &Path, workspace: &str) -> std::collections::HashSet<u32> {
    numbered_keys(&root.join("agent").join(workspace))
}

/// The numbers a snapshot directory holds, ignoring in-progress writes. Shared by the shell's keys
/// and the agent's: they differ only in which directory they read.
fn numbered_keys(dir: &Path) -> std::collections::HashSet<u32> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|name| !name.starts_with(".tmp-"))
                .filter_map(|n| n.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The shell numbers `workspace` holds snapshots for on disk — the shells that can come back.
pub fn shell_keys(root: &Path, workspace: &str) -> std::collections::HashSet<u32> {
    numbered_keys(&root.join("shell").join(workspace))
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

/// Deterministic hex key for a canonical path: [`content_hash`] over the path's bytes, which is
/// fixed by its definition rather than by the toolchain. The key is the only link from a path to
/// its backup, so a key that changed between builds would orphan every file backup.
pub fn path_key(canonical: &Path) -> String {
    format!(
        "{:016x}",
        content_hash([canonical.as_os_str().as_encoded_bytes()])
    )
}

/// The key earlier builds used: std's `DefaultHasher`, which isn't stable across Rust releases.
/// Only [`file_backup_path`] consults it, to move a backup written under it.
fn legacy_path_key(canonical: &Path) -> String {
    use std::hash::{Hash, Hasher};
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
    // A fresh name per write, created exclusively: two flushes of one backup (the periodic one and
    // the final one) can overlap, and a fixed name let one rename the other's half-written file —
    // or follow a symlink planted at a guessable name.
    let tmp = path.with_file_name(format!(".tmp-{}-{file_name}", uuid::Uuid::new_v4()));
    let written = (|| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?
            .write_all(content.as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A stable 64-bit FNV-1a hash of `chunks`, concatenated. Stable across builds and toolchains, which
/// `DefaultHasher` doesn't promise — it is written to disk and compared on a later run.
pub fn content_hash<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in chunks {
        for &byte in chunk {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Where a file backup records the disk text its edits started from: `<backup>.base`.
fn base_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".base");
    path.with_file_name(name)
}

/// [`write`], plus — for a file's backup — the [`content_hash`] of the disk text the unsaved edits
/// started from. That, not the backup's mtime, is what says on the next run whether the file was
/// changed by something else meanwhile: the backup is rewritten as typing goes on, so a change
/// made on disk early in a session was older than the latest backup and went unnoticed, and a
/// tool that restores old mtimes (`cp -p`, `rsync -t`, `tar`) was never newer at all.
pub fn write_with_base(path: &Path, content: &str, base: Option<u64>) -> std::io::Result<()> {
    write(path, content)?;
    match base {
        Some(hash) => write(&base_path(path), &format!("{hash:016x}")),
        None => {
            let _ = std::fs::remove_file(base_path(path));
            Ok(())
        }
    }
}

/// The disk-text hash [`write_with_base`] recorded beside `path`, if any. Backups written before it
/// existed have none, and fall back to comparing mtimes.
pub fn read_base(path: &Path) -> Option<u64> {
    let hex = std::fs::read_to_string(base_path(path)).ok()?;
    u64::from_str_radix(hex.trim(), 16).ok()
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
    let _ = std::fs::remove_file(base_path(path));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace rename carries its scratches, shells and conversations to the new name, where
    /// restore looks for them, and never overwrites a number the new name already holds.
    #[test]
    fn rename_workspace_moves_every_per_workspace_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&scratch_backup_path(root, "old", 1), "scratch").unwrap();
        write(&shell_backup_path(root, "old", 2), "shell").unwrap();
        write(&agent_backup_path(root, "old", 3), "agent").unwrap();
        // Left by an earlier workspace of the new name.
        write(&agent_backup_path(root, "new", 3), "earlier").unwrap();
        write(&agent_backup_path(root, "old", 4), "agent 4").unwrap();

        rename_workspace(root, "old", "new").unwrap();

        assert_eq!(scratch_keys(root, "new"), [1].into());
        assert_eq!(shell_keys(root, "new"), [2].into());
        assert_eq!(agent_keys(root, "new"), [3, 4].into());
        assert_eq!(
            read(&agent_backup_path(root, "new", 3)).unwrap().0,
            "earlier"
        );
        assert!(scratch_keys(root, "old").is_empty());
        assert!(shell_keys(root, "old").is_empty());
        // The colliding one stays where it was rather than being lost.
        assert_eq!(agent_keys(root, "old"), [3].into());
    }

    #[test]
    fn path_key_is_deterministic_and_path_specific() {
        let a = Path::new("/work/src/main.rs");
        let b = Path::new("/work/src/lib.rs");
        assert_eq!(path_key(a), path_key(a), "same path → same key");
        assert_ne!(path_key(a), path_key(b), "different paths → different keys");
        assert_eq!(path_key(a).len(), 16, "fixed-width hex key");
        // Pinned: a key that moved between builds would orphan every file backup.
        assert_eq!(path_key(a), "afc1dd9772afd097");
    }

    /// A backup written under the old toolchain-dependent key, with its base stamp, is found at
    /// the current key, and one already at the current key is never replaced by it.
    #[test]
    fn a_backup_under_the_legacy_key_is_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let canonical = Path::new("/work/a.rs");
        let legacy = root.join("files").join(legacy_path_key(canonical));
        assert_ne!(legacy, root.join("files").join(path_key(canonical)));
        write_with_base(&legacy, "unsaved", Some(7)).unwrap();

        let path = file_backup_path(root, canonical);
        assert_eq!(read(&path).unwrap().0, "unsaved");
        assert_eq!(read_base(&path), Some(7));
        assert!(!legacy.exists());

        write(&legacy, "stale").unwrap();
        let path = file_backup_path(root, canonical);
        assert_eq!(read(&path).unwrap().0, "unsaved");
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = scratch_backup_path(dir.path(), "work", 1);
        write(&path, "one").unwrap();
        write(&path, "two").unwrap();
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("1")]);
    }

    #[test]
    fn file_and_scratch_paths_live_in_distinct_subdirs() {
        let root = Path::new("/state/backups");
        let f = file_backup_path(root, Path::new("/work/a.rs"));
        let sh = shell_backup_path(root, "work", 2);
        assert_eq!(sh, root.join("shell").join("work").join("2"));
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
