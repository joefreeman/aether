//! The core's RPC error type. The core is sans-IO: it never talks to a connection — it
//! emits `Effect::Request` and receives outcomes through `Session::on_rpc_result`
//! (docs/client-core.md). The shell owns the actual transport (natively the WebSocket
//! actor; a browser shell bridges the page's socket) and reports failures in this shape.

/// JSON-RPC error from the server (or a transport failure surfaced in its shape).
#[derive(Debug, Clone, thiserror::Error)]
#[error("RPC {method} returned error {code}: {message}")]
pub struct RpcError {
    pub method: &'static str,
    pub code: i32,
    pub message: String,
}
