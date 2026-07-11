//! The `/status` payload: a read-only snapshot of the running server, consumed by `ae server
//! status`. This is *not* part of the JSON-RPC wire protocol — it's an out-of-band diagnostic
//! served over the same loopback port's HTTP surface (see [`crate::http::serve_http`]), so a
//! short-lived CLI can read it with a plain HTTP GET instead of a WebSocket + JSON-RPC handshake.
//!
//! The type lives here (server-side) rather than in `aether-protocol` because it isn't a protocol
//! message; the fetch helper lives here too so the wire contract — what the server writes and how a
//! client reads it back — stays in one place.

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// A snapshot of the running server, returned as JSON from `GET /status`. Fields are additive: the
/// CLI deserializes leniently, so a newer server can add fields without breaking an older `ae`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The running server's build version ([`aether_protocol::PROTOCOL_VERSION`]). The CLI compares
    /// it to its own to flag a version drift — an old daemon that a freshly-installed client can't
    /// even connect to, because the handshake version gate would reject it (see `crate::connection`).
    pub version: String,
    /// When this instance started (unix ms). Drives an accurate uptime, independent of the runtime
    /// file's mtime.
    pub started_at_unix_ms: u64,
    /// Connected clients right now (TUI / GUI / web sessions).
    pub clients: usize,
    /// Open buffers across all workspaces.
    pub buffers_open: usize,
    /// How many open buffers have unsaved edits — what you'd want to know before `ae server stop`.
    pub buffers_unsaved: usize,
    /// Activated (loaded) workspaces.
    pub workspaces_active: usize,
    /// Idle-reaper setting: `Some(secs)` is a client-conjured instance that self-reaps after that
    /// many idle seconds; `None` is the persistent `ae server` daemon.
    pub idle_timeout_secs: Option<u64>,
}

impl ServerStatus {
    /// Build the snapshot from the authoritative in-memory state. Pure and cheap — just reads
    /// counts — so it's fine to call under the state lock.
    pub fn from_state(s: &crate::state::ServerState) -> Self {
        ServerStatus {
            version: aether_protocol::PROTOCOL_VERSION.to_string(),
            started_at_unix_ms: s.started_at_unix_ms,
            clients: s.clients.len(),
            buffers_open: s.buffers.len(),
            buffers_unsaved: s.buffers.values().filter(|b| b.dirty).count(),
            workspaces_active: s.workspaces.len(),
            idle_timeout_secs: s.idle_timeout.map(|d| d.as_secs()),
        }
    }
}

/// Fetch `/status` from a running server on `port` over a blocking loopback HTTP GET.
///
/// Blocking (std::net, not tokio) because `ae server status` runs outside any async runtime. Short
/// timeouts so a wedged server — port open but not serving — surfaces as an error the caller reports
/// as "unhealthy" rather than hanging. The `Host` header must name a loopback authority or the
/// server 403s it (its DNS-rebinding guard — see [`crate::http::is_loopback_authority`]).
pub fn fetch_status(port: u16) -> anyhow::Result<ServerStatus> {
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
        let s = ServerStatus {
            version: "9.9.9".into(),
            started_at_unix_ms: 1_700_000_000_000,
            clients: 2,
            buffers_open: 5,
            buffers_unsaved: 1,
            workspaces_active: 3,
            idle_timeout_secs: Some(300),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ServerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, "9.9.9");
        assert_eq!(back.buffers_unsaved, 1);
        assert_eq!(back.idle_timeout_secs, Some(300));
    }

    /// Unknown fields (a newer server) don't break deserialization — the CLI stays forward-compatible.
    #[test]
    fn status_ignores_unknown_fields() {
        let json = r#"{
            "version": "1.0.0",
            "started_at_unix_ms": 0,
            "clients": 0,
            "buffers_open": 0,
            "buffers_unsaved": 0,
            "workspaces_active": 0,
            "idle_timeout_secs": null,
            "future_field": "ignored"
        }"#;
        let s: ServerStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.version, "1.0.0");
        assert_eq!(s.idle_timeout_secs, None);
    }
}
