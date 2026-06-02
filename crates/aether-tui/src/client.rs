//! WebSocket client wrapper with typed JSON-RPC.

use aether_protocol::envelope::{ClientInbound, JsonRpc, Notification, Request, RpcMethod};
use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::fmt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// JSON-RPC error from the server, surfaced as an `anyhow` source so the rest of the app's
/// `anyhow::Result` plumbing keeps working while callers that care about the code can
/// `downcast_ref::<RpcError>()` to branch on it (e.g. `WOULD_OVERWRITE` for save-as).
#[derive(Debug, Clone)]
pub struct RpcError {
    pub method: &'static str,
    pub code: i32,
    pub message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RPC {} returned error {}: {}",
            self.method, self.code, self.message
        )
    }
}

impl std::error::Error for RpcError {}

pub struct Client {
    ws: Ws,
    next_id: u64,
    /// Notifications received while awaiting a response. Drained by the app between RPC calls.
    pending_notifications: VecDeque<Notification>,
}

impl Client {
    /// Connect to the server with credentials in the query string. The server checks the token
    /// during the WebSocket upgrade and rejects the connection if it's wrong — no JSON-RPC
    /// handshake.
    pub async fn connect(
        base_url: &str,
        token: &str,
        client_version: &str,
    ) -> anyhow::Result<Self> {
        let url = format!("{base_url}/?token={token}&client_version={client_version}");
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .with_context(|| format!("connecting to {base_url}"))?;
        Ok(Self {
            ws,
            next_id: 1,
            pending_notifications: VecDeque::new(),
        })
    }

    pub async fn rpc<M: RpcMethod>(&mut self, params: M::Params) -> anyhow::Result<M::Result> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            jsonrpc: JsonRpc,
            id,
            method: M::NAME.into(),
            params: Some(serde_json::to_value(&params)?),
        };
        let text = serde_json::to_string(&req)?;
        self.ws.send(Message::text(text)).await?;

        loop {
            let frame = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow!("WebSocket closed while awaiting response"))?;
            let frame = frame?;
            let Message::Text(text) = frame else {
                continue;
            };
            let inbound: ClientInbound = serde_json::from_str(&text)
                .with_context(|| format!("parsing inbound frame: {text}"))?;
            match inbound {
                ClientInbound::Response(r) if r.id == id => {
                    return Ok(serde_json::from_value(r.result)?);
                }
                ClientInbound::Error(e) if e.id == id => {
                    return Err(anyhow::Error::new(RpcError {
                        method: M::NAME,
                        code: e.error.code,
                        message: e.error.message,
                    }));
                }
                ClientInbound::Notification(n) => self.pending_notifications.push_back(n),
                ClientInbound::Response(_) | ClientInbound::Error(_) => {
                    // Stray response for a different id; ignore.
                }
            }
        }
    }

    pub fn drain_notifications(&mut self) -> Vec<Notification> {
        self.pending_notifications.drain(..).collect()
    }

    /// Await the next incoming notification or response — used by the app's main `select!`
    /// loop while no RPC is in flight. Returns `Ok(None)` when the connection closes.
    pub async fn recv(&mut self) -> anyhow::Result<Option<ClientInbound>> {
        // Drain pending first so a previously-buffered notification is delivered before we read.
        if let Some(n) = self.pending_notifications.pop_front() {
            return Ok(Some(ClientInbound::Notification(n)));
        }
        loop {
            let Some(frame) = self.ws.next().await else {
                return Ok(None);
            };
            let frame = frame?;
            let Message::Text(text) = frame else { continue };
            let inbound: ClientInbound = serde_json::from_str(&text)
                .with_context(|| format!("parsing inbound frame: {text}"))?;
            return Ok(Some(inbound));
        }
    }
}
