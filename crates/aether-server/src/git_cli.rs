//! Running the `git` CLI.
//!
//! Reads stay on libgit2 (see [`crate::git`]) — they're fast, in-process, and already paid for.
//! Every *mutation* comes through here instead, because libgit2 silently skips the things a user
//! expects git to do: it runs no hooks (`pre-commit`, `commit-msg`), does no commit signing, and
//! applies no smudge/clean filters, so an LFS checkout would write pointer files into the working
//! tree. Credential helpers, `core.autocrlf` and sparse-checkout come free with the real binary.
//! See `docs/git-phase-2.md` decision 1.
//!
//! ## Environment
//! Hooks are the user's own code and are meant to run in the user's own environment — but the
//! daemon is spawned detached and long-lived, so *its* environment is frozen at boot and may not
//! even have `git` on `PATH` (a Nix, Homebrew or version-manager install typically isn't there).
//! So every spawn borrows the same per-root environment resolution the language servers use
//! ([`crate::lsp::shell_env`]), which asks the user's login shell what it would set up in that
//! directory. Cached per root, so the shell launch is paid once.
//!
//! ## Errors
//! Deliberately *not* parsed. A non-zero exit is an ordinary [`GitOutput`] carrying the exit code
//! and git's own stderr, for the caller to surface verbatim the way a terminal would. Only a
//! failure to run git at all is an `Err`. Structured error variants would mean tracking git's
//! message wording across versions and locales, and would throw away the detail the user needs.

use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// What one `git` invocation produced. `code` is `None` when the child was killed by a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// `stdout` with trailing newlines removed — what almost every caller wants from a
    /// single-value query like `--version` or `rev-parse`.
    pub fn trimmed_stdout(&self) -> &str {
        self.stdout.trim_end_matches(['\n', '\r'])
    }
}

/// Run `git <args>` with `cwd` as the working directory, and wait for it to finish.
///
/// `Err` means git could not be run at all (not on `PATH`, not executable); an
/// [`std::io::ErrorKind::NotFound`] there is the "no git installed" case worth reporting to the
/// user. A git that *ran* and failed is `Ok` with a non-zero [`GitOutput::code`] — check
/// [`GitOutput::success`].
///
/// Output is captured rather than streamed. Every current caller is a short query; a long-running
/// operation (fetch, push, a large checkout) wants incremental progress and cancellation instead,
/// which is a second entry point to add alongside this one when there's an operation that needs it.
pub async fn run(cwd: &Path, args: &[&str]) -> std::io::Result<GitOutput> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Overlay, not replace: only the keys the shell resolved are overridden, so daemon-only vars
    // survive. `None` (no `$SHELL`, capture failed) simply inherits the daemon's environment.
    if let Some(env) = crate::lsp::shell_env::resolve(cwd).await {
        cmd.envs(&env);
    }

    let out = cmd.output().await?;
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    })
}

/// The installed git's version string (`git version 2.43.0`), or `None` when git can't be run.
///
/// `cwd` decides which environment the probe resolves — pass the directory git would actually be
/// run in, so the answer reflects the `PATH` a real operation would see rather than the daemon's.
/// A `None` here is a genuine diagnostic: every write operation in `docs/git-phase-2.md` depends
/// on this binary existing.
pub async fn version(cwd: &Path) -> Option<String> {
    let out = run(cwd, &["--version"]).await.ok()?;
    out.success().then(|| out.trimmed_stdout().to_string())
}

/// [`version`], for callers that may not have a directory to probe in.
///
/// Falls back to the daemon's own working directory rather than declining to answer, so that
/// `None` always means "git could not be run" and never "we didn't look" — the app-info dialog
/// renders the absence as a warning, so an ambiguous `None` would show a false alarm. Both
/// `app/info` and `GET /status` go through here, which is what keeps the two serving one snapshot.
pub async fn version_in(cwd: Option<&Path>) -> Option<String> {
    match cwd {
        Some(dir) => version(dir).await,
        None => version(&std::env::current_dir().ok()?).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip rather than fail where git isn't installed. CI is expected to have it (the write
    /// paths are untestable without it), but a contributor's sandbox may not.
    macro_rules! require_git {
        ($dir:expr) => {
            if version($dir).await.is_none() {
                return;
            }
        };
    }

    #[tokio::test]
    async fn version_reports_the_installed_git() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());
        let v = version(dir.path()).await.unwrap();
        assert!(v.starts_with("git version"), "unexpected version: {v}");
        // Trimmed: callers put this straight into a dialog row.
        assert!(!v.ends_with('\n'));
    }

    #[tokio::test]
    async fn run_reports_the_working_directory_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());
        let repo = dir.path().join("inner");
        std::fs::create_dir(&repo).unwrap();
        git2::Repository::init(&repo).unwrap();

        // Resolved relative to `cwd`, so this proves the child really ran there.
        let out = run(&repo, &["rev-parse", "--show-toplevel"]).await.unwrap();
        assert!(out.success());
        let reported = std::fs::canonicalize(out.trimmed_stdout()).unwrap();
        assert_eq!(reported, repo.canonicalize().unwrap());
    }

    /// A command that fails is not an `Err` — it's an `Ok` carrying git's own words. This is the
    /// contract the whole error story rests on: the client shows this text unaltered.
    #[tokio::test]
    async fn a_failing_command_surfaces_git_s_own_stderr() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());

        let out = run(dir.path(), &["rev-parse", "--show-toplevel"])
            .await
            .expect("git ran");
        assert!(!out.success());
        assert_ne!(out.code, Some(0));
        assert!(
            out.stderr.contains("not a git repository"),
            "expected git's own message, got: {:?}",
            out.stderr
        );
        assert!(out.stdout.is_empty());
    }

    /// Exit codes travel intact, so a caller can distinguish git's conventional codes without
    /// reading the message.
    #[tokio::test]
    async fn exit_codes_travel_intact() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());
        let repo = dir.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        git2::Repository::init(&repo).unwrap();

        // `--quiet` makes diff exit 1 on differences, 0 on none — an ordinary non-error non-zero.
        let clean = run(&repo, &["diff", "--quiet"]).await.unwrap();
        assert_eq!(clean.code, Some(0));

        // `-c core.excludesFile=` so a developer's global ignore file can't decide whether this
        // add succeeds. That per-invocation trick covers config *values*; once a command runs
        // hooks (commit, checkout) the global config can also point `core.hooksPath` somewhere
        // real, and isolating that needs `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` in the child's
        // environment — the point at which `run` should grow an env-overlay parameter.
        std::fs::write(repo.join("a.txt"), "x\n").unwrap();
        let out = run(&repo, &["-c", "core.excludesFile=", "add", "a.txt"])
            .await
            .unwrap();
        assert!(out.success(), "add failed: {}", out.stderr);
        let dirty = run(&repo, &["diff", "--quiet", "--cached"]).await.unwrap();
        assert_eq!(dirty.code, Some(1));
    }
}
