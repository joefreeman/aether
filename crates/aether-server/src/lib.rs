//! Aether editor server.

mod brackets;
mod config;
mod connection;
mod cursor;
mod error;
mod grep;
mod handlers;
mod indent;
mod picker;
mod server;
mod state;
mod surround;
mod syntax;
mod watcher;
mod workspace_index;
mod wrap;

pub use config::{ProjectConfig, SERVER_PORT};
pub use server::{run, run_with_listener, spawn_for_test, ServerHandle};
