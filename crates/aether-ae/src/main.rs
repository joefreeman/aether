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
//! no subcommand. The project is inferred from PATH (or the current directory) by matching against
//! configured project roots; `-p/--project` overrides that, and `ae edit ...` is the explicit form
//! for when a PATH would otherwise collide with the `server` subcommand name.
//!
//! iced owns the main thread and its own tokio runtime, so the GUI client is dispatched as a plain
//! synchronous call; the server and terminal client are `async` and get a runtime built here.

use anyhow::Context;
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
    /// `$XDG_CONFIG_HOME/aether/projects/<name>.toml`. Omit to infer the project from PATH (or, with
    /// no PATH, from the current directory); with neither resolving, the project picker opens.
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

/// Run the default `edit` command: resolve the project (explicit, inferred, or picker), then launch
/// the terminal or GUI client.
fn run_edit(edit: EditArgs, version: String) -> anyhow::Result<()> {
    let project = resolve_project(&edit)?;
    if want_gui(&edit) {
        run_gui(project, edit.path, version)
    } else {
        run_tui(project, edit.path, version)
    }
}

/// Decide which project to open. `--project` always wins. Otherwise infer from the PATH: a path
/// inside exactly one project opens there; a path inside *several* is an error the user must
/// disambiguate. A path inside *no* configured project is no longer an error — we return `None`,
/// and the client opens the file directly in an ephemeral "(no project)" context (`ae /etc/hosts`).
/// With no PATH, infer from the current directory but fall back to the picker (a bare `ae` in an
/// unconfigured dir shouldn't be a hard error).
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

    let cwd = std::env::current_dir().context("determining the current directory")?;
    match aether_server::infer_project_for_path(&cwd)? {
        ProjectMatch::One(name) => Ok(Some(name)),
        ProjectMatch::None | ProjectMatch::Ambiguous(_) => Ok(None),
    }
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
    runtime()?.block_on(aether_server::run())
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
