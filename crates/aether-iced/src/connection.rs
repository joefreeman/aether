//! WebSocket connection actor.
//!
//! Unlike the TUI's sequential `Client` (one in-flight RPC, drained notifications), the iced
//! client is message-driven: `update` fires RPCs as `Task`s and their responses come back as
//! messages, with notifications interleaved. So the socket lives in a background actor that
//! correlates responses to pending requests by id and forwards notifications on a channel. The
//! actor runs on its own tokio runtime (created in `main`), which keeps it independent of
//! whatever executor iced uses for `Task`s — the `Handle` only awaits channels, which are
//! runtime-agnostic.

use aether_protocol::envelope::{ClientInbound, JsonRpc, Notification, Request, RpcMethod};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// The notification stream's shared receiver — the shell's pump locks it per recv.
pub type NotifRx = std::sync::Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Notification>>>;

pub use crate::core::transport::RpcError;

struct Outgoing {
    method: &'static str,
    params: serde_json::Value,
    reply: oneshot::Sender<Result<serde_json::Value, RpcError>>,
}

/// Cheap clonable handle for issuing RPCs from anywhere (iced `Task`s included).
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Outgoing>,
}

/// A placeholder transport for the boot-connecting state, before any socket exists. Its actor
/// channel has no receiver, so any `call` errors immediately — but the app parks all input while
/// `ConnState::Connecting`, so a dummy handle is never actually exercised; it's swapped for the
/// real one the moment the dial lands. Pairs with [`dummy_notifications`].
pub fn dummy_handle() -> Handle {
    let (tx, _rx) = mpsc::unbounded_channel();
    Handle { tx }
}

/// A closed notification stream for the boot-connecting state — `recv` returns `None` at once.
/// The pump is *not* spawned for it (the real one starts when the connection lands), so its
/// `None` never reaches the app.
pub fn dummy_notifications() -> NotifRx {
    let (_tx, rx) = mpsc::unbounded_channel();
    std::sync::Arc::new(tokio::sync::Mutex::new(rx))
}

impl Handle {
    /// A typed RPC: serialize, call, deserialize. The error keeps its [`RpcError`] shape so
    /// callers can branch on server codes (e.g. `WOULD_OVERWRITE`).
    pub async fn rpc<M: RpcMethod>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let params = serde_json::to_value(params).expect("params serialize");
        let v = self.call(M::NAME, params).await?;
        serde_json::from_value(v).map_err(|e| RpcError {
            method: M::NAME,
            code: 0,
            message: format!("malformed result: {e}"),
        })
    }
}

/// The native transport: requests ride the actor's channel; the future awaits the
/// correlated reply (and is `'static` — it owns its `oneshot` end, not the handle).
impl Handle {
    /// Fire a raw JSON-RPC call. The request is ENQUEUED SYNCHRONOUSLY (before the returned
    /// future is polled) — callers performing several calls in sequence get them on the
    /// wire in call order, which the core's `Effect::Request` ordering contract relies on.
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
        let sent = self
            .tx
            .send(Outgoing {
                method,
                params,
                reply,
            })
            .map_err(|_| transport_err("connection closed"));
        async move {
            sent?;
            rx.await.map_err(|_| transport_err("connection closed"))?
        }
    }
}

/// Connect to the server and spawn the actor on the *current* tokio runtime. Returns the RPC
/// handle and the notification stream; the receiver yields `None` when the connection dies.
pub async fn connect(
    base_url: &str,
    client_version: &str,
) -> anyhow::Result<(Handle, mpsc::UnboundedReceiver<Notification>)> {
    let url = format!("{base_url}/?version={client_version}");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (notif_tx, notif_rx) = mpsc::unbounded_channel();
    tokio::spawn(actor(ws, req_rx, notif_tx));
    Ok((Handle { tx: req_tx }, notif_rx))
}

async fn actor(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut req_rx: mpsc::UnboundedReceiver<Outgoing>,
    notif_tx: mpsc::UnboundedSender<Notification>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut pending: HashMap<u64, Outgoing> = HashMap::new();
    let mut next_id: u64 = 1;
    loop {
        tokio::select! {
            out = req_rx.recv() => {
                let Some(out) = out else { break }; // all Handles dropped — shut down
                let id = next_id;
                next_id += 1;
                let req = Request {
                    jsonrpc: JsonRpc,
                    id,
                    method: out.method.into(),
                    params: Some(out.params.clone()),
                };
                let text = match serde_json::to_string(&req) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = out.reply.send(Err(RpcError {
                            method: out.method,
                            code: 0,
                            message: e.to_string(),
                        }));
                        continue;
                    }
                };
                pending.insert(id, out);
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
                match inbound {
                    ClientInbound::Response(r) => {
                        if let Some(out) = pending.remove(&r.id) {
                            let _ = out.reply.send(Ok(r.result));
                        }
                    }
                    ClientInbound::Error(e) => {
                        if let Some(out) = pending.remove(&e.id) {
                            let _ = out.reply.send(Err(RpcError {
                                method: out.method,
                                code: e.error.code,
                                message: e.error.message,
                            }));
                        }
                    }
                    ClientInbound::Notification(n) => {
                        if notif_tx.send(n).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    // Close the socket gracefully (best-effort) so the server tears the client down promptly
    // rather than waiting on the TCP error path.
    let _ = sink.send(WsMessage::Close(None)).await;
    // Fail every in-flight RPC so awaiting Tasks resolve; dropping notif_tx ends the app's
    // notification stream, which it reads as "disconnected".
    for (_, out) in pending {
        let _ = out.reply.send(Err(RpcError {
            method: out.method,
            code: 0,
            message: "connection closed".into(),
        }));
    }
}
