//! `ae-iced` — the iced (native GUI) Aether client, milestone 1: a single-buffer editor with
//! modal input and native pixel scrolling. Connection + buffer bootstrap happen on a dedicated
//! tokio runtime before iced takes over the main thread; the WebSocket actor stays on that
//! runtime for the app's lifetime.

mod alt_filter;
mod app;
mod connection;
mod editor;
mod input;
mod picker;
mod theme;

// The core crate under the path the shell has always used, plus its modules at their
// pre-extraction paths so references didn't churn during the seam work (docs/client-core.md).
pub(crate) use aether_client as core;
pub(crate) use aether_client::{chips, grid, keymap, labels};

use anyhow::anyhow;
use clap::Parser;

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

    // Launch straight into the app in a connectionless "Connecting…" state — no blocking dial up
    // front, so the client can start before the daemon and wait for it immersively. The app dials
    // (and waits for the server, fixed loopback address — no discovery file) from within, on iced's
    // own runtime, and installs the session once the socket lands.
    app::run(app::Bootstrap::Connecting(app::ConnectingBootstrap {
        project: cli.project,
        file: cli.file,
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    }))
    .map_err(|e| anyhow!("iced: {e}"))
}
