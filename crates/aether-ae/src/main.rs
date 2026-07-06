//! `ae` — the single Aether binary. One executable runs the server daemon, the terminal client,
//! and (when built with the `gui` feature) the native GUI client; the shells themselves live in
//! their own crates (`aether-server`, `aether-tui`, `aether-iced`) and this crate is just the
//! entry point that parses the CLI and dispatches into one of them.
//!
//! - `ae server [OPTIONS]`               — run the editor server daemon
//! - `ae [edit] [PATH]`                  — open a client, auto-detecting terminal vs GUI
//! - `ae --gui [PATH]` / `ae --tui ...`  — force a specific client
//! - `ae -w NAME [PATH]`                 — open in a named workspace, overriding inference
//!
//! `edit` is the default command: a bare `ae` (or `ae file.rs`) runs it, so the common case needs
//! no subcommand. With a PATH, the workspace is inferred by matching it against configured workspace
//! roots; a bare `ae` (no PATH) opens the workspace picker rather than guessing from the working
//! directory. `-w/--workspace` overrides inference, and `ae edit ...` is the explicit form for when a
//! PATH would otherwise collide with the `server` subcommand name.
//!
//! Server lifetime: `edit` auto-starts a server if none is listening (see [`ensure_server_running`])
//! — a detached, idle-reapable daemon that outlives the client and shuts itself down once no client
//! has been connected for a while (and no buffer is unsaved). `ae server` runs the same daemon but
//! *persistently*: it never idle-reaps, so it's the way to keep one pinned (and to see its logs).
//!
//! iced owns the main thread and its own tokio runtime, so the GUI client is dispatched as a plain
//! synchronous call; the server and terminal client are `async` and get a runtime built here.

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "ae", version, about = "Aether editor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Arguments for the default `edit` command, used when no subcommand is given.
    #[command(flatten)]
    edit: EditArgs,

    /// Profile (separate instance) to use, e.g. `dev` alongside your daily one [default: default].
    ///
    /// Like browser profiles: each has its own config, sessions, and server on its own port. Also
    /// read from `AETHER_PROFILE`. See `docs/profiles.md`.
    #[arg(short = 'p', long, global = true, env = "AETHER_PROFILE")]
    profile: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the editor server daemon.
    Server(ServerArgs),
    /// Open files in a client (the default when no subcommand is given).
    Edit(EditArgs),
    /// Manage profiles (separate instances).
    Profiles(ProfilesArgs),
}

#[derive(Args, Debug)]
struct ProfilesArgs {
    #[command(subcommand)]
    command: ProfilesCommand,
}

#[derive(Subcommand, Debug)]
enum ProfilesCommand {
    /// List profiles and their ports.
    List,
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

    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Subcommand, Debug)]
enum ServerCommand {
    /// Stop the running server for this profile.
    Stop,
}

#[derive(Args, Debug, Default)]
struct EditArgs {
    /// Force the native GUI client, overriding terminal/GUI auto-detection.
    #[arg(long, conflicts_with = "tui")]
    gui: bool,

    /// Force the terminal client, overriding terminal/GUI auto-detection.
    #[arg(long)]
    tui: bool,

    /// Workspace to open, overriding inference from PATH.
    ///
    /// Config lives at `$XDG_CONFIG_HOME/aether/workspaces/<name>.toml`. Omit to infer from PATH;
    /// with no PATH (or one outside every workspace), the picker opens.
    #[arg(short = 'w', long)]
    workspace: Option<String>,

    /// File or directory to open. Omit for a scratch buffer.
    ///
    /// Resolved against the working directory; infers the workspace when `--workspace` is absent. A
    /// directory opens the file browser there.
    path: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let version = env!("CARGO_PKG_VERSION").to_string();

    // Pin the profile for this whole process *before* anything resolves a config path or a port.
    aether_server::set_active_profile(
        cli.profile
            .clone()
            .unwrap_or_else(|| aether_server::DEFAULT_PROFILE.to_string()),
    );

    match cli.command {
        Some(Command::Server(args)) => run_server(args),
        Some(Command::Edit(edit)) => run_edit(edit, version),
        Some(Command::Profiles(args)) => run_profiles(args),
        None => run_edit(cli.edit, version),
    }
}

/// Run the default `edit` command: resolve the workspace (explicit, inferred, or picker), resolve the
/// active profile's port (creating the profile on first use), make sure a server is up on it, then
/// launch the terminal or GUI client pointed at it.
fn run_edit(edit: EditArgs, version: String) -> anyhow::Result<()> {
    let workspace = resolve_workspace(&edit)?;
    let port = aether_server::ensure_profile_port()?;
    let idle_timeout_secs = aether_server::profile_idle_timeout_secs()?;
    ensure_server_running(port, idle_timeout_secs);
    let server_url = format!("ws://127.0.0.1:{port}");
    if want_gui(&edit) {
        run_gui(
            workspace,
            edit.path,
            version,
            server_url,
            aether_server::active_profile().to_string(),
        )
    } else {
        run_tui(workspace, edit.path, version, server_url)
    }
}

/// `ae profiles list` — show each profile, its recorded port, and whether its server is up.
fn run_profiles(args: ProfilesArgs) -> anyhow::Result<()> {
    match args.command {
        ProfilesCommand::List => {
            let profiles = aether_server::list_profiles()?;
            if profiles.is_empty() {
                println!("no profiles yet — one is created on first use");
                return Ok(());
            }
            for p in profiles {
                let status = if server_is_up(p.port) {
                    "running"
                } else {
                    "stopped"
                };
                println!("{:<16} port {}  {status}", p.name, p.port);
            }
            Ok(())
        }
    }
}

/// Make sure a server is listening on `port` before we launch a client. The client connects with
/// retry, so we only need to *start* one if none is up: probe the port, and if nothing answers,
/// spawn a detached, idle-reapable `ae server` for the active profile (reaping after
/// `idle_timeout_secs`, the profile's setting). The spawned server outlives this client (and any
/// later one) until it idle-reaps itself. A lost race — two clients each spawning, or a stale
/// process holding the port — is harmless: the redundant server fails to bind and exits.
/// Best-effort: on failure we warn and let the client's connect loop surface it.
fn ensure_server_running(port: u16, idle_timeout_secs: u64) {
    if server_is_up(port) {
        return;
    }
    if let Err(e) = spawn_detached_server(idle_timeout_secs) {
        eprintln!("warning: could not auto-start the aether server ({e}); is one running?");
    }
}

/// Whether something is already listening on `port`. A short connect probe is enough — a
/// stale/foreign listener is caught later by the WebSocket version gate, not here.
fn server_is_up(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// Spawn `ae --profile <name> server --idle-timeout N` from our own executable, detached so it
/// outlives this client and is insulated from the controlling terminal (no SIGINT on Ctrl-C, no
/// SIGHUP on terminal close). The spawned server resolves its port from the same `profile.toml` we
/// just read — the port is never passed on the CLI. stdio goes to the void — a user who wants server
/// logs runs `ae server` by hand.
fn spawn_detached_server(idle_timeout_secs: u64) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let exe = server_spawn_exe(
        std::env::current_exe()?,
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
    );
    let mut cmd = Command::new(exe);
    // `ae server --profile X --idle-timeout N` — the spawned server resolves its port from the same
    // profile.toml we just read (the port is never passed on the CLI).
    cmd.arg("server")
        .arg("--profile")
        .arg(aether_server::active_profile())
        .arg("--idle-timeout")
        .arg(idle_timeout_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

/// Which executable to spawn the detached server from. Normally our own binary — but inside an
/// AppImage, `current_exe()` resolves into the transient FUSE mount (`/tmp/.mount_XXXX/usr/bin/ae`)
/// that belongs to *this* launch, so a server spawned from there would outlive its own binary and
/// keep the mount pinned. The AppImage runtime exports `APPIMAGE` (the image's on-disk path) and
/// `APPDIR` (the mount point); when we're genuinely running from that mount, spawn the AppImage
/// itself so the server is an independent launch with its own lifetime. The `starts_with` guard
/// keeps an APPIMAGE var merely *inherited* from some other AppImage'd parent (say, a terminal
/// emulator) from hijacking the spawn when this `ae` is a plain binary.
fn server_spawn_exe(
    current: std::path::PathBuf,
    appimage: Option<std::ffi::OsString>,
    appdir: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    match (appimage, appdir) {
        (Some(image), Some(dir)) if current.starts_with(&dir) => image.into(),
        _ => current,
    }
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

/// Decide which workspace to open. `--workspace` always wins. Otherwise infer from the PATH: a path
/// inside exactly one workspace opens there; a path inside *several* is an error the user must
/// disambiguate. A path inside *no* configured workspace is no longer an error — we return `None`,
/// and the client opens the file directly in an ephemeral "(no workspace)" context (`ae /etc/hosts`).
/// With no PATH at all, we return `None` so a bare `ae` opens the workspace picker — the working
/// directory is deliberately *not* used to guess a workspace (it only resolves relative file paths).
fn resolve_workspace(edit: &EditArgs) -> anyhow::Result<Option<String>> {
    use aether_server::WorkspaceMatch;

    if edit.workspace.is_some() {
        return Ok(edit.workspace.clone());
    }

    if let Some(path) = &edit.path {
        return match aether_server::infer_workspace_for_path(std::path::Path::new(path))? {
            WorkspaceMatch::One(name) => Ok(Some(name)),
            // Outside every workspace: open workspace-lessly (the client falls back to open-from-path).
            WorkspaceMatch::None => Ok(None),
            WorkspaceMatch::Ambiguous(names) => anyhow::bail!(
                "{path} is within multiple workspaces ({}) — disambiguate with `--workspace NAME`",
                names.join(", ")
            ),
        };
    }

    // No `--workspace` and no path: open the workspace picker rather than guessing from the working
    // directory. The working directory is only meaningful for resolving a relative *file* path
    // (the `path` branch above, and the open-from-path overlay), not as a standalone workspace
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
    if let Some(ServerCommand::Stop) = args.command {
        return stop_server();
    }
    let filter = match args.log {
        Some(filter) => tracing_subscriber::EnvFilter::new(filter),
        None => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_server=info,info")),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let idle_timeout = args.idle_timeout.map(std::time::Duration::from_secs);
    runtime()?.block_on(aether_server::run(idle_timeout))
}

/// `ae server stop` — signal the running server for the active profile to shut down (SIGTERM, which
/// the accept loop handles gracefully), and wait briefly for it to release the port so a follow-up
/// `ae server` / client can bind immediately.
fn stop_server() -> anyhow::Result<()> {
    let profile = aether_server::active_profile();
    let Some(pid) = aether_server::running_server_pid()? else {
        println!("no server running for profile '{profile}'");
        return Ok(());
    };
    terminate(pid)?;
    // Poll for exit (graceful shutdown is near-instant); don't hang forever if it ignores us.
    for _ in 0..40 {
        if aether_server::running_server_pid()?.is_none() {
            println!("stopped server (pid {pid}) for profile '{profile}'");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("sent stop to server (pid {pid}) for profile '{profile}'; still shutting down");
    Ok(())
}

#[cfg(unix)]
fn terminate(pid: u32) -> anyhow::Result<()> {
    // SAFETY: `kill` with SIGTERM has no effect on this process and just signals `pid`.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == -1 {
        return Err(std::io::Error::last_os_error()).context("sending SIGTERM");
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> anyhow::Result<()> {
    anyhow::bail!("`ae server stop` is not supported on this platform")
}

fn run_tui(
    workspace: Option<String>,
    path: Option<String>,
    version: String,
    server_url: String,
) -> anyhow::Result<()> {
    runtime()?.block_on(aether_tui::run(workspace, path, version, server_url))
}

#[cfg(feature = "gui")]
fn run_gui(
    workspace: Option<String>,
    path: Option<String>,
    version: String,
    server_url: String,
    profile: String,
) -> anyhow::Result<()> {
    // iced owns the main thread and manages its own tokio runtime, so this is a synchronous call —
    // do not wrap it in `runtime().block_on`, which would panic on a nested runtime.
    aether_iced::run(workspace, path, version, server_url, profile)
}

#[cfg(not(feature = "gui"))]
fn run_gui(
    _workspace: Option<String>,
    _path: Option<String>,
    _version: String,
    _server_url: String,
    _profile: String,
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
    fn bare_invocation_opens_the_picker_not_a_cwd_inferred_workspace() {
        // No `--workspace` and no path → `None`, so the client opens the workspace chooser. The key
        // guarantee: this branch never consults the working directory to guess a workspace.
        assert_eq!(resolve_workspace(&EditArgs::default()).unwrap(), None);
    }

    #[test]
    fn explicit_workspace_flag_wins() {
        let edit = EditArgs {
            workspace: Some("myproj".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_workspace(&edit).unwrap(),
            Some("myproj".to_string())
        );
    }

    #[test]
    fn server_spawns_from_the_appimage_when_running_inside_its_mount() {
        let exe = server_spawn_exe(
            "/tmp/.mount_aeXYZ/usr/bin/ae".into(),
            Some("/home/joe/apps/aether.AppImage".into()),
            Some("/tmp/.mount_aeXYZ".into()),
        );
        assert_eq!(exe, std::path::Path::new("/home/joe/apps/aether.AppImage"));
    }

    #[test]
    fn inherited_appimage_vars_do_not_hijack_a_plain_binary_spawn() {
        // `ae` launched from inside some *other* AppImage'd app (a terminal emulator, say)
        // inherits its APPIMAGE/APPDIR — but our exe isn't under that mount, so spawn ourselves.
        let exe = server_spawn_exe(
            "/usr/local/bin/ae".into(),
            Some("/home/joe/apps/kitty.AppImage".into()),
            Some("/tmp/.mount_kitty1".into()),
        );
        assert_eq!(exe, std::path::Path::new("/usr/local/bin/ae"));
    }

    #[test]
    fn no_appimage_vars_means_spawn_current_exe() {
        let exe = server_spawn_exe("/usr/local/bin/ae".into(), None, None);
        assert_eq!(exe, std::path::Path::new("/usr/local/bin/ae"));

        // APPIMAGE without APPDIR (or vice versa) is not a trustworthy AppImage context either.
        let exe = server_spawn_exe(
            "/usr/local/bin/ae".into(),
            Some("/home/joe/apps/aether.AppImage".into()),
            None,
        );
        assert_eq!(exe, std::path::Path::new("/usr/local/bin/ae"));
    }
}
