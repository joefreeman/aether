//! Aether editor server.

mod backup;
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
mod jumplist;
mod lsp;
mod number;
mod picker;
mod server;
mod sneak;
mod state;
mod status;
mod surround;
mod symbols;
mod syntax;
mod watcher;
mod workspace_index;
mod wrap;

pub use config::{
    active_profile, ensure_profile_port, infer_workspace_for_path, list_profiles,
    profile_idle_timeout_secs, running_server_pid, set_active_profile, ProfileEntry,
    WorkspaceConfig, WorkspaceMatch, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_PROFILE, SERVER_PORT,
};
/// Declared projects (`docs/projects.md`); re-exported for [`spawn_for_test_with_projects`].
pub use config::{ProjectEntry, ProjectRef};
/// Dummy-LSP test fixture types (see [`spawn_for_test_with_lsp`]); re-exported for integration tests.
pub use lsp::dummy::{
    DiagnosticsTrigger, DummyDiagnostic, DummyDocSymbol, DummyLspConfig, DummyRange, DummySymbol,
    DummyTextEdit,
};
pub use server::{
    run, run_with_listener, spawn_for_test, spawn_for_test_full, spawn_for_test_multi,
    spawn_for_test_multi_with_persistence, spawn_for_test_multi_with_sessions,
    spawn_for_test_with_lsp, spawn_for_test_with_projects, ServerHandle,
};
pub use status::{app_info, fetch_status};

/// **Test view.** One language server's lifecycle state, flattened for integration assertions.
///
/// Pinning isn't observable over the wire — the LSP picker shows a server's *status*, not whether
/// it's held up by an open buffer or by a declared project — so tests read it here rather than
/// reaching into private manager internals.
#[derive(Debug)]
pub struct LspServerView {
    pub language: String,
    pub ready: bool,
    pub pinned: bool,
    pub open_buffers: usize,
}

/// Every language server the state currently holds, in unspecified order (tests assert on the set,
/// which is at most a couple of entries).
pub type LspManagerView = Vec<LspServerView>;

/// Snapshot [`LspManagerView`] from a locked server state.
pub fn lsp_view(s: &state::ServerState) -> LspManagerView {
    s.lsp
        .servers
        .values()
        .map(|h| LspServerView {
            language: h.language.clone(),
            ready: matches!(h.status, aether_protocol::lsp::LspStatus::Ready),
            pinned: h.pinned,
            open_buffers: h.open_buffers.len(),
        })
        .collect()
}
