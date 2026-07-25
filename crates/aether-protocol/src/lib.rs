//! Aether protocol types.
//!
//! Wire format: JSON-RPC 2.0 over WebSocket. See `docs/protocol.md` for the full schema.

use serde::{Deserialize, Serialize};

pub mod app;
pub mod buffer;
pub mod cursor;
pub mod directory;
pub mod envelope;
pub mod error;
pub mod git;
pub mod hints;
pub mod input;
pub mod jumplist;
pub mod lsp;
pub mod nav;
pub mod path;
pub mod picker;
pub mod search;
pub mod settings;
pub mod sneak;
pub mod viewport;
pub mod workspace;

pub type BufferId = u64;
pub type ViewportId = u64;
pub type Revision = u64;
pub type ClientId = uuid::Uuid;

/// Fixed loopback port the server binds and every client connects to. Single-instance: the bind
/// is the real mutex (only one process can hold the port), so clients hard-code the address
/// rather than discovering it — which also lets a client launch *before* the server and wait for
/// it to come up. The server identifies its instance (for restart detection) over the wire on
/// `workspace/activate`, not via a discovery file.
pub const SERVER_PORT: u16 = 2384;

/// The default loopback WebSocket URL clients connect to ([`SERVER_PORT`]). The connection layer
/// appends its own `?version=` query string.
pub fn default_server_url() -> String {
    format!("ws://127.0.0.1:{SERVER_PORT}")
}

/// Prefix marking a workspace id as *ephemeral* (a "no-workspace" context synthesized to host files
/// opened outside any configured workspace). A workspace is addressed on the wire by id (the `name`
/// field of [`workspace::WorkspaceInfo`] / [`workspace::WorkspaceActivateParams`]); for a persisted workspace
/// that id is its human name, for an ephemeral one it's a reserved token of the form
/// `ephemeral/<n>`. The `/` can't appear in a real workspace name (the server rejects separators), so
/// the two namespaces never collide. Clients use [`is_ephemeral_workspace_id`] to render such a
/// context as "(no workspace)" rather than showing the raw token.
pub const EPHEMERAL_WORKSPACE_PREFIX: &str = "ephemeral/";

/// Whether a workspace id denotes an ephemeral ("(no workspace)") context. See
/// [`EPHEMERAL_WORKSPACE_PREFIX`].
pub fn is_ephemeral_workspace_id(id: &str) -> bool {
    id.starts_with(EPHEMERAL_WORKSPACE_PREFIX)
}

/// The build version a client announces on connect (`?version=`); the server requires an
/// exact match against its own copy of this string. Server and all clients ship in one binary, so
/// "same release" always means identical versions — any difference means a freshly-installed binary
/// is talking to a stale daemon still holding [`SERVER_PORT`], whose wire format may have drifted.
/// Sourced from the workspace version (every crate sets `version.workspace = true`), so this and the
/// server's/native clients' `CARGO_PKG_VERSION` are guaranteed equal within a build.
pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git SHA this binary was built from, or `None` when it wasn't built from a checkout (a
/// source tarball, a machine without `git`). Stamped by `build.rs`.
///
/// [`PROTOCOL_VERSION`] identifies a *release*; this identifies a *build*. Between releases the
/// version is constant while the code moves daily, so the SHA is what actually answers "which
/// binary is this?" — the question `app/info` exists to settle.
pub const BUILD_COMMIT: Option<&str> = {
    let c = env!("AETHER_COMMIT");
    if c.is_empty() {
        None
    } else {
        Some(c)
    }
};

/// Whether the working tree had uncommitted changes when this binary was built — i.e. whether
/// [`BUILD_COMMIT`] fully describes it. Always `false` when the commit is unknown.
pub const BUILD_DIRTY: bool = matches!(env!("AETHER_COMMIT_DIRTY").as_bytes(), [b'1']);

/// Whether this is a debug build (`debug_assertions` on). Worth reporting next to the commit: a
/// debug build of the same commit performs differently enough to explain a "why is this slow?".
pub const BUILD_DEBUG: bool = cfg!(debug_assertions);

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
