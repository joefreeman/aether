//! Minimal HTTP responder for the embedded web client, plus the connection router that
//! multiplexes plain HTTP and WebSocket traffic on the single loopback port.
//!
//! The editor protocol runs over WebSocket (JSON-RPC). For the browser client the same port also
//! serves the static web bundle over HTTP. We tell the two apart by *peeking* the start of the
//! connection: a WebSocket upgrade carries the mandatory `Sec-WebSocket-Key` header; anything
//! else is treated as a plain HTTP GET. `peek` leaves the bytes in the socket queue, so the
//! downstream handler (the WS handshake, or our own request reader) re-reads the full request.
//!
//! Serving: the server owns a fixed `index.html` (the `INDEX_HTML` shell below) and serves only the
//! built JS/CSS bundle from `web/dist/assets/*` on disk. The shell references stable, unhashed asset
//! paths (`/assets/index.js`, `/assets/index.css` — pinned in `web/vite.config.ts`), so it never
//! changes between builds: `index.html` isn't a build artifact, only the bundle is. The dist path is
//! baked from `CARGO_MANIFEST_DIR`, so a rebuilt bundle is picked up with no rebuild of the daemon
//! (fine for a single-machine personal editor; not relocatable). Asset responses carry
//! `Cache-Control: no-store` since the unhashed names can't cache-bust.
//!
//! Authorization: there is no token. The listener is loopback-only, and both transports reject any
//! request whose `Host` (and, for browser clients, `Origin`) isn't our loopback authority — see
//! [`is_loopback_authority`]. That defeats DNS-rebinding (a rebound request carries the attacker's
//! hostname) and stops a cross-site page from connecting. The trade-off is no isolation between
//! local users on a shared machine, which is acceptable for a single-user personal editor.

use crate::state::SharedState;
use anyhow::Context;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Built web-client output directory (just the JS/CSS bundle), resolved at compile time.
const WEB_DIST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist");

/// The page served at `/`: a fixed shell that loads the stable-named bundle from `web/dist/assets`.
/// Owned by the server (not emitted by Vite) so it's always present and never carries a build hash.
/// The dev counterpart is `web/index.html`, which instead loads `/src/main.ts` via the Vite dev
/// server; both mount `#app`, so keep those in sync.
const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Aether</title>
  <link rel="stylesheet" href="/assets/index.css" />
  <script type="module" src="/assets/index.js"></script>
</head>
<body>
  <div id="app"></div>
</body>
</html>
"#;

/// Peek the connection and route it: a WebSocket upgrade goes to the JSON-RPC connection handler,
/// everything else is served as HTTP. Bytes peeked here remain queued for the chosen handler.
pub async fn route(stream: TcpStream, state: SharedState) -> anyhow::Result<()> {
    let mut head = [0u8; 1024];
    let n = stream.peek(&mut head).await.context("peeking request head")?;
    if is_websocket_upgrade(&head[..n]) {
        crate::connection::handle(stream, state).await
    } else {
        serve_http(stream).await
    }
}

/// True if a `Host`/`Origin` header value addresses our loopback server. Only the hostname is
/// checked (any port), so it covers both the fixed production port and ephemeral test ports.
/// Rejecting non-loopback hostnames is what defeats DNS-rebinding: a rebound request reaches us
/// with the attacker's hostname in `Host`/`Origin`, not `127.0.0.1`/`localhost`. A scheme prefix
/// (present on `Origin`, absent on `Host`) is tolerated; the sandbox `Origin: null` is rejected.
pub(crate) fn is_loopback_authority(value: &str) -> bool {
    let without_scheme = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let authority = without_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(without_scheme);
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    host == "127.0.0.1" || host == "localhost"
}

/// True when the peeked head looks like a WebSocket upgrade. We match the mandatory
/// `Sec-WebSocket-Key` header case-insensitively rather than the path, so the browser can open its
/// socket at `/` exactly like the TUI does.
fn is_websocket_upgrade(head: &[u8]) -> bool {
    find_subslice(&head.to_ascii_lowercase(), b"sec-websocket-key").is_some()
}

async fn serve_http(mut stream: TcpStream) -> anyhow::Result<()> {
    let request = read_request_head(&mut stream).await?;

    // Reject anything whose `Host` isn't our loopback authority — a DNS-rebound request from a
    // malicious site still carries the attacker's hostname here, so this is the rebinding defense.
    if !request_host(&request).is_some_and(is_loopback_authority) {
        let resp = http_response("403 Forbidden", "text/plain; charset=utf-8", b"forbidden");
        stream.write_all(&resp).await?;
        stream.flush().await?;
        return Ok(());
    }

    // The request line is `GET /path?query HTTP/1.1`; route on the path, ignoring the query (the
    // client reads `?file=…` itself from the served page).
    let path = request_path(&request).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    let response = if path == "/" || path == "/index.html" {
        http_response("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes())
    } else if let Some((bytes, content_type)) = path.strip_prefix('/').and_then(load_asset) {
        http_response("200 OK", content_type, &bytes)
    } else {
        http_response("404 Not Found", "text/plain; charset=utf-8", b"not found")
    };
    stream.write_all(&response).await?;
    stream.flush().await?;
    Ok(())
}

/// Load a built asset by its URL-relative path (e.g. `assets/index.js`) from `web/dist`.
/// Returns the bytes and a content type, or `None` if the path escapes the dist dir or is missing.
fn load_asset(rel: &str) -> Option<(Vec<u8>, &'static str)> {
    if rel.contains("..") {
        return None;
    }
    let full: PathBuf = Path::new(WEB_DIST).join(rel);
    let bytes = std::fs::read(&full).ok()?;
    let content_type = match full.extension().and_then(|e| e.to_str()) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    };
    Some((bytes, content_type))
}

/// Read until the end of the HTTP header block (`\r\n\r\n`). We only serve GETs with no body, so
/// the headers are all we need. Capped so a misbehaving client can't make us read forever.
async fn read_request_head(stream: &mut TcpStream) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.context("reading request")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if find_subslice(&buf, b"\r\n\r\n").is_some() || buf.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Pull the path out of the request line (`GET /path HTTP/1.1`).
fn request_path(request: &str) -> Option<&str> {
    request.lines().next()?.split_whitespace().nth(1)
}

/// Pull the `Host` header value from the request head (header names are case-insensitive).
fn request_host(request: &str) -> Option<&str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host").then(|| value.trim())
    })
}

fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(body);
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_authority_accepts_local_host_and_origin() {
        // Host header forms (no scheme), any port.
        assert!(is_loopback_authority("127.0.0.1:2384"));
        assert!(is_loopback_authority("localhost:5173"));
        assert!(is_loopback_authority("127.0.0.1"));
        // Origin header forms (with scheme).
        assert!(is_loopback_authority("http://127.0.0.1:2384"));
        assert!(is_loopback_authority("http://localhost:5173"));
        assert!(is_loopback_authority("https://localhost"));
    }

    #[test]
    fn loopback_authority_rejects_foreign_and_null() {
        assert!(!is_loopback_authority("evil.com"));
        assert!(!is_loopback_authority("evil.com:2384"));
        assert!(!is_loopback_authority("http://evil.com:2384"));
        // A rebinding host that merely embeds the loopback string isn't loopback.
        assert!(!is_loopback_authority("127.0.0.1.evil.com"));
        assert!(!is_loopback_authority("localhostx"));
        // Sandboxed iframes send `Origin: null`.
        assert!(!is_loopback_authority("null"));
        assert!(!is_loopback_authority(""));
    }

    #[test]
    fn request_host_is_case_insensitive() {
        let req = "GET / HTTP/1.1\r\nhOsT:  127.0.0.1:2384\r\nConnection: close\r\n\r\n";
        assert_eq!(request_host(req), Some("127.0.0.1:2384"));
        assert_eq!(request_host("GET / HTTP/1.1\r\n\r\n"), None);
    }
}
