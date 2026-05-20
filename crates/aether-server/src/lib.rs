//! Aether editor server.

mod config;
mod connection;
mod cursor;
mod error;
mod handlers;
mod server;
mod state;
mod syntax;
mod wrap;

pub use config::ProjectConfig;
pub use server::{run, run_with_listener, spawn_for_test, ServerHandle};
