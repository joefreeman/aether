//! Aether editor server.

mod brackets;
mod case;
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
mod number;
mod picker;
mod server;
mod sneak;
mod state;
mod surround;
mod syntax;
mod watcher;
mod workspace_index;
mod wrap;

pub use config::{
    active_profile, ensure_profile_port, infer_project_for_path, list_profiles,
    profile_idle_timeout_secs, running_server_pid, set_active_profile, ProfileEntry, ProjectConfig,
    ProjectMatch, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_PROFILE, SERVER_PORT,
};
pub use server::{
    run, run_with_listener, spawn_for_test, spawn_for_test_multi,
    spawn_for_test_multi_with_sessions, ServerHandle,
};
