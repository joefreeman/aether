//! The core's view of the server connection: a JSON-RPC call surface, nothing more. The
//! shell provides the implementation (natively the WebSocket actor's `Handle`; a browser
//! shell would bridge `web-sys` sockets) — the concrete socket can never live in core
//! (docs/client-core.md: core must compile for every conceivable shell).

use aether_protocol::envelope::RpcMethod;
use futures_util::future::BoxFuture;

/// JSON-RPC error from the server (or a transport failure surfaced in its shape).
#[derive(Debug, Clone, thiserror::Error)]
#[error("RPC {method} returned error {code}: {message}")]
pub struct RpcError {
    pub method: &'static str,
    pub code: i32,
    pub message: String,
}

/// A shareable transport — what core update methods receive. `Arc` because sequenced RPC
/// chains (set cursor *then* insert) must lazily issue the later calls from inside their
/// future: implementations enqueue at `call` time, so pre-building both would race them.
pub type SharedTransport = std::sync::Arc<dyn Transport + Send + Sync>;

/// What the core needs from a connection: fire a request, get the raw result back. The
/// returned future is `'static` — the implementation captures its own clone of whatever it
/// needs, so core code never holds the transport across an await.
pub trait Transport {
    fn call(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> BoxFuture<'static, Result<serde_json::Value, RpcError>>;
}

/// A typed RPC over a [`Transport`]: serialize, call, deserialize. The error keeps its
/// [`RpcError`] shape so callers can branch on server codes (e.g. `WOULD_OVERWRITE`).
pub fn rpc<M: RpcMethod>(
    t: &dyn Transport,
    params: M::Params,
) -> BoxFuture<'static, Result<M::Result, RpcError>> {
    let transport_err = |message: String| RpcError {
        method: M::NAME,
        code: 0,
        message,
    };
    let fut = match serde_json::to_value(&params) {
        Ok(v) => t.call(M::NAME, v),
        Err(e) => {
            let err = transport_err(e.to_string());
            return Box::pin(async move { Err(err) });
        }
    };
    Box::pin(async move {
        let value = fut.await?;
        serde_json::from_value(value).map_err(|e| transport_err(format!("parsing result: {e}")))
    })
}
