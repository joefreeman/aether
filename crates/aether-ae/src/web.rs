//! `ae --web` — open the editor in the browser (the deferred half of docs/tether.md §6).
//!
//! The CLI is a *launcher*: it builds the web client's URL — the same `?workspace=&root=&file=`
//! (+ `#L:C`) scheme the web shell's own boot and share links use — hands it to the OS opener,
//! and exits. The URL is always printed too, for launches where no opener can reach a browser
//! (SSH sessions).
//!
//! The exception is the quick-edit invocation (file positional, no explicit `--workspace` — the
//! same shape that tethers the native shells): there the process stays alive as a headless
//! **waiter**. It opens the buffer over the same RPC the shells' boots use — which places the
//! buffer in this client's workspace context, so the server's `buffer/closed` broadcast reaches
//! it without any viewport (`clients_affected_by_close`) — and exits 0 when the push reports the
//! buffer gone. That gives `ae --web file` the `$EDITOR` contract: git waits on the CLI, the
//! commit message is edited in a browser tab, `Space Alt-x` there closes the buffer, the waiter
//! exits, git proceeds. Ctrl-C closes the buffer (best-effort) and exits non-zero, so an
//! abandoned edit aborts the caller instead of hanging it.
//!
//! Deliberately out of scope for now (each bails with a message rather than half-working):
//! external files — a path outside every configured workspace needs an ephemeral context, which
//! the web boot can't address by name; and the native shells' release gesture (`Space k`) has no
//! web analogue — the waiter only ends on close or Ctrl-C (server-side tether registration,
//! docs/tether.md §6, is the future fix).

use aether_connection::{ConnectError, Handle, Inbound};
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferClosed, BufferClosedParams, BufferOpen, BufferOpenParams,
};
use aether_protocol::envelope::NotificationMethod;
use aether_protocol::workspace::{WorkspaceActivate, WorkspaceActivateParams};
use anyhow::{bail, Context};

/// Run the `--web` launch. `workspace` is the resolved (explicit or inferred) workspace, `path`
/// the bare CLI positional (jump suffix already peeled), `tether` the quick-edit flag computed in
/// `run_edit` — the same inputs the native shells get.
pub fn run(
    workspace: Option<String>,
    path: Option<String>,
    jump: Option<(u32, u32)>,
    tether: bool,
    version: String,
    port: u16,
) -> anyhow::Result<()> {
    let resolved = match &path {
        Some(p) => Some(aether_tui::resolve_cli_path(p)?),
        None => None,
    };
    match resolved {
        // No path: land on the workspace (or the chooser when none was named) — pure launcher.
        None => launch_only(port, workspace.as_deref()),
        // A directory is a session, not an errand (it never tethers natively either): land in
        // the workspace it infers to. The explorer-at-dir nicety needs a URL param the web boot
        // doesn't have yet, so this opens the workspace's last buffer instead.
        Some(dir) if dir.is_dir() => match workspace {
            Some(ws) => launch_only(port, Some(&ws)),
            None => bail!(
                "{} is outside every configured workspace — external paths aren't supported \
                 with --web yet",
                dir.display()
            ),
        },
        Some(file) => {
            let Some(ws) = workspace else {
                bail!(
                    "{} is outside every configured workspace — external files aren't supported \
                     with --web yet",
                    file.display()
                );
            };
            crate::runtime()?.block_on(open_in_workspace(port, ws, file, jump, tether, version))
        }
    }
}

/// Open the browser on a workspace (or the chooser) with nothing to wait for. The server was
/// spawned by `ensure_server_running` but may still be binding; probe until its HTTP side is up
/// so the tab doesn't land on a connection error.
fn launch_only(port: u16, workspace: Option<&str>) -> anyhow::Result<()> {
    wait_for_server(port)?;
    let url = web_url(port, workspace, None, None);
    open_in_browser(&url);
    Ok(())
}

/// The file case: dial the server (retrying while it boots), resolve the file against the
/// workspace's canonical roots, and either just launch the tab (`--workspace` named — a
/// deliberate session) or open the buffer here first and wait out its close (the tether).
async fn open_in_workspace(
    port: u16,
    workspace: String,
    file: std::path::PathBuf,
    jump: Option<(u32, u32)>,
    tether: bool,
    version: String,
) -> anyhow::Result<()> {
    let (handle, mut inbound) = connect_with_retry(port, &version).await?;
    let activated = handle
        .rpc::<WorkspaceActivate>(WorkspaceActivateParams {
            name: workspace.clone(),
            open_last: false,
        })
        .await
        .map_err(|e| anyhow::anyhow!("could not activate workspace '{workspace}': {e}"))?;

    let abs = file.display().to_string();
    // The server-canonicalized roots from the activate we just did — the same paths the shells'
    // boots match against, so root indices agree with what the web client will see.
    let Some((path_index, relative_path)) =
        aether_client::session::strip_longest_root(&abs, &activated.workspace.paths)
    else {
        bail!(
            "{abs} is outside the roots of workspace '{workspace}' — external files aren't \
             supported with --web yet"
        );
    };
    let url = web_url(port, Some(&workspace), Some((path_index, &relative_path)), jump);

    if !tether {
        // `--workspace` named: a session, not an errand. The web boot opens the file itself.
        open_in_browser(&url);
        return Ok(());
    }

    // The tether: open the buffer from *this* client before the browser exists. That puts it in
    // this client's workspace context (workspace MRU), which is what routes the `buffer/closed`
    // broadcast here despite the waiter never subscribing a viewport — and it means the web
    // boot's own open (by root + relative path) attaches to the same buffer, even for a
    // not-yet-existing file (`ae --web path/to/new-file`, create-on-first-save).
    let opened = handle
        .rpc::<BufferOpen>(BufferOpenParams {
            path_index: Some(path_index),
            relative_path: Some(relative_path.clone()),
            create_if_missing: true,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("could not open {relative_path}: {e}"))?;
    open_in_browser(&url);
    println!("Waiting for {relative_path} to be closed in the browser (Ctrl-C to abort)…");
    wait_for_close(&handle, &mut inbound, opened.buffer_id).await
}

/// Block until the tethered buffer is closed (by the browser, another client, or a path
/// deletion). Ctrl-C abandons the edit: close the buffer so the errand doesn't linger in the
/// workspace session, then exit non-zero so an `$EDITOR` caller aborts.
async fn wait_for_close(
    handle: &Handle,
    inbound: &mut tokio::sync::mpsc::UnboundedReceiver<Inbound>,
    buffer_id: aether_protocol::BufferId,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            msg = inbound.recv() => match msg {
                None => bail!("the connection to the server was lost"),
                Some(Inbound::Notification(n)) if n.method == BufferClosed::NAME => {
                    let Ok(params) = serde_json::from_value::<BufferClosedParams>(n.params) else {
                        continue;
                    };
                    if params.buffer_id == buffer_id {
                        return Ok(());
                    }
                }
                Some(_) => {}
            },
            _ = tokio::signal::ctrl_c() => {
                let close = handle.rpc::<BufferClose>(BufferCloseParams {
                    buffer_id,
                    open_next: false,
                });
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), close).await;
                bail!("aborted — the buffer was closed without saving");
            }
        }
    }
}

/// Dial the freshly-ensured server, retrying while it binds. A version mismatch is terminal
/// (retrying can't fix a stale daemon — same rule as the native shells); anything else retries
/// on a short interval until the deadline.
async fn connect_with_retry(
    port: u16,
    version: &str,
) -> anyhow::Result<(Handle, tokio::sync::mpsc::UnboundedReceiver<Inbound>)> {
    let url = format!("ws://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match aether_connection::connect(&url, version).await {
            Ok(pair) => return Ok(pair),
            Err(ConnectError::VersionMismatch(m)) => bail!("{m}"),
            Err(ConnectError::Down(e)) if std::time::Instant::now() >= deadline => {
                return Err(e).context("could not connect to the aether server");
            }
            Err(ConnectError::Down(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
        }
    }
}

/// Probe until something is listening on the (just-ensured) server port, so the browser tab
/// doesn't beat the daemon to it. Mirrors the deadline of [`connect_with_retry`].
fn wait_for_server(port: u16) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !crate::server_is_up(port) {
        if std::time::Instant::now() >= deadline {
            bail!("the aether server did not come up on port {port}");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Ok(())
}

/// Build the web client's URL: the server's loopback base plus the shared
/// [`aether_client::web_link`] query — the same builder the in-editor `Space Alt-z` copy uses,
/// so the CLI and the clients can't drift from what the web boot parses. The jump (0-based,
/// protocol convention) becomes the 1-based `#L:C` fragment, and only means anything on a file.
fn web_url(
    port: u16,
    workspace: Option<&str>,
    file: Option<(u32, &str)>,
    jump: Option<(u32, u32)>,
) -> String {
    use aether_client::web_link::{web_link, WebLinkTarget};
    let target = match file {
        Some((root, path)) => WebLinkTarget::File {
            root,
            path,
            at: jump,
        },
        None => WebLinkTarget::Workspace,
    };
    format!("http://127.0.0.1:{port}/{}", web_link(workspace, target))
}

/// Hand a URL to the OS opener, and always print it — the fallback for launches with no opener
/// in reach (SSH). Best-effort: a spawn failure downgrades to the printed URL, it never fails
/// the launch. The child is reaped on a throwaway thread so a long-lived waiter doesn't hold a
/// zombie.
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    println!("{url}");
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    match Command::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("could not launch a browser ({e}); open the URL above yourself"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_url_matches_the_web_shells_boot_scheme() {
        // Bare launch → the chooser: no params at all.
        assert_eq!(web_url(2385, None, None, None), "http://127.0.0.1:2385/");
        // Workspace only.
        assert_eq!(
            web_url(2385, Some("aether"), None, None),
            "http://127.0.0.1:2385/?workspace=aether"
        );
        // File in root 0: `root` omitted, matching the web shell's `fileQuery`.
        assert_eq!(
            web_url(2385, Some("aether"), Some((0, "src/main.rs")), None),
            "http://127.0.0.1:2385/?workspace=aether&file=src/main.rs"
        );
        // Non-zero root is carried; jump becomes the 1-based `#L:C` fragment (0-based in).
        assert_eq!(
            web_url(2400, Some("aether"), Some((2, "notes.md")), Some((41, 9))),
            "http://127.0.0.1:2400/?workspace=aether&root=2&file=notes.md#42:10"
        );
        // A jump without a file has nothing to attach to — no stray fragment.
        assert_eq!(
            web_url(2385, Some("aether"), None, Some((41, 9))),
            "http://127.0.0.1:2385/?workspace=aether"
        );
    }

    #[test]
    fn web_url_escapes_query_values() {
        // `#` would end the query early, `&` would split the pair, `+` would decode as a space,
        // spaces are unsafe raw. `URLSearchParams` decodes `%XX` back to the original.
        assert_eq!(
            web_url(2385, Some("my workspace"), Some((0, "a&b/#1 + c.txt")), None),
            "http://127.0.0.1:2385/?workspace=my%20workspace&file=a%26b/%231%20%2B%20c.txt"
        );
        // Percent itself must escape, or a literal `%20` in a filename would decode as a space.
        assert_eq!(
            web_url(2385, None, Some((0, "100%.txt")), None),
            "http://127.0.0.1:2385/?file=100%25.txt"
        );
    }
}
