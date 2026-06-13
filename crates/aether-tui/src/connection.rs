//! WebSocket connection actor — a deliberate sibling of the iced shell's `connection.rs`
//! (duplicated, not shared: the native shells happen to want the same actor today, but
//! each is free to drift).
//!
//! The socket lives in a background task that correlates responses to pending requests by
//! id and forwards notifications on a channel; the `Handle` only awaits channels. `call`
//! ENQUEUES SYNCHRONOUSLY, so callers issuing several requests get them on the wire in
//! call order — the ordering contract the core's `Effect::Request` relies on.

use aether_protocol::envelope::{ClientInbound, JsonRpc, Notification, Request, RpcMethod};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// (The iced copy defines a shared-receiver alias `NotifRx` here for its pump pattern; the
// TUI's `Client` owns its receiver directly, so there's no alias in this copy.)

pub use aether_client::transport::RpcError;

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
    let url = format!("{base_url}/?client_version={client_version}");
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
