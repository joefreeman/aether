//! LSP integration: aether-server acting as an LSP *client*.
//!
//! Staged per `docs/lsp.md`. Phase 0 lands the position-encoding conversion ([`position`]) ahead of
//! the transport, since it's pure and fully testable on its own. The subprocess transport, request
//! router, document sync, and lifecycle land in Phase 1, at which point they consume what's here —
//! hence the module-wide `allow(dead_code)` until then.
#![allow(dead_code)]

pub mod client;
pub mod config;
pub mod diagnostics;
pub mod lifecycle;
pub mod manager;
pub mod position;
pub mod process;
pub mod transport;
pub mod uri;
