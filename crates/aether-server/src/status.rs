//! The application snapshot: build identity, live instance, and on-disk state locations.
//!
//! One builder, two consumers. [`AppInfo`] is the result of the `app/info` RPC (the `Space ?`
//! dialog) *and* the body of `GET /status` — an out-of-band diagnostic on the same loopback port
//! (see [`crate::http::serve_http`]) that `ae server status` and the web client's staleness check
//! read with a plain HTTP GET, no WebSocket handshake needed. Serving one struct both ways is
//! deliberate: a separate CLI-only shape drifted from the dialog's the moment either grew a field.
//!
//! The type itself lives in `aether-protocol` (it *is* a protocol message now); the fetch helper
//! lives here so the wire contract — what the server writes and how a short-lived CLI reads it
//! back — stays in one place.

use aether_protocol::app::{AppInfo, AppPaths};
use anyhow::Context;

/// Build the snapshot from the authoritative in-memory state. Cheap — counts plus a handful of path
/// derivations — so it's fine to call under the state lock, and cheap enough that the dialog
/// re-fetches on every open rather than caching numbers that go stale immediately.
pub fn app_info(s: &crate::state::ServerState) -> AppInfo {
    let now = crate::config::now_unix_ms();
    AppInfo {
        version: aether_protocol::PROTOCOL_VERSION.to_string(),
        commit: aether_protocol::BUILD_COMMIT.map(str::to_string),
        commit_dirty: aether_protocol::BUILD_DIRTY,
        debug_build: aether_protocol::BUILD_DEBUG,
        // Read at snapshot time rather than cached at boot: it's the environment of the *server*
        // process, which is the binary this whole struct describes.
        appimage: std::env::var("APPIMAGE").ok().filter(|p| !p.is_empty()),
        profile: crate::config::active_profile().to_string(),
        port: s.port,
        pid: std::process::id(),
        started_at_unix_ms: s.started_at_unix_ms,
        // Saturating: a backwards clock jump reads as zero uptime rather than wrapping to ~584
        // million years, which is the more useful lie.
        uptime_secs: now.saturating_sub(s.started_at_unix_ms) / 1000,
        idle_timeout_secs: s.idle_timeout.map(|d| d.as_secs()),
        clients: s.clients.len(),
        buffers_open: s.buffers.len(),
        buffers_unsaved: s.buffers.values().filter(|b| b.dirty).count(),
        workspaces_active: s.workspaces.len(),
        paths: paths(),
    }
}

/// Resolve the active profile's two on-disk roots. Everything the profile persists lives at a
/// fixed name under one of them (`settings.toml`, `workspaces/` under config; `sessions.json`,
/// `hints.json`, `backups/` under state) and resolves iff its base does, so only the bases
/// travel. A failure here (no XDG base dirs) mirrors the features being disabled server-side —
/// an absent row in the dialog is itself the diagnostic, not a rendering gap.
fn paths() -> AppPaths {
    let show = |p: anyhow::Result<std::path::PathBuf>| p.ok().map(|p| p.display().to_string());
    AppPaths {
        config_dir: show(crate::config::profile_config_dir()),
        state_dir: show(crate::config::profile_state_dir()),
    }
}

/// Fetch `/status` from a running server on `port` over a blocking loopback HTTP GET.
///
/// Blocking (std::net, not tokio) because `ae server status` runs outside any async runtime. Short
/// timeouts so a wedged server — port open but not serving — surfaces as an error the caller reports
/// as "unhealthy" rather than hanging. The `Host` header must name a loopback authority or the
/// server 403s it (its DNS-rebinding guard — see [`crate::http::is_loopback_authority`]).
pub fn fetch_status(port: u16) -> anyhow::Result<AppInfo> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .context("connecting to server")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let req =
        format!("GET /status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    // The server sends `Connection: close`, so the socket EOFs after the body and `read_to_end`
    // returns the whole response.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("malformed HTTP response from server")?;
    let status_line = head.lines().next().unwrap_or_default();
    if !status_line.contains(" 200") {
        anyhow::bail!("server returned {status_line:?}");
    }
    serde_json::from_str(body.trim()).context("parsing /status JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_roundtrips() {
        let s = app_info(&crate::state::ServerState::new());
        let json = serde_json::to_string(&s).unwrap();
        let back: AppInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    /// Unknown fields (a newer server) don't break deserialization — the CLI stays
    /// forward-compatible. The counterpart property (an *older* payload missing today's fields) is
    /// covered by the serde defaults exercised below.
    #[test]
    fn status_ignores_unknown_fields() {
        let json = r#"{
            "version": "1.0.0",
            "profile": "default",
            "pid": 1,
            "started_at_unix_ms": 0,
            "clients": 0,
            "buffers_open": 0,
            "buffers_unsaved": 0,
            "workspaces_active": 0,
            "future_field": "ignored"
        }"#;
        let s: AppInfo = serde_json::from_str(json).unwrap();
        assert_eq!(s.version, "1.0.0");
        assert_eq!(s.idle_timeout_secs, None);
        // Everything the old `/status` shape didn't carry defaults rather than failing the parse,
        // so a freshly-installed CLI can still read a running older daemon.
        assert_eq!(s.commit, None);
        assert_eq!(s.uptime_secs, 0);
        assert_eq!(s.paths, AppPaths::default());
    }

    /// `version` must stay a top-level field: the web client reads it off `/status` to decide its
    /// cached bundle is stale, and a nested/renamed field would break exactly the check that tells
    /// an outdated bundle to reload. See `web/src/client.ts`.
    #[test]
    fn version_is_top_level_on_the_wire() {
        let s = app_info(&crate::state::ServerState::new());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v.get("version").and_then(|v| v.as_str()),
            Some(aether_protocol::PROTOCOL_VERSION)
        );
    }
}
