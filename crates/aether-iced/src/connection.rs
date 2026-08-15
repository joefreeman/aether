//! The shell's transport — the shared native WebSocket actor, re-exported from
//! [`aether_connection`] (one implementation for both native shells; contract and tests live
//! there). The actor runs on its own tokio runtime (created in `main`), independent of iced's
//! `Task` executor — the `Handle` only awaits channels, which are runtime-agnostic. The app's
//! single sequential pump turns the ordered [`Inbound`] stream into `Message`s, so wire order
//! survives into iced's queue (docs/client-core.md).

use tokio::sync::{mpsc, Mutex};

pub use aether_connection::{
    connect, dummy_handle, parse_reply, ConnectError, Handle, Inbound, RpcError,
};

/// The inbound stream's shared receiver — the app's pump locks it per recv (iced `Task`s need a
/// clonable, `'static` handle to it, unlike the TUI loop which owns its receiver directly).
pub type InboundRx = std::sync::Arc<Mutex<mpsc::UnboundedReceiver<Inbound>>>;

/// [`aether_connection::dummy_inbound`] in the pump's shared shape. The pump is *not* spawned
/// for it (the real one starts when the connection lands), so its immediate `None` never
/// reaches the app.
pub fn dummy_inbound() -> InboundRx {
    std::sync::Arc::new(Mutex::new(aether_connection::dummy_inbound()))
}
