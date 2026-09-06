//! Running the `git` CLI.
//!
//! Reads stay on libgit2 (see [`crate::git`]) — they're fast, in-process, and already paid for.
//! Every *mutation* comes through here instead, because libgit2 silently skips the things a user
//! expects git to do: it runs no hooks (`pre-commit`, `commit-msg`), does no commit signing, and
//! applies no smudge/clean filters, so an LFS checkout would write pointer files into the working
//! tree. Credential helpers, `core.autocrlf` and sparse-checkout come free with the real binary.
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

use crate::process::{self, Stream};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Watches for a cancellation of the operation [`run_streaming`] is running.
///
/// Re-exported from [`crate::process`], which owns the running of children: it is the same type a
/// shell view's run is cancelled through, so `git/cancel` and `shell/cancel` cannot drift into two
/// ideas of what a cancel is.
pub type CancelToken = process::CancelToken;
/// The other end of a [`CancelToken`] — held by whoever may cancel the operation.
pub type CancelHandle = process::CancelHandle;

pub fn cancel_channel() -> (CancelHandle, CancelToken) {
    process::cancel_channel()
}

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
/// Output is captured rather than streamed, which is what every short query wants. The network
/// operations want [`run_streaming`] instead — incremental progress and a killable child.
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
    // The daemon has no terminal, so a git that decides to open an editor would sit there forever
    // (and `$EDITOR` from the user's shell may well be an interactive one). Every command here
    // either needs no message or passes it with `-F`, so "the editor did nothing and succeeded" is
    // exactly the right answer — `git rebase --continue` then reuses the stored message, and
    // `git pull`'s merge commit takes its default. Set after the shell environment, deliberately,
    // so it wins over an inherited `GIT_EDITOR`.
    cmd.env("GIT_EDITOR", "true");

    let out = cmd.output().await?;
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    })
}

/// [`run`], but reporting git's progress as it arrives and killable while it runs.
///
/// The second entry point `run`'s own doc-comment anticipated. Everything short — `rev-parse`,
/// `branch -d`, `--version` — wants the simple shape; the network operations want this one, because
/// a push to an unreachable host sits in a TCP timeout for minutes with nothing on screen, and
/// "did that work?" is not a question an editor should leave open.
///
/// `on_progress` receives git's stderr as it is written. **Split on `\r` as well as `\n`**: git's
/// counters (`Writing objects:  47% (8/17)`) rewrite one line in place with carriage returns, so a
/// newline-only reader would sit silent and then emit the whole transfer at once — precisely the
/// behaviour this exists to avoid.
///
/// `cancel` kills the child. The returned [`GitOutput`] then carries whatever git managed to say
/// plus `code: None` (the signal case), which is why callers should treat cancellation as their own
/// outcome rather than reading it out of the exit status.
pub async fn run_streaming(
    cwd: &Path,
    args: &[&str],
    cancel: CancelToken,
    mut on_progress: impl FnMut(String) + Send,
) -> std::io::Result<GitOutput> {
    let mut cmd = process::command("git");
    cmd.args(args).current_dir(cwd);
    if let Some(env) = crate::lsp::shell_env::resolve(cwd).await {
        cmd.envs(&env);
    }
    // The daemon has no terminal, so a git that decides to open an editor would sit there forever
    // (and `$EDITOR` from the user's shell may well be an interactive one). Every command here
    // either needs no message or passes it with `-F`, so "the editor did nothing and succeeded" is
    // exactly the right answer — a continued rebase reuses the stored message, and a merge takes
    // its default. Set after the shell environment, deliberately, so it wins over an inherited
    // `GIT_EDITOR`.
    cmd.env("GIT_EDITOR", "true");
    // Ask for progress explicitly: git suppresses it when stderr isn't a terminal, which ours
    // never is.
    cmd.env("GIT_PROGRESS_DELAY", "0");

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut pending = Vec::<u8>::new();
    let exit = process::run_streaming(cmd, cancel, |stream, chunk| {
        match stream {
            Stream::Stdout => stdout.push_str(&String::from_utf8_lossy(chunk)),
            Stream::Stderr => {
                stderr.push_str(&String::from_utf8_lossy(chunk));
                pending.extend_from_slice(chunk);
                for line in take_progress_lines(&mut pending) {
                    on_progress(line);
                }
            }
        }
        // git's output is never capped: the caller wants the whole of what it said.
        true
    })
    .await?;

    Ok(GitOutput {
        stdout,
        stderr,
        code: exit.code,
    })
}

/// Split off every complete progress line in `pending`, leaving any partial tail behind.
///
/// Both terminators count: `\n` ends a real line, `\r` ends one of git's in-place counter updates.
/// Empty segments are dropped, since `\r\n` would otherwise emit a blank between the two.
fn take_progress_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(idx) = pending.iter().position(|b| *b == b'\n' || *b == b'\r') {
        let line: Vec<u8> = pending.drain(..=idx).collect();
        let text = String::from_utf8_lossy(&line[..line.len() - 1])
            .trim()
            .to_string();
        if !text.is_empty() {
            out.push(text);
        }
    }
    out
}

/// The installed git's version string (`git version 2.43.0`), or `None` when git can't be run.
///
/// `cwd` decides which environment the probe resolves — pass the directory git would actually be
/// run in, so the answer reflects the `PATH` a real operation would see rather than the daemon's. A
/// `None` here is a genuine diagnostic: every git write depends on this binary existing.
pub async fn version(cwd: &Path) -> Option<String> {
    let out = run(cwd, &["--version"]).await.ok()?;
    out.success().then(|| out.trimmed_stdout().to_string())
}

/// Whether the installed git is at least `major.minor` — the capability check behind the flags
/// that aren't old enough to assume (`stash push --staged`, git 2.35).
///
/// Unreadable or unparseable is `false`: a capability we can't confirm is one we don't use, which
/// turns into a refusal the user can act on rather than a raw usage dump from git.
pub async fn at_least(cwd: &Path, major: u32, minor: u32) -> bool {
    version(cwd)
        .await
        .as_deref()
        .and_then(parse_version)
        .is_some_and(|v| v >= (major, minor))
}

/// `(major, minor)` out of a `git --version` line. Every platform's string leads with the same two
/// numbers — `git version 2.43.0`, `git version 2.39.3 (Apple Git-145)`,
/// `git version 2.42.0.windows.2` — and no check here needs more than that.
fn parse_version(line: &str) -> Option<(u32, u32)> {
    let rest = line.trim().strip_prefix("git version ")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some((major, minor))
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

    /// The platform suffixes are the point: a vendor's trailing junk must not cost us a feature
    /// the git underneath it has.
    #[test]
    fn parse_version_reads_major_minor_through_platform_suffixes() {
        assert_eq!(parse_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(
            parse_version("git version 2.39.3 (Apple Git-145)"),
            Some((2, 39))
        );
        assert_eq!(parse_version("git version 2.42.0.windows.2"), Some((2, 42)));
        // Nothing to read is never "new enough" — the caller refuses rather than guesses.
        assert_eq!(parse_version("hg version 6.4"), None);
        assert_eq!(parse_version(""), None);
    }

    /// The 2.35 boundary `stash push --staged` sits on, checked from both sides so an off-by-one
    /// in the comparison can't pass.
    #[test]
    fn version_comparison_is_inclusive_of_the_named_release() {
        let v = |s| parse_version(s).unwrap();
        assert!(v("git version 2.35.0") >= (2, 35));
        assert!(v("git version 2.43.0") >= (2, 35));
        assert!(v("git version 3.0.0") >= (2, 35));
        assert!(v("git version 2.34.1") < (2, 35));
        assert!(v("git version 1.9.5") < (2, 35));
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

    /// git's counters rewrite one line in place with carriage returns. A newline-only reader would
    /// sit silent for the whole transfer and then emit it in one lump — the exact behaviour the
    /// progress stream exists to avoid — so both terminators have to end a line.
    #[test]
    fn progress_lines_split_on_carriage_returns_too() {
        let mut pending = b"Counting objects: 10%\rCounting objects: 40%\rdone\n".to_vec();
        assert_eq!(
            take_progress_lines(&mut pending),
            vec![
                "Counting objects: 10%".to_string(),
                "Counting objects: 40%".to_string(),
                "done".to_string(),
            ]
        );
        assert!(pending.is_empty());

        // A partial tail stays buffered until its terminator arrives, rather than being reported
        // as a truncated line.
        let mut pending = b"Writing objects:  4".to_vec();
        assert!(take_progress_lines(&mut pending).is_empty());
        pending.extend_from_slice(b"7%\r");
        assert_eq!(
            take_progress_lines(&mut pending),
            vec!["Writing objects:  47%".to_string()]
        );

        // `\r\n` is one break, not two — the empty segment between them is dropped.
        let mut pending = b"one\r\ntwo\r\n".to_vec();
        assert_eq!(
            take_progress_lines(&mut pending),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    /// The streaming runner reports git's output as it goes and still returns the exit status.
    #[tokio::test]
    async fn run_streaming_collects_output_and_status() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());
        let repo = dir.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        git2::Repository::init(&repo).unwrap();

        let (_handle, token) = cancel_channel();
        let out = run_streaming(&repo, &["rev-parse", "--show-toplevel"], token, |_| {})
            .await
            .expect("git ran");
        assert!(out.success());
        assert_eq!(
            std::fs::canonicalize(out.trimmed_stdout()).unwrap(),
            repo.canonicalize().unwrap()
        );
    }

    /// Cancelling kills the child instead of waiting it out — the whole reason `git/cancel` exists,
    /// since a push to an unreachable host otherwise sits in a TCP timeout for minutes.
    ///
    /// A `pre-commit` hook that sleeps is the portable way to make git genuinely block: no network,
    /// no timing luck, and the commit not existing afterwards proves the process really died rather
    /// than the runner just returning early.
    #[tokio::test]
    async fn a_cancelled_command_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        require_git!(dir.path());
        let repo_dir = dir.path().join("r");
        std::fs::create_dir(&repo_dir).unwrap();
        let repo = git2::Repository::init(&repo_dir).unwrap();

        let hooks = repo_dir.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nsleep 60\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
            cfg.set_bool("commit.gpgsign", false).unwrap();
            cfg.set_str("core.hooksPath", hooks.to_str().unwrap())
                .unwrap();
        }
        std::fs::write(repo_dir.join("a.txt"), "x\n").unwrap();
        run(&repo_dir, &["-c", "core.excludesFile=", "add", "a.txt"])
            .await
            .unwrap();

        let (handle, token) = cancel_channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let _ = handle.send(true);
        });

        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run_streaming(&repo_dir, &["commit", "-m", "blocked"], token, |_| {}),
        )
        .await
        .expect("run_streaming returned rather than waiting out the hook")
        .expect("git ran");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "returned before the 60s hook finished"
        );
        // Killed by a signal, so there is no exit code — which is exactly why callers read
        // cancellation off the token rather than out of the status.
        assert_eq!(out.code, None);
        assert!(
            repo.head().is_err(),
            "the commit must not have happened — the process was really killed"
        );
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
