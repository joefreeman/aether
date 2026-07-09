//! `aether-iced` — the iced (native GUI) Aether client, driven by the shared `aether-client` core.
//! Connection + buffer bootstrap happen on a dedicated tokio runtime before iced takes over the
//! main thread; the WebSocket actor stays on that runtime for the app's lifetime. [`run`] is the
//! single entry point the `ae` binary calls for the GUI client.

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

/// The active profile name, stashed once at [`run`] so the "open another window" path
/// (`Space Alt-x`) can spawn a sibling `ae --gui` pointed at the *same* profile — and thus the same
/// daemon. Process-global because it never changes over a run; mirrors how the server holds its own
/// active profile rather than threading it through every call.
static PROFILE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The profile this GUI client is running under (defaulting if [`run`] never set it — only the
/// case in tests, which don't spawn windows). Used to build the `--profile` arg of a spawned window.
pub(crate) fn active_profile() -> &'static str {
    PROFILE.get().map(String::as_str).unwrap_or("default")
}

/// Run the native GUI client to completion. `workspace`/`file` are the (optional) CLI positionals,
/// `version` is the handshake version string, `server_url` is the (profile-resolved) WebSocket
/// address to dial, and `profile` is the active profile name (recorded for window-spawning). iced
/// owns the main thread and manages its own tokio runtime, so unlike the terminal client this is a
/// synchronous call (not awaited on a runtime the caller provides).
#[allow(clippy::too_many_arguments)]
pub fn run(
    workspace: Option<String>,
    file: Option<String>,
    jump: Option<(u32, u32)>,
    buffer_id: Option<u64>,
    version: String,
    server_url: String,
    profile: String,
) -> anyhow::Result<()> {
    let _ = PROFILE.set(profile);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("aether_iced=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Launch straight into the app in a connectionless "Connecting…" state — no blocking dial up
    // front, so the client can start before the daemon and wait for it immersively. The app dials
    // `server_url` from within, on iced's own runtime, and installs the session once the socket lands.
    app::run(app::Bootstrap::Connecting(app::ConnectingBootstrap {
        workspace,
        file,
        jump_to: jump.map(|(line, col)| aether_protocol::LogicalPosition { line, col }),
        buffer_id,
        client_version: version,
        server_url,
    }))
    .map_err(|e| anyhow!("iced: {e}"))
}
