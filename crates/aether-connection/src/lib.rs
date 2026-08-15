//! WebSocket connection actor — the one transport implementation shared by the native shells
//! (TUI and iced). The core stays sans-IO (docs/client-core.md); this crate is the adapter the
//! native shells plug between it and a real socket. It is never part of the wasm build: the web
//! shell bridges the page's own socket instead.
//!
//! The socket lives in a background task that owns request bookkeeping and delivers **every
//! inbound message on one ordered [`Inbound`] stream** — responses correlated to the id
//! [`Handle::send`] returned, notifications interleaved exactly as the server wrote them.
//! Preserving that order end-to-end is load-bearing (docs/client-core.md): a push the server
//! emits *after* a response (an async picker fill, a query's re-push) must be processed after
//! it, and some of those pushes are sent exactly once. Each shell keeps the order intact on its
//! side by consuming the stream from a single place — the TUI's run loop, iced's sequential
//! pump. Requests ENQUEUE SYNCHRONOUSLY, so callers issuing several get them on the wire in
//! call order — the core's `Effect::Request` sequencing contract.
//!
//! [`Handle::rpc`] (await-style, correlated via oneshot) remains for the boot/reconnect dials,
//! which run before anything is pumping the stream; once a shell's loop covers the connection,
//! everything goes through [`Handle::send`] + the stream.
//!
//! The actor is spawned on whatever tokio runtime calls [`connect`]; the `Handle` only awaits
//! channels, so it is runtime-agnostic from there.

use aether_protocol::envelope::{ClientInbound, JsonRpc, Notification, Request, RpcMethod};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub use aether_client::transport::RpcError;

/// Parse a reply's raw JSON into its typed result, folding a deserialization failure into the
/// [`RpcError`] shape (so transport, server, and malformed-payload failures all reach handlers
/// the same way). Used by [`Handle::rpc`] and by the shells' stream-mode continuations.
pub fn parse_reply<T: serde::de::DeserializeOwned>(
    method: &'static str,
    result: Result<serde_json::Value, RpcError>,
) -> Result<T, RpcError> {
    result.and_then(|v| {
        serde_json::from_value(v).map_err(|e| RpcError {
            method,
            code: 0,
            message: format!("malformed result: {e}"),
        })
    })
}

/// One inbound server message, delivered to the shell in wire order.
#[derive(Debug)]
pub enum Inbound {
    /// The reply to a stream-mode request, correlated by the id [`Handle::send`] returned.
    /// `method` is carried for error reporting (the shell parses `result` per continuation).
    Response {
        id: u64,
        method: &'static str,
        result: Result<serde_json::Value, RpcError>,
    },
    Notification(Notification),
}

/// How a request wants its reply delivered.
enum Reply {
    /// Await-style (boot/reconnect dials — nothing is pumping the stream yet).
    Oneshot(oneshot::Sender<Result<serde_json::Value, RpcError>>),
    /// On the ordered [`Inbound`] stream (everything issued from the shell loop).
    Stream,
}

struct Outgoing {
    id: u64,
    method: &'static str,
    params: serde_json::Value,
    reply: Reply,
}

/// Cheap clonable handle for issuing RPCs from anywhere (iced `Task`s included).
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Outgoing>,
    /// Request ids, assigned handle-side so [`Handle::send`] can return the id synchronously —
    /// the caller must be able to register its continuation before the reply can possibly
    /// arrive. Shared across clones; the actor uses the id as given.
    next_id: Arc<AtomicU64>,
}

/// A placeholder transport for the boot `Connecting` state, before any socket exists. Its actor
/// channel has no receiver, so a `call` would error — but the core drops every RPC while not
/// `Connected` (and the shells park input while connecting), so the dummy is never actually
/// exercised; it's swapped for the real handle the moment the boot dial lands. Pairs with
/// [`dummy_inbound`].
pub fn dummy_handle() -> Handle {
    let (tx, _rx) = mpsc::unbounded_channel();
    Handle {
        tx,
        next_id: Arc::new(AtomicU64::new(1)),
    }
}

/// A closed inbound stream for the boot `Connecting` state — `recv` returns `None` at once,
/// but no shell reads it while connecting (the TUI gates its select arm on `Connected`; iced
/// doesn't spawn its pump for it), so the `None` never surfaces.
pub fn dummy_inbound() -> mpsc::UnboundedReceiver<Inbound> {
    let (_tx, rx) = mpsc::unbounded_channel();
    rx
}

impl Handle {
    /// A typed RPC: serialize, call, deserialize. The error keeps its [`RpcError`] shape so
    /// callers can branch on server codes (e.g. `WOULD_OVERWRITE`). Await-style — for the
    /// boot/reconnect dials only; inside the shell loop use [`Self::send`] so the reply keeps
    /// its place in the ordered stream.
    pub async fn rpc<M: RpcMethod>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let params = serde_json::to_value(params).expect("params serialize");
        parse_reply(M::NAME, self.call(M::NAME, params).await)
    }

    /// Fire a typed request whose reply arrives on the [`Inbound`] stream. Returns the request
    /// id for the caller's continuation table. ENQUEUES SYNCHRONOUSLY — see [`Self::send_raw`].
    pub fn send<M: RpcMethod>(&self, params: M::Params) -> u64 {
        let params = serde_json::to_value(params).expect("params serialize");
        self.send_raw(M::NAME, params)
    }

    /// Fire a raw JSON-RPC request whose reply arrives on the [`Inbound`] stream. The request is
    /// ENQUEUED SYNCHRONOUSLY, so callers issuing several get them on the wire in call order —
    /// the core's `Effect::Request` ordering contract relies on it. If the connection is already
    /// gone the send is dropped; the shell's continuation is drained when the stream ends.
    pub fn send_raw(&self, method: &'static str, params: serde_json::Value) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Outgoing {
            id,
            method,
            params,
            reply: Reply::Stream,
        });
        id
    }

    /// Fire a raw await-style call (the [`Self::rpc`] plumbing). The request is ENQUEUED
    /// SYNCHRONOUSLY (before the returned future is polled), like [`Self::send_raw`].
    pub fn call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, RpcError>> + Send + 'static
    {
        let transport_err = move |message: &str| RpcError {
            method,
            code: 0,
            message: message.into(),
        };
        let (reply, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let sent = self
            .tx
            .send(Outgoing {
                id,
                method,
                params,
                reply: Reply::Oneshot(reply),
            })
            .map_err(|_| transport_err("connection closed"));
        async move {
            sent?;
            rx.await.map_err(|_| transport_err("connection closed"))?
        }
    }
}

/// Why a dial failed. A version mismatch (the running daemon is a different build) is terminal —
/// retrying can't fix it, so the shell surfaces it and stops; any other failure means "server not
/// up yet" and is retried on the backoff curve.
#[derive(Debug)]
pub enum ConnectError {
    /// The server rejected the handshake with `426 Upgrade Required` (its version gate). Carries the
    /// message to show: the server's response body if it survived the handshake, else a synthesized
    /// one naming our own version.
    VersionMismatch(String),
    /// Dial failed for any other reason (connection refused, reset, timeout, …).
    Down(anyhow::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::VersionMismatch(m) => f.write_str(m),
            ConnectError::Down(e) => write!(f, "{e}"),
        }
    }
}

/// Connect to the server and spawn the actor on the *current* tokio runtime. Returns the RPC
/// handle and the ordered inbound stream; the receiver yields `None` when the connection dies.
pub async fn connect(
    base_url: &str,
    client_version: &str,
) -> Result<(Handle, mpsc::UnboundedReceiver<Inbound>), ConnectError> {
    use tokio_tungstenite::tungstenite::{http::StatusCode, Error as WsError};
    let url = format!("{base_url}/?version={client_version}");
    let ws = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => ws,
        // The server's version gate rejected the upgrade (426): the daemon holding the port is a
        // different build. Terminal — surface it rather than dialing forever. Prefer the server's
        // own message; fall back to one naming the client version if the body didn't survive.
        Err(WsError::Http(resp)) if resp.status() == StatusCode::UPGRADE_REQUIRED => {
            let detail = resp
                .body()
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .filter(|s| !s.is_empty());
            return Err(ConnectError::VersionMismatch(detail.unwrap_or_else(|| {
                format!("server is a different version than this client ({client_version}) — restart the server")
            })));
        }
        Err(e) => return Err(ConnectError::Down(e.into())),
    };
    // Disable Nagle: with it on, a small request written while an earlier frame is still
    // unACKed sits in the kernel for the peer's delayed-ACK timer (~40ms). The server sets
    // the same on its side; both directions matter for keystroke-paced RPC.
    if let tokio_tungstenite::MaybeTlsStream::Plain(stream) = ws.get_ref() {
        let _ = stream.set_nodelay(true);
    }
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    tokio::spawn(actor(ws, req_rx, inbound_tx));
    Ok((
        Handle {
            tx: req_tx,
            next_id: Arc::new(AtomicU64::new(1)),
        },
        inbound_rx,
    ))
}

/// Deliver one correlated reply the way its request asked for — to its oneshot, or onto the
/// ordered stream. Returns false when the stream side is gone (the app hung up).
fn deliver(
    inbound_tx: &mpsc::UnboundedSender<Inbound>,
    out: Outgoing,
    result: Result<serde_json::Value, RpcError>,
) -> bool {
    match out.reply {
        Reply::Oneshot(tx) => {
            let _ = tx.send(result);
            true
        }
        Reply::Stream => inbound_tx
            .send(Inbound::Response {
                id: out.id,
                method: out.method,
                result,
            })
            .is_ok(),
    }
}

async fn actor(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut req_rx: mpsc::UnboundedReceiver<Outgoing>,
    inbound_tx: mpsc::UnboundedSender<Inbound>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut pending: HashMap<u64, Outgoing> = HashMap::new();
    loop {
        tokio::select! {
            out = req_rx.recv() => {
                let Some(out) = out else { break }; // all Handles dropped — shut down
                let req = Request {
                    jsonrpc: JsonRpc,
                    id: out.id,
                    method: out.method.into(),
                    params: Some(out.params.clone()),
                };
                let text = match serde_json::to_string(&req) {
                    Ok(t) => t,
                    Err(e) => {
                        let err = RpcError {
                            method: out.method,
                            code: 0,
                            message: e.to_string(),
                        };
                        deliver(&inbound_tx, out, Err(err));
                        continue;
                    }
                };
                pending.insert(out.id, out);
                if sink.send(WsMessage::text(text)).await.is_err() {
                    break;
                }
            }
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else { break };
                let WsMessage::Text(text) = frame else { continue };
                let inbound: ClientInbound = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(%e, "unparseable inbound frame");
                        continue;
                    }
                };
                // Everything below rides `inbound_tx` in the order it came off the socket —
                // never reordered relative to each other. That IS the contract; see the
                // module docs.
                let delivered = match inbound {
                    ClientInbound::Response(r) => match pending.remove(&r.id) {
                        Some(out) => deliver(&inbound_tx, out, Ok(r.result)),
                        None => true,
                    },
                    ClientInbound::Error(e) => match pending.remove(&e.id) {
                        Some(out) => {
                            let err = RpcError {
                                method: out.method,
                                code: e.error.code,
                                message: e.error.message,
                            };
                            deliver(&inbound_tx, out, Err(err))
                        }
                        None => true,
                    },
                    ClientInbound::Notification(n) => {
                        inbound_tx.send(Inbound::Notification(n)).is_ok()
                    }
                };
                if !delivered {
                    break;
                }
            }
        }
    }
    // Close the socket gracefully (best-effort) so the server tears the client down promptly
    // rather than waiting on the TCP error path.
    let _ = sink.send(WsMessage::Close(None)).await;
    // Resolve awaiting dials with an error. Stream-mode entries are dropped silently: their
    // continuations live in the shell's table, and the shell fails them when the stream ends —
    // the one place that can tell disconnect fallout from a server-sent error (so it can skip
    // the per-request toast noise a dying connection used to produce). Dropping `inbound_tx`
    // then ends the app's stream, which it reads as "disconnected".
    for (_, out) in pending {
        if let Reply::Oneshot(tx) = out.reply {
            let _ = tx.send(Err(RpcError {
                method: out.method,
                code: 0,
                message: "connection closed".into(),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A `426 Upgrade Required` on the handshake (the server's version gate) must classify as the
    /// terminal `VersionMismatch`, carrying the server's message — not as retryable `Down`. This is
    /// what stops the client dialing a stale daemon forever.
    #[tokio::test]
    async fn version_mismatch_426_is_terminal_with_server_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await; // drain the client's upgrade request
            let body = "version mismatch: server 9.9.9, client 0.0.1 — restart the server";
            let resp = format!(
                "HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        let Err(err) = connect(&format!("ws://{addr}"), "0.0.1").await else {
            panic!("426 must fail the dial");
        };
        match err {
            ConnectError::VersionMismatch(m) => assert!(
                m.contains("restart the server"),
                "message should guide the user to restart, got: {m}"
            ),
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    /// The load-bearing transport contract (docs/client-core.md): responses and notifications
    /// are delivered on ONE stream, in the order they came off the socket. A push the server
    /// emits after a response must be processed after it — some pushes (async picker fills) are
    /// sent exactly once, so reordering them ahead of the response that establishes their
    /// generation silently drops them ("Finding symbols…" forever).
    #[tokio::test]
    async fn inbound_stream_preserves_wire_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            // Wait for the client's request so its id can be echoed.
            let req = loop {
                match ws.next().await.unwrap().unwrap() {
                    WsMessage::Text(t) => {
                        break serde_json::from_str::<serde_json::Value>(&t).unwrap()
                    }
                    _ => continue,
                }
            };
            let id = req["id"].as_u64().unwrap();
            // Notification, then the reply, then another notification — wire order.
            for frame in [
                serde_json::json!({"jsonrpc": "2.0", "method": "test/a", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
                serde_json::json!({"jsonrpc": "2.0", "method": "test/b", "params": {}}),
            ] {
                ws.send(WsMessage::text(frame.to_string())).await.unwrap();
            }
            let _ = ws.next().await; // hold the socket open while the client reads
        });
        let (handle, mut inbound) = connect(&format!("ws://{addr}"), "0.0.1").await.unwrap();
        let sent_id = handle.send_raw("test/echo", serde_json::json!({}));
        let mut got = Vec::new();
        for _ in 0..3 {
            match inbound.recv().await.expect("stream open") {
                Inbound::Notification(n) => got.push(format!("notif:{}", n.method)),
                Inbound::Response { id, result, .. } => {
                    assert_eq!(id, sent_id, "reply correlated to the id `send` returned");
                    assert!(result.is_ok());
                    got.push("response".into());
                }
            }
        }
        assert_eq!(
            got,
            ["notif:test/a", "response", "notif:test/b"],
            "delivery order is wire order — pushes never reorder around responses"
        );
    }

    /// `parse_reply` folds every failure mode into the `RpcError` shape: a server error passes
    /// through untouched (codes intact, for handlers that branch on them), a mismatched payload
    /// becomes a `malformed result` error, and a fitting payload parses.
    #[test]
    fn parse_reply_keeps_the_rpc_error_shape() {
        #[derive(serde::Deserialize, Debug)]
        struct Res {
            ok: bool,
        }
        let parsed: Res = parse_reply("test/m", Ok(serde_json::json!({"ok": true}))).unwrap();
        assert!(parsed.ok);
        let malformed = parse_reply::<Res>("test/m", Ok(serde_json::json!({"nope": 1})))
            .expect_err("shape mismatch must fail");
        assert!(
            malformed.message.contains("malformed result"),
            "{malformed}"
        );
        let server_err = RpcError {
            method: "test/m",
            code: 42,
            message: "no".into(),
        };
        let passed =
            parse_reply::<Res>("test/m", Err(server_err)).expect_err("error passes through");
        assert_eq!(
            passed.code, 42,
            "server codes survive for handlers to branch on"
        );
    }

    /// The disconnect drain contract: when the socket dies, awaiting dials resolve with an
    /// error, but stream-mode replies are NOT synthesized — the stream just ends, and the
    /// shell fails its own continuation table (the one place that can tell disconnect fallout
    /// from a server-sent error, so it can skip the per-request toast noise).
    #[tokio::test]
    async fn disconnect_fails_dials_but_synthesizes_no_stream_replies() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            // Read both requests so they're in the actor's pending map, then hang up.
            for _ in 0..2 {
                while !matches!(ws.next().await, Some(Ok(WsMessage::Text(_)))) {}
            }
            drop(ws);
        });
        let (handle, mut inbound) = connect(&format!("ws://{addr}"), "0.0.1").await.unwrap();
        let dial = handle.call("test/dial", serde_json::json!({}));
        let _stream_id = handle.send_raw("test/stream", serde_json::json!({}));
        let err = dial.await.expect_err("the awaiting dial must resolve");
        assert!(err.message.contains("connection closed"), "{err}");
        // The stream request's reply never appears — the stream ends without it.
        loop {
            match inbound.recv().await {
                None => break,
                Some(Inbound::Response { method, .. }) => {
                    panic!("no synthesized reply expected on disconnect, got one for {method}")
                }
                Some(Inbound::Notification(_)) => {}
            }
        }
    }

    /// A plain dial failure (nothing listening) stays retryable — a version mismatch must be the
    /// *only* thing that gives up, so a briefly-not-yet-up daemon still gets waited out.
    #[tokio::test]
    async fn refused_connection_is_down_not_fatal() {
        // Bind then drop, so the port is known-closed (reliable connection-refused, no privileged port).
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let Err(err) = connect(&format!("ws://{addr}"), "0.0.1").await else {
            panic!("closed port must fail the dial");
        };
        assert!(matches!(err, ConnectError::Down(_)), "got {err:?}");
    }
}
