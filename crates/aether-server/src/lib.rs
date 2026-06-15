//! Aether editor server.

mod brackets;
mod config;
mod connection;
mod cursor;
mod error;
mod git;
mod grep;
mod handlers;
mod http;
mod indent;
mod lsp;
mod picker;
mod server;
mod state;
mod surround;
mod syntax;
mod watcher;
mod workspace_index;
mod wrap;

pub use config::{infer_project_for_path, ProjectConfig, ProjectMatch, SERVER_PORT};
pub use server::{run, run_with_listener, spawn_for_test, spawn_for_test_multi, ServerHandle};
