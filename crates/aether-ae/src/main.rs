//! `ae` — the single Aether binary. One executable runs the server daemon, the terminal client,
//! and (when built with the `gui` feature) the native GUI client; the shells themselves live in
//! their own crates (`aether-server`, `aether-tui`, `aether-iced`) and this crate is just the
//! entry point that parses the CLI and dispatches into one of them.
//!
//! - `ae server [OPTIONS]`               — run the editor server daemon
//! - `ae [edit] [PATH]`                  — open a client, auto-detecting terminal vs GUI
//! - `ae --gui [PATH]` / `ae --tui ...`  — force a specific client
//! - `ae -p NAME [PATH]`                 — open in a named project, overriding inference
//!
//! `edit` is the default command: a bare `ae` (or `ae file.rs`) runs it, so the common case needs
//! no subcommand. With a PATH, the project is inferred by matching it against configured project
//! roots; a bare `ae` (no PATH) opens the project picker rather than guessing from the working
//! directory. `-p/--project` overrides inference, and `ae edit ...` is the explicit form for when a
//! PATH would otherwise collide with the `server` subcommand name.
//!
//! Server lifetime: `edit` auto-starts a server if none is listening (see [`ensure_server_running`])
//! — a detached, idle-reapable daemon that outlives the client and shuts itself down once no client
//! has been connected for a while (and no buffer is unsaved). `ae server` runs the same daemon but
//! *persistently*: it never idle-reaps, so it's the way to keep one pinned (and to see its logs).
//!
//! iced owns the main thread and its own tokio runtime, so the GUI client is dispatched as a plain
//! synchronous call; the server and terminal client are `async` and get a runtime built here.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "ae", version, about = "Aether editor")]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Arguments for the default `edit` command, used when no subcommand is given.
    #[command(flatten)]
    edit: EditArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the editor server daemon.
    Server(ServerArgs),
    /// Open files in a client (the default when no subcommand is given).
    Edit(EditArgs),
}

#[derive(Args, Debug)]
struct ServerArgs {
    /// Override the tracing filter (e.g. `aether_server=debug`). Falls back to `RUST_LOG`, then a
    /// sensible default.
    #[arg(long, value_name = "FILTER")]
    log: Option<String>,

    /// Shut the server down after this many seconds with no connected clients and no unsaved
    /// buffers. Set by client auto-start so a conjured daemon cleans up after itself; omitted by a
    /// user-run `ae server`, which stays up until signalled. Hidden — it's an internal handshake,
    /// not a user-facing knob.
    #[arg(long, value_name = "SECS", hide = true)]
    idle_timeout: Option<u64>,
}

#[derive(Args, Debug, Default)]
struct EditArgs {
    /// Force the native GUI client, overriding terminal/GUI auto-detection.
    #[arg(long, conflicts_with = "tui")]
    gui: bool,

    /// Force the terminal client, overriding terminal/GUI auto-detection.
    #[arg(long)]
    tui: bool,

    /// Project name. Overrides inference — the named project must have a config at
    /// `$XDG_CONFIG_HOME/aether/projects/<name>.toml`. Omit to infer the project from PATH; with no
    /// PATH (or a PATH outside every configured project), the project picker opens.
    #[arg(short = 'p', long)]
    project: Option<String>,

    /// File or directory to open. Resolved against the current working directory and used to infer
    /// the project when `--project` is absent. A directory opens the file browser at that location
    /// with a scratch buffer underneath. Omit to start in a scratch buffer with no file browser.
    path: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let version = env!("CARGO_PKG_VERSION").to_string();

    match cli.command {
        Some(Command::Server(args)) => run_server(args),
        Some(Command::Edit(edit)) => run_edit(edit, version),
        None => run_edit(cli.edit, version),
    }
}

/// Run the default `edit` command: resolve the project (explicit, inferred, or picker), make sure a
/// server is up to talk to, then launch the terminal or GUI client.
fn run_edit(edit: EditArgs, version: String) -> anyhow::Result<()> {
    let project = resolve_project(&edit)?;
    ensure_server_running();
    if want_gui(&edit) {
        run_gui(project, edit.path, version)
    } else {
        run_tui(project, edit.path, version)
    }
}

/// Idle timeout handed to an auto-started server. Long enough to ride out closing one client and
/// opening another (e.g. swapping the TUI for the GUI) so the live server — and its in-memory undo
/// and unsaved edits — survives the hop; short enough that a forgotten server doesn't linger. A
/// user-run `ae server` ignores this and stays up permanently.
const AUTOSTART_IDLE_TIMEOUT_SECS: u64 = 300;

/// Make sure a server is listening before we launch a client. The client connects with retry, so we
/// only need to *start* one if none is up: probe the fixed port, and if nothing answers, spawn a
/// detached, idle-reapable `ae server`. The spawned server outlives this client (and any later one)
/// until it idle-reaps itself. A lost race — two clients each spawning, or a stale server still
/// holding the port — is harmless: the redundant server fails to bind the singleton port and exits.
/// Best-effort: on failure we warn and let the client's connect loop surface the unreachable server.
fn ensure_server_running() {
    if server_is_up() {
        return;
    }
    if let Err(e) = spawn_detached_server() {
        eprintln!("warning: could not auto-start the aether server ({e}); is one running?");
    }
}

/// Whether something is already listening on the server's loopback port. A short connect probe is
/// enough — a stale/foreign listener is caught later by the WebSocket version gate, not here.
fn server_is_up() -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, aether_server::SERVER_PORT));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// Spawn `ae server --idle-timeout N` from our own executable, detached so it outlives this client
/// and is insulated from the controlling terminal (no SIGINT on Ctrl-C, no SIGHUP on terminal
/// close). stdio goes to the void — a user who wants server logs runs `ae server` by hand.
fn spawn_detached_server() -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .arg("--idle-timeout")
        .arg(AUTOSTART_IDLE_TIMEOUT_SECS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

/// Put the spawned server in its own session so the controlling terminal's signals never reach it.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and is the only thing we run in the forked child
    // before `exec`; we touch no shared state, allocate nothing, and return on its result.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Non-Unix fallback: the null stdio in `spawn_detached_server` already decouples the child; we
/// just don't get session detachment.
#[cfg(not(unix))]
fn detach(_cmd: &mut std::process::Command) {}

/// Decide which project to open. `--project` always wins. Otherwise infer from the PATH: a path
/// inside exactly one project opens there; a path inside *several* is an error the user must
/// disambiguate. A path inside *no* configured project is no longer an error — we return `None`,
/// and the client opens the file directly in an ephemeral "(no project)" context (`ae /etc/hosts`).
/// With no PATH at all, we return `None` so a bare `ae` opens the project picker — the working
/// directory is deliberately *not* used to guess a project (it only resolves relative file paths).
fn resolve_project(edit: &EditArgs) -> anyhow::Result<Option<String>> {
    use aether_server::ProjectMatch;

    if edit.project.is_some() {
        return Ok(edit.project.clone());
    }

    if let Some(path) = &edit.path {
        return match aether_server::infer_project_for_path(std::path::Path::new(path))? {
            ProjectMatch::One(name) => Ok(Some(name)),
            // Outside every project: open project-lessly (the client falls back to open-from-path).
            ProjectMatch::None => Ok(None),
            ProjectMatch::Ambiguous(names) => anyhow::bail!(
                "{path} is within multiple projects ({}) — disambiguate with `--project NAME`",
                names.join(", ")
            ),
        };
    }

    // No `--project` and no path: open the project picker rather than guessing from the working
    // directory. The working directory is only meaningful for resolving a relative *file* path
    // (the `path` branch above, and the open-from-path overlay), not as a standalone project
    // signal — a bare `ae` should land on the (recency-sorted) chooser regardless of where it's
    // launched from.
    Ok(None)
}

/// Decide whether to launch the GUI when the user didn't force a client. `--gui`/`--tui` win
/// outright. Otherwise infer from the environment: a terminal on stdout means we were launched
/// from a shell (terminal client), while no terminal plus a desktop GUI session means a desktop
/// launcher (GUI). With neither — no terminal and no GUI session — fall back to the terminal client.
fn want_gui(edit: &EditArgs) -> bool {
    if edit.gui {
        return true;
    }
    if edit.tui {
        return false;
    }
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        return false;
    }
    has_gui_display()
}

/// Whether a desktop GUI session is available for a no-terminal launch. macOS and Windows always
/// have a windowing server present (Quartz / the Desktop Window Manager), so a launch with no
/// controlling terminal is a desktop-launcher start → GUI. On Linux/BSD that only holds when an
/// X11 or Wayland display is configured — `DISPLAY`/`WAYLAND_DISPLAY` are X11/Wayland-specific and
/// are never set on macOS, so they must not gate the decision there.
fn has_gui_display() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
}

fn run_server(args: ServerArgs) -> anyhow::Result<()> {
    let filter = match args.log {
        Some(filter) => tracing_subscriber::EnvFilter::new(filter),
        None => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_server=info,info")),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let idle_timeout = args.idle_timeout.map(std::time::Duration::from_secs);
    runtime()?.block_on(aether_server::run(idle_timeout))
}

fn run_tui(project: Option<String>, path: Option<String>, version: String) -> anyhow::Result<()> {
    runtime()?.block_on(aether_tui::run(project, path, version))
}

#[cfg(feature = "gui")]
fn run_gui(project: Option<String>, path: Option<String>, version: String) -> anyhow::Result<()> {
    // iced owns the main thread and manages its own tokio runtime, so this is a synchronous call —
    // do not wrap it in `runtime().block_on`, which would panic on a nested runtime.
    aether_iced::run(project, path, version)
}

#[cfg(not(feature = "gui"))]
fn run_gui(
    _project: Option<String>,
    _path: Option<String>,
    _version: String,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "this build of `ae` was compiled without GUI support — rebuild with `--features gui`, \
         or run the terminal client with `--tui`"
    )
}

/// A multi-threaded tokio runtime for the server / terminal client. The GUI client never uses
/// this — it builds its own runtime inside `aether_iced::run`.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_opens_the_picker_not_a_cwd_inferred_project() {
        // No `--project` and no path → `None`, so the client opens the project chooser. The key
        // guarantee: this branch never consults the working directory to guess a project.
        assert_eq!(resolve_project(&EditArgs::default()).unwrap(), None);
    }

    #[test]
    fn explicit_project_flag_wins() {
        let edit = EditArgs {
            project: Some("myproj".into()),
            ..Default::default()
        };
        assert_eq!(resolve_project(&edit).unwrap(), Some("myproj".to_string()));
    }
}
