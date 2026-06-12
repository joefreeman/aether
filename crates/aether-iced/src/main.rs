//! `ae-iced` — the iced (native GUI) Aether client, milestone 1: a single-buffer editor with
//! modal input and native pixel scrolling. Connection + buffer bootstrap happen on a dedicated
//! tokio runtime before iced takes over the main thread; the WebSocket actor stays on that
//! runtime for the app's lifetime.

mod app;
mod connection;
mod discovery;
mod editor;
mod input;
mod picker;
mod theme;

// The core crate under the path the shell has always used, plus its modules at their
// pre-extraction paths so references didn't churn during the seam work (docs/client-core.md).
pub(crate) use aether_client as core;
pub(crate) use aether_client::{chips, grid, keymap, labels};

use anyhow::{anyhow, bail, Context};
use aether_protocol::buffer::{BufferOpen, BufferOpenParams};
use aether_protocol::project::{ProjectActivate, ProjectActivateParams};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(name = "ae-iced", version, about = "Aether editor — iced client (dev binary)")]
struct Cli {
    /// Project name (must have a config at `$XDG_CONFIG_HOME/aether/projects/<name>.toml`).
    /// Omit to start with the project picker open, like the other clients.
    project: Option<String>,
    /// File to open, resolved against the current working directory; must fall within one of
    /// the project's roots. Omit to attach to the project's most recent buffer (or a scratch).
    /// Ignored when no project is selected.
    file: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ae_iced=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    let bootstrap = rt.block_on(bootstrap(&cli))?;
    // `rt` stays alive past `run` (the connection actor lives on it); it drops on exit.
    app::run(bootstrap).map_err(|e| anyhow!("iced: {e}"))
}

async fn bootstrap(cli: &Cli) -> anyhow::Result<app::Bootstrap> {
    let info = discovery::read()?;
    let base_url = format!("ws://127.0.0.1:{}", info.port);
    let (handle, notifications) =
        connection::connect(&base_url, env!("CARGO_PKG_VERSION")).await?;

    // No project named on the CLI: start with the project picker open. Activation (and the
    // first tab's own connection) happens when the user picks one.
    let Some(project) = &cli.project else {
        return Ok(app::Bootstrap::Choose(app::ChooseBootstrap {
            handle,
            notifications: Arc::new(Mutex::new(notifications)),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            server_started_at: info.started_at_unix_ms,
        }));
    };

    let activated = handle
        .rpc::<ProjectActivate>(ProjectActivateParams {
            name: project.clone(),
        })
        .await?;
    let project_paths = activated.project.paths.clone();

    let params = match &cli.file {
        Some(f) => {
            let abs = resolve_cli_path(f)?;
            if abs.is_dir() {
                bail!("{} is a directory — the iced client can't browse yet", abs.display());
            }
            let abs_str = abs.display().to_string();
            let (path_index, relative_path) = app::strip_longest_root(&abs_str, &project_paths)
                .ok_or_else(|| anyhow!("{} is outside the project's roots", abs.display()))?;
            BufferOpenParams {
                path_index: Some(path_index),
                relative_path: Some(relative_path),
                ..Default::default()
            }
        }
        // No file: attach to the most recent buffer, or a transient scratch placeholder —
        // same convention as the TUI's bootstrap.
        None => BufferOpenParams {
            buffer_id: activated.last_buffer_id,
            transient: if activated.last_buffer_id.is_none() {
                Some(true)
            } else {
                None
            },
            ..Default::default()
        },
    };
    let open = handle.rpc::<BufferOpen>(params).await?;

    Ok(app::Bootstrap::Session(Box::new(app::SessionBootstrap {
        handle,
        notifications: Arc::new(Mutex::new(notifications)),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        server_started_at: info.started_at_unix_ms,
        project: activated.project.name,
        buffer: app::buffer_info(open, &project_paths),
        project_paths,
    })))
}

/// Resolve a CLI path against the current working directory (shell-conventional).
fn resolve_cli_path(input: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(input);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    abs.canonicalize()
        .with_context(|| format!("resolving {}", abs.display()))
}
