//! Spawn a language server as a child process and connect to its stdio.
//!
//! Thin glue over [`super::client::connect`]: the protocol logic is stream-generic and tested over
//! in-memory pipes (see `client`/`transport` tests), so this only has to wire a child's pipes in
//! and drain its stderr to the log.

use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::client::{self, LspClient, LspInbound};

/// A running language-server subprocess: the client handle, its inbound channel, and the child so
/// the caller can terminate it (or rely on `kill_on_drop`).
pub struct LspProcess {
    pub client: LspClient,
    pub inbound: mpsc::UnboundedReceiver<LspInbound>,
    pub child: Child,
}

/// Spawn `command args...` in `cwd` and connect to it. The child is killed if its [`Child`] is
/// dropped.
///
/// `cwd` is the discovered LSP root (see [`super::manager`]). Beyond the servers' own use of it, it
/// is what makes version-manager shims resolve a per-project toolchain: shims (asdf, rbenv/pyenv,
/// rustup's cargo proxies, mise-with-shims, …) read the tool-version files from their process's cwd
/// *upward*, so launching here honors a pin at the root or any ancestor. `command` itself is still
/// resolved on `PATH` — we only steer the cwd, staying agnostic to which manager (if any) is in use.
pub fn spawn(command: &str, args: &[&str], cwd: &Path) -> std::io::Result<LspProcess> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    // Language servers log to stderr; surface it at debug rather than letting it pile up in the pipe
    // buffer (a full stderr pipe would eventually block the server).
    if let Some(stderr) = child.stderr.take() {
        let name = command.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server = %name, "lsp stderr: {line}");
            }
        });
    }

    let (client, inbound) = client::connect(stdout, stdin);
    Ok(LspProcess {
        client,
        inbound,
        child,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    /// Smoke-test the real OS-pipe plumbing (the bit the in-memory duplex tests can't cover):
    /// `cat` echoes stdin to stdout verbatim. It fully-buffers a pipe, flushing on exit, so we
    /// drop the client to close stdin and force the echoed frame back through the real pipes.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_round_trips_a_frame_through_cat() {
        let LspProcess {
            client,
            mut inbound,
            mut child,
        } = match spawn("cat", &[], Path::new(".")) {
            Ok(p) => p,
            Err(_) => return, // no `cat` on this host; nothing to test
        };
        client.notify("test/ping", json!({ "v": 1 })).unwrap();
        drop(client); // closes stdin → cat flushes the echo and exits

        let msg = tokio::time::timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("timed out waiting for echo")
            .expect("connection closed before echo");
        match msg {
            LspInbound::Notification { method, params } => {
                assert_eq!(method, "test/ping");
                assert_eq!(params["v"], 1);
            }
            other => panic!("unexpected inbound: {other:?}"),
        }
        let _ = child.wait().await;
    }

    /// The child must run in the `cwd` we pass — this is what lets version-manager shims resolve a
    /// per-project toolchain. The child writes its own working directory to a marker file opened
    /// *relative to that cwd*; if `current_dir` is honored the marker lands inside `dir` and holds
    /// `dir`'s path. Canonicalize both sides so a symlinked temp root (e.g. `/tmp`) doesn't fool us.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_runs_child_in_the_given_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let LspProcess { mut child, .. } =
            match spawn("sh", &["-c", "pwd -P > cwd_marker"], dir.path()) {
                Ok(p) => p,
                Err(_) => return, // no `sh` on this host; nothing to test
            };
        let _ = child.wait().await;

        let marker = std::fs::read_to_string(dir.path().join("cwd_marker"))
            .expect("marker written in the child's cwd");
        let reported = std::fs::canonicalize(marker.trim()).expect("canonicalize reported cwd");
        let expected = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        assert_eq!(reported, expected);
    }
}
