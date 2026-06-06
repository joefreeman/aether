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
pub mod path;
pub mod picker;
pub mod project;
pub mod search;
pub mod viewport;

pub type BufferId = u64;
pub type ViewportId = u64;
pub type Revision = u64;
pub type ClientId = uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalPosition {
    pub line: u32,
    pub col: u32,
}
