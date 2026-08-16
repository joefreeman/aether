//! `aether-iced` — the iced (native GUI) Aether client, driven by the shared `aether-client` core.
//! Connection + buffer bootstrap happen on a dedicated tokio runtime before iced takes over the
//! main thread; the WebSocket actor stays on that runtime for the app's lifetime. [`run`] is the
//! single entry point the `ae` binary calls for the GUI client.

mod alt_filter;
mod app;
mod connection;
mod editor;
mod input;
#[cfg(target_os = "macos")]
mod mac_open;
mod picker;
mod theme;

// The core crate under the path the shell has always used, plus its modules at their
// pre-extraction paths so references didn't churn during the seam work (docs/client-core.md).
pub(crate) use aether_client as core;
pub(crate) use aether_client::{chips, grid, keymap, labels};

use anyhow::anyhow;

/// Run the native GUI client to completion. `workspace`/`file` are the (optional) CLI positionals,
/// `version` is the handshake version string and `server_url` is the (profile-resolved) WebSocket
/// address to dial — which is all a second window needs, since windows are in-process now. iced
/// owns the main thread and manages its own tokio runtime, so unlike the terminal client this is a
/// synchronous call (not awaited on a runtime the caller provides).
#[allow(clippy::too_many_arguments)]
pub fn run(
    workspace: Option<String>,
    file: Option<String>,
    jump: Option<(u32, u32)>,
    buffer_id: Option<u64>,
    tether: bool,
    version: String,
    server_url: String,
) -> anyhow::Result<()> {
    // Prefer the integrated GPU on hybrid-graphics machines. iced/wgpu default to the
    // HighPerformance adapter, which lands rendering on the discrete GPU even when every display
    // is driven by the integrated one — a text editor gains nothing from that, and a discrete GPU
    // with flaky power management can stall presentation fences and freeze the window outright.
    // iced exposes no settings field for adapter choice; wgpu's env override is the only lever,
    // so default it here (before iced spawns any threads) and let an explicit value win.
    if std::env::var_os("WGPU_POWER_PREF").is_none() {
        std::env::set_var("WGPU_POWER_PREF", "low");
    }
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
        tether,
        client_version: version,
        server_url,
    }))
    .map_err(|e| anyhow!("iced: {e}"))
}
