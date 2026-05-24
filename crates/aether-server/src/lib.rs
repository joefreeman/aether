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
mod syntax;
mod workspace_index;
mod wrap;

pub use config::ProjectConfig;
pub use server::{run, run_with_listener, spawn_for_test, ServerHandle};
