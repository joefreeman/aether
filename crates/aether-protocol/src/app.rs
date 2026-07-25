//! Application info & diagnostics — the payload behind `Space ?` and `ae server status`.
//!
//! One snapshot answering the questions you can't answer from inside the editor: *which build is
//! this, which daemon am I talking to, and where does its state live?* Aether is a client–server
//! editor with profiles and an auto-started server, so "the editor" is really two processes whose
//! identities can diverge — and the state each profile owns is scattered across a config subtree, a
//! state subtree, and several JSON/TOML files.
//!
//! This type is served two ways from one builder (`aether_server::status`): as the `app/info` RPC
//! result, and as the body of the out-of-band `GET /status` that `ae server status` and the web
//! client's staleness check read. Keeping one struct means the dialog and the CLI can't drift.
//!
//! **Wire contract**: fields are additive and flat. Flat because the web client reads `version` off
//! `/status` to decide whether its cached bundle is outdated (`web/src/client.ts`) — a *stale*
//! bundle has to keep finding that field on a *newer* server's response, or the very check that
//! tells it to reload would break on the upgrade that moved it. Nothing here may be nested or
//! renamed for that reason; new fields go on the end with a serde default.

use crate::envelope::RpcMethod;
use serde::{Deserialize, Serialize};

/// Snapshot the running application: build identity, live instance, and on-disk state locations.
/// Read-only and cheap (counts and path derivations), so the client re-fetches per dialog open
/// rather than caching — the numbers are stale the moment they're rendered otherwise.
pub struct AppInfoGet;
impl RpcMethod for AppInfoGet {
    const NAME: &'static str = "app/info";
    type Params = AppInfoParams;
    type Result = AppInfo;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AppInfoParams {}

/// A snapshot of the running application. Describes the **server**: on native shells the client is
/// the same binary, so its build fields describe both, but the web client's bundle can lag behind
/// the daemon that serves it — which is why the client compares these against its own compiled-in
/// [`crate::PROTOCOL_VERSION`] / [`crate::BUILD_COMMIT`] rather than assuming they match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    // ---- build ----
    /// Release version ([`crate::PROTOCOL_VERSION`]).
    pub version: String,
    /// Short git SHA the server was built from ([`crate::BUILD_COMMIT`]); absent outside a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The server's tree had uncommitted changes at build time ([`crate::BUILD_DIRTY`]).
    #[serde(default)]
    pub commit_dirty: bool,
    /// The server is a debug build ([`crate::BUILD_DEBUG`]).
    #[serde(default)]
    pub debug_build: bool,
    /// Running from an AppImage (`$APPIMAGE` set at server start), whose path this is. Worth
    /// distinguishing: the AppImage self-spawns via `$APPIMAGE` and bundles its own runtime, so
    /// "which binary" has a different answer than for a cargo-built one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appimage: Option<String>,

    // ---- instance ----
    /// Active profile name (`--profile` / `AETHER_PROFILE`, else `default`). The single most
    /// load-bearing row: every path below, the port, and the whole state subtree hang off it.
    pub profile: String,
    /// The loopback port this server is listening on. `None` only for an embedded/in-process server
    /// that never recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The server process's pid.
    pub pid: u32,
    /// When this instance started (unix ms) — also its restart-detection identity, echoed on
    /// `workspace/activate`.
    pub started_at_unix_ms: u64,
    /// Seconds since start, computed server-side so the client needs no clock (the core is sans-IO
    /// and has none).
    #[serde(default)]
    pub uptime_secs: u64,
    /// Idle-reaper setting: `Some(secs)` is a client-conjured instance that self-reaps after that
    /// many idle seconds; `None` is the persistent `ae server` daemon. Explains a vanished session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// Connected clients right now (TUI / GUI / web sessions).
    pub clients: usize,
    /// Open buffers across all workspaces.
    pub buffers_open: usize,
    /// How many open buffers have unsaved edits — what you'd want to know before `ae server stop`.
    pub buffers_unsaved: usize,
    /// Activated (loaded) workspaces.
    pub workspaces_active: usize,

    // ---- paths ----
    /// Where this profile's state lives on disk. Profile-scoped, so non-obvious.
    #[serde(default)]
    pub paths: AppPaths,
}

/// On-disk locations for the active profile. Every field is `Option` because resolution can fail
/// (no XDG base dirs) — the same failure that disables the corresponding feature server-side, so an
/// absent row is itself the diagnostic. Rendered in the dialog and by `ae server status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPaths {
    /// `<config>/aether/profiles/<name>/` — user-authored durable config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    /// `<state>/aether/profiles/<name>/` — machine-managed state (sessions, backups).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<String>,
    /// `settings.toml` (app-wide preferences, `Space .`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<String>,
    /// `sessions.json` — workspace recency + dormant buffer restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<String>,
    /// `hints.json` — hint learning state (docs/hints.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<String>,
    /// `backups/` — unsaved-buffer backups (docs/unsaved-persistence.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backups: Option<String>,
}
