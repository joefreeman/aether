//! Aether protocol types.
//!
//! Wire format: JSON-RPC 2.0 over WebSocket. See `docs/protocol.md` for the full schema.

use serde::{Deserialize, Serialize};

pub mod buffer;
pub mod cursor;
pub mod directory;
pub mod envelope;
pub mod error;
pub mod git;
pub mod input;
pub mod lsp;
pub mod nav;
pub mod path;
pub mod picker;
pub mod project;
pub mod search;
pub mod viewport;

pub type BufferId = u64;
pub type ViewportId = u64;
pub type Revision = u64;
pub type ClientId = uuid::Uuid;

/// Fixed loopback port the server binds and every client connects to. Single-instance: the bind
/// is the real mutex (only one process can hold the port), so clients hard-code the address
/// rather than discovering it — which also lets a client launch *before* the server and wait for
/// it to come up. The server identifies its instance (for restart detection) over the wire on
/// `project/activate`, not via a discovery file.
pub const SERVER_PORT: u16 = 2384;

/// The default loopback WebSocket URL clients connect to ([`SERVER_PORT`]). The connection layer
/// appends its own `?client_version=` query string.
pub fn default_server_url() -> String {
    format!("ws://127.0.0.1:{SERVER_PORT}")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalPosition {
    pub line: u32,
    pub col: u32,
}

/// Serde helpers for counted params (`count` defaults to 1 and stays off the wire at 1).
pub(crate) fn count_one() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn count_is_one(n: &u32) -> bool {
    *n == 1
}
