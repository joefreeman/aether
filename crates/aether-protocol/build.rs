//! Stamps the build's git identity into the binary as `AETHER_COMMIT` / `AETHER_COMMIT_DIRTY`,
//! read back by [`aether_protocol::BUILD_COMMIT`] / [`aether_protocol::BUILD_DIRTY`].
//!
//! Why: the release version (`0.2.0`) doesn't identify a *build*. A hand-built dev binary, a
//! release AppImage, and a colleague's checkout can all report the same version while behaving
//! differently, and the app-info dialog exists precisely to answer "which build am I running?".
//!
//! Both values are best-effort: a source tarball, a shallow export, or a machine without `git`
//! yields an empty commit and a `false` dirty flag, which the app renders as "unknown build".
//! Never fails the build — build identity is a diagnostic, not a requirement.
//!
//! The dirty flag is a snapshot of the *build moment*: committing afterwards doesn't un-dirty an
//! already-built binary. That's the intended reading ("this binary was built from a modified
//! tree"), and it's why the flag is worth as much as the SHA.

use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run only when HEAD moves. Emitting any `rerun-if-changed` replaces cargo's default
    // "re-run when any package file changed" heuristic — fine here, since nothing in this script
    // reads the crate's sources. Without this, every touched source file would re-shell out to git.
    if let Some(git_dir) = git_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        // HEAD is usually a symref (`ref: refs/heads/main`); the ref file is what actually moves on
        // commit. Watch both — resolving the symref here would go stale on a branch switch anyway,
        // and `packed-refs` covers the branch whose loose ref has been packed away.
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
        let refs = git_dir.join("refs").join("heads");
        if refs.is_dir() {
            println!("cargo:rerun-if-changed={}", refs.display());
        }
    } else {
        // No checkout to watch: pin the re-run to this script so cargo doesn't re-run it constantly.
        println!("cargo:rerun-if-changed=build.rs");
    }

    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    // `--porcelain` prints one line per modified/untracked path and nothing at all for a clean
    // tree, so "non-empty output" *is* the dirty test. Only meaningful when we resolved a commit:
    // outside a checkout the empty output would otherwise read as "clean".
    let dirty = !commit.is_empty()
        && git(&["status", "--porcelain"])
            .map(|s| !s.is_empty())
            .unwrap_or(false);

    println!("cargo:rustc-env=AETHER_COMMIT={commit}");
    println!(
        "cargo:rustc-env=AETHER_COMMIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
}

/// Run a git command in the crate's directory (git walks up to the repo root itself) and return its
/// trimmed stdout. `None` for a missing binary, a non-zero exit, or non-UTF-8 output — every
/// failure mode collapses to "no build identity available".
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// The repository's `.git` directory, as an absolute path. `None` when not building from a
/// checkout. Note this resolves to the *common* dir, so a worktree build watches the real HEAD.
fn git_dir() -> Option<std::path::PathBuf> {
    let dir = git(&["rev-parse", "--absolute-git-dir"])?;
    let path = Path::new(&dir).to_path_buf();
    path.is_dir().then_some(path)
}
