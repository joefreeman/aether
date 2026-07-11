//! Resolve the environment a language server should run in, as if the user had opened a
//! login + interactive shell in the workspace root.
//!
//! Setting the child's cwd (see [`super::process::spawn`]) is enough for version managers that
//! expose per-project toolchains through *shims on `PATH`*: the shim reads the tool-version file
//! from its cwd upward. It is **not** enough for the equally common *shell-activation* model
//! (`mise activate`, `direnv`, `asdf`, `nvm`, …), where a shell-rc hook rewrites `PATH` (and other
//! vars) as a function of the current directory — there are no shims, so cwd alone changes nothing.
//!
//! Our daemon is long-lived and detached (spawned once via `setsid`, then reused across every
//! workspace), so its own environment is both frozen at boot *and* — since one daemon serves many
//! roots — impossible to make correct for all of them at once. So we resolve the environment
//! **per root, at spawn time**, by asking the user's own shell what it would set up in that
//! directory. This stays agnostic to which manager (if any) is in play: run `$SHELL -l -i -c`
//! with the cwd set to the root, dump the environment it produced, and read it back.
//!
//! Why both `-l` and `-i`: the login (`-l`) stage sources the profile that typically puts the
//! manager itself on `PATH` (`~/.local/bin`, …) — needed because the *capture* shell inherits the
//! daemon's possibly-minimal environment; the interactive (`-i`) stage is where activation hooks
//! actually register, so it's what makes them fire for the target directory.
//!
//! Everything here is best-effort: no `$SHELL`, a shell that hangs or errors, output we can't
//! parse — any of these yields `None`, and the caller falls back to inheriting the daemon's
//! environment (the pre-`690eea7` behavior). A server is never left worse off than before.
//!
//! Results are cached per root: launching a login+interactive shell is comparatively expensive and
//! the answer is stable for the daemon's lifetime.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::process::Command;

/// Sentinels bracketing the environment dump, so shell-init chatter printed to stdout (before our
/// command runs) can't be mistaken for environment data. Distinctive enough not to collide with a
/// real value.
const BEGIN: &str = "__AE_ENV_BEGIN__";
const END: &str = "__AE_ENV_END__";

/// How long to wait for the shell to produce its environment before giving up and falling back. A
/// well-behaved login shell returns in well under a second; this is only a backstop against an rc
/// file that blocks (e.g. waiting on input we've redirected from `/dev/null`).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-root cache of resolved environments. A cached `None` records "we tried and it didn't work"
/// so we don't re-pay the shell launch — or the timeout on a broken one — for every server that
/// starts under this root. Keyed by root only: `$SHELL` is constant for the daemon's lifetime.
#[allow(clippy::type_complexity)]
static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<HashMap<String, String>>>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<PathBuf, Option<HashMap<String, String>>>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The environment to run a language server under for `root`, or `None` to inherit the daemon's
/// environment. Cached after the first call per root.
pub async fn resolve(root: &Path) -> Option<HashMap<String, String>> {
    // Fast path: a prior resolution (success or failure) for this root.
    {
        let cache = cache().lock().unwrap();
        if let Some(hit) = cache.get(root) {
            return hit.clone();
        }
    }

    let resolved = match std::env::var_os("SHELL") {
        Some(shell) => capture(&shell, root).await,
        None => {
            tracing::debug!("no $SHELL set; language servers inherit the daemon environment");
            None
        }
    };

    // Store the outcome. A concurrent miss for the same root may have raced us here; keep whichever
    // landed first (the captures are equivalent) and return our own result either way.
    cache()
        .lock()
        .unwrap()
        .entry(root.to_path_buf())
        .or_insert_with(|| resolved.clone());
    resolved
}

/// Run `<shell> -l -i -c` in `root`, dumping its environment between sentinels, and parse it back.
async fn capture(shell: &OsStr, root: &Path) -> Option<HashMap<String, String>> {
    // `env -0` gives NUL-delimited `KEY=VALUE` records (robust to a newline inside a value). The
    // sentinels let us discard whatever the shell's rc files printed to stdout during init.
    let script = format!("printf %s {BEGIN}; env -0; printf %s {END}");
    let run = Command::new(shell)
        .arg("-l")
        .arg("-i")
        .arg("-c")
        .arg(&script)
        .current_dir(root)
        // No stdin: an rc that reads input gets EOF instead of hanging. Discard stderr — interactive
        // shells emit job-control chatter there that isn't ours to surface.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match tokio::time::timeout(CAPTURE_TIMEOUT, run).await {
        Ok(Ok(out)) => parse_env_dump(&out.stdout),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "shell env capture failed to run; inheriting daemon environment");
            None
        }
        Err(_) => {
            tracing::warn!(
                root = %root.display(),
                "shell env capture timed out; inheriting daemon environment"
            );
            None
        }
    }
}

/// Extract the environment from the shell's stdout: the bytes between [`BEGIN`] and [`END`], split
/// on NUL into `KEY=VALUE` records. Returns `None` if the sentinels are missing or the result has
/// no `PATH` — a dump without `PATH` means the shell produced nothing usable, so we'd rather fall
/// back than hand a server a crippled environment.
fn parse_env_dump(stdout: &[u8]) -> Option<HashMap<String, String>> {
    let begin = find(stdout, BEGIN.as_bytes())?;
    let after = begin + BEGIN.len();
    let end_rel = find(&stdout[after..], END.as_bytes())?;
    let body = &stdout[after..after + end_rel];

    let mut env = HashMap::new();
    for record in body.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        // Values may in principle be arbitrary bytes, but the vars a toolchain cares about are UTF-8
        // in practice — skip anything that isn't rather than guess an encoding.
        let Ok(record) = std::str::from_utf8(record) else {
            continue;
        };
        if let Some((key, value)) = record.split_once('=') {
            if !key.is_empty() {
                env.insert(key.to_string(), value.to_string());
            }
        }
    }
    env.contains_key("PATH").then_some(env)
}

/// First index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_records_between_sentinels_and_ignores_noise() {
        let dump = format!("rc chatter to stdout\n{BEGIN}PATH=/usr/bin\0HOME=/home/joe\0{END}");
        let env = parse_env_dump(dump.as_bytes()).expect("well-formed dump");
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(env.get("HOME").map(String::as_str), Some("/home/joe"));
    }

    #[test]
    fn parse_keeps_values_containing_equals_and_spaces() {
        let dump = format!("{BEGIN}PATH=/bin\0FOO=a=b c\0{END}");
        let env = parse_env_dump(dump.as_bytes()).expect("well-formed dump");
        // Only the first `=` splits key from value.
        assert_eq!(env.get("FOO").map(String::as_str), Some("a=b c"));
    }

    #[test]
    fn parse_rejects_a_dump_without_path() {
        let dump = format!("{BEGIN}HOME=/home/joe\0{END}");
        assert!(parse_env_dump(dump.as_bytes()).is_none());
    }

    #[test]
    fn parse_rejects_missing_sentinels() {
        assert!(parse_env_dump(b"PATH=/usr/bin\0").is_none());
        assert!(parse_env_dump(format!("{BEGIN}PATH=/usr/bin\0").as_bytes()).is_none());
    }

    /// End-to-end capture against a fake "shell": a script that ignores `-l -i -c` and emits a
    /// sentinel-wrapped dump. It reports its own `$PWD`, proving we launch it in the root we pass
    /// (which is what makes directory-specific activation resolve the right toolchain), and includes
    /// leading rc-style noise plus a multi-word value to exercise the parser end to end.
    #[cfg(unix)]
    #[tokio::test]
    async fn capture_runs_the_shell_in_root_and_reads_its_env() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let shell = dir.path().join("fake-shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\n\
             echo 'pretend rc chatter'\n\
             printf %s __AE_ENV_BEGIN__\n\
             printf 'PATH=%s\\0' \"$PATH\"\n\
             printf 'PWD_SEEN=%s\\0' \"$(pwd)\"\n\
             printf 'GREETING=hello world\\0'\n\
             printf %s __AE_ENV_END__\n",
        )
        .expect("write fake shell");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake shell");

        let env = match capture(shell.as_os_str(), dir.path()).await {
            Some(env) => env,
            None => return, // no `/bin/sh` on this host; nothing to test
        };

        let pwd_seen = std::fs::canonicalize(env.get("PWD_SEEN").expect("PWD_SEEN captured"))
            .expect("canonicalize reported pwd");
        let expected = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        assert_eq!(pwd_seen, expected, "capture must run in the root we pass");
        assert_eq!(env.get("GREETING").map(String::as_str), Some("hello world"));
        assert!(env.contains_key("PATH"));
    }

    /// A shell that fails (or prints nothing) resolves to `None`, so the caller inherits the
    /// daemon environment rather than a broken one.
    #[cfg(unix)]
    #[tokio::test]
    async fn capture_falls_back_when_the_shell_produces_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let shell = dir.path().join("broken-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 1\n").expect("write broken shell");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))
            .expect("chmod broken shell");

        assert!(capture(shell.as_os_str(), dir.path()).await.is_none());
    }
}
