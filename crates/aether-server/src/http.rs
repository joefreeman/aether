//! Minimal HTTP responder for the embedded web client, plus the connection router that
//! multiplexes plain HTTP and WebSocket traffic on the single loopback port.
//!
//! The editor protocol runs over WebSocket (JSON-RPC). For the browser client the same port also
//! serves the static web bundle over HTTP. We tell the two apart by *peeking* the start of the
//! connection: a WebSocket upgrade carries the mandatory `Sec-WebSocket-Key` header; anything
//! else is treated as a plain HTTP GET. `peek` leaves the bytes in the socket queue, so the
//! downstream handler (the WS handshake, or our own request reader) re-reads the full request.
//!
//! Serving: when the web client has been built (`web/dist`, via `npm run build`), its bundle is
//! served — `index.html` from disk plus its hashed `/assets/*`. Otherwise we fall back to the
//! hand-written spike page (`web-spike/index.html`), which de-risked the auth/serving handshake
//! and keyboard capture. Either way the page's `__AETHER_TOKEN__` placeholder is replaced with the
//! live server token so the browser can open an authenticated WebSocket back to the same origin
//! without reading the runtime discovery file (which a browser can't). The dist path is baked from
//! `CARGO_MANIFEST_DIR`, so a built bundle is picked up automatically with no rebuild of the
//! daemon (fine for a single-machine personal editor; not relocatable).

use crate::state::SharedState;
use anyhow::Context;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Built web-client output directory, resolved at compile time relative to this crate.
const WEB_DIST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist");

/// Fallback page when `web/dist` hasn't been built.
const SPIKE_PAGE: &str = include_str!("../web-spike/index.html");

/// Peek the connection and route it: a WebSocket upgrade goes to the JSON-RPC connection handler,
/// everything else is served as HTTP. Bytes peeked here remain queued for the chosen handler.
pub async fn route(stream: TcpStream, state: SharedState) -> anyhow::Result<()> {
    let mut head = [0u8; 1024];
    let n = stream.peek(&mut head).await.context("peeking request head")?;
    if is_websocket_upgrade(&head[..n]) {
        crate::connection::handle(stream, state).await
    } else {
        serve_http(stream, state).await
    }
}

/// True when the peeked head looks like a WebSocket upgrade. We match the mandatory
/// `Sec-WebSocket-Key` header case-insensitively rather than the path, so the browser can open its
/// socket at `/` exactly like the TUI does.
fn is_websocket_upgrade(head: &[u8]) -> bool {
    find_subslice(&head.to_ascii_lowercase(), b"sec-websocket-key").is_some()
}

async fn serve_http(mut stream: TcpStream, state: SharedState) -> anyhow::Result<()> {
    let request = read_request_head(&mut stream).await?;
    // The request line is `GET /path?query HTTP/1.1`; route on the path, ignoring the query (the
    // client reads `?file=…` itself from the served page).
    let path = request_path(&request).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    let response = if path == "/" || path == "/index.html" {
        let token = state.lock().await.token.clone();
        let body = index_html().replace("__AETHER_TOKEN__", &token);
        http_response("200 OK", "text/html; charset=utf-8", body.as_bytes())
    } else if let Some((bytes, content_type)) = path.strip_prefix('/').and_then(load_asset) {
        http_response("200 OK", content_type, &bytes)
    } else {
        http_response("404 Not Found", "text/plain; charset=utf-8", b"not found")
    };
    stream.write_all(&response).await?;
    stream.flush().await?;
    Ok(())
}

/// The page to serve at `/`: the built client's `index.html` if present, else the spike page.
fn index_html() -> String {
    std::fs::read_to_string(Path::new(WEB_DIST).join("index.html"))
        .unwrap_or_else(|_| SPIKE_PAGE.to_string())
}

/// Load a built asset by its URL-relative path (e.g. `assets/index-AbC123.js`) from `web/dist`.
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

fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
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
