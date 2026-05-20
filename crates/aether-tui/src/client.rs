//! WebSocket client wrapper with typed JSON-RPC.

use aether_protocol::envelope::{
    ClientInbound, JsonRpc, Notification, Request, RpcMethod,
};
use anyhow::{anyhow, bail, Context};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Client {
    ws: Ws,
    next_id: u64,
    /// Notifications received while awaiting a response. Drained by the app between RPC calls.
    pending_notifications: VecDeque<Notification>,
}

impl Client {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to {url}"))?;
        Ok(Self { ws, next_id: 1, pending_notifications: VecDeque::new() })
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
                    bail!("RPC {} returned error {}: {}", M::NAME, e.error.code, e.error.message);
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
            let Some(frame) = self.ws.next().await else { return Ok(None) };
            let frame = frame?;
            let Message::Text(text) = frame else { continue };
            let inbound: ClientInbound = serde_json::from_str(&text)
                .with_context(|| format!("parsing inbound frame: {text}"))?;
            return Ok(Some(inbound));
        }
    }
}
