//! The shell's transport — the shared native WebSocket actor, re-exported from
//! [`aether_connection`] (one implementation for both native shells; contract and tests live
//! there). This shell consumes the ordered [`Inbound`] stream from a single place — the run
//! loop's select arm — which is what carries the wire-order delivery contract
//! (docs/client-core.md) through to processing.

pub use aether_connection::{
    connect, dummy_handle, dummy_inbound, parse_reply, ConnectError, Handle, Inbound, RpcError,
};
