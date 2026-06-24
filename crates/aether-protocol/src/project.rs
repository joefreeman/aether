//! Project selection. The server hosts many projects; each client has a single active project
//! at a time. The client picks one with `project/activate` (also used to switch). `project/list`
//! enumerates the projects the server has configured on disk.

use crate::buffer::BufferOpenResult;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// Enumerate configured projects (the `*.toml` files under `$XDG_CONFIG_HOME/aether/projects/`).
/// Does not indicate which one — if any — the calling client has active; the client tracks that
/// locally.
pub struct ProjectList;
impl RpcMethod for ProjectList {
    const NAME: &'static str = "project/list";
    type Params = ProjectListParams;
    type Result = ProjectListResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProjectListParams {}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectListResult {
    pub projects: Vec<ProjectSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
}

/// Activate a project for this client. Used both for the initial selection (the client has just
/// connected and has no active project) and for switching (already active, picking a different
/// one). Switching tears down the client's per-buffer state for the previously-active project.
/// The buffers themselves stay in the server, available to other clients.
pub struct ProjectActivate;
impl RpcMethod for ProjectActivate {
    const NAME: &'static str = "project/activate";
    type Params = ProjectActivateParams;
    type Result = ProjectActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectActivateParams {
    pub name: String,
    /// Also open the landing buffer — the project's `last_buffer_id` when there is one, a
    /// fresh *transient* scratch otherwise — and return it in `opened`. The bootstrap
    /// convention (activate, then land somewhere) folded into one round-trip
    /// (docs/protocol-composites.md, C).
    #[serde(default)]
    pub open_last: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectActivateResult {
    pub project: ProjectInfo,
    /// The most-recently-used buffer in this project for the calling client, if any. Populated
    /// from the server's per-client MRU. `None` means the client has no history in this project
    /// (first visit, or every prior buffer has been closed). The client should attach to this
    /// buffer rather than spawn a fresh scratch, so switching back to a project lands you on the
    /// buffer you last had open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_buffer_id: Option<BufferId>,
    /// With `open_last`: the landing buffer, fully opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<BufferOpenResult>,
    /// The server instance's start time (unix ms) — its identity for restart detection. A client
    /// caches it on activation and compares across reconnects: a changed value means the daemon
    /// restarted (so unsaved buffer state died with it), distinct from a connection that merely
    /// blipped. Carried on the wire rather than read from a discovery file, so it's authoritative
    /// for the instance you're actually talking to.
    #[serde(default)]
    pub server_started_at: u64,
}

/// Describes the active project: its name and absolute root paths. Returned by
/// `project/activate`. Paths are server-canonicalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub paths: Vec<String>,
}

/// Create a fresh project with no roots. The client uses the project picker's "create new" row
/// to invoke this; the server writes an empty-`paths` TOML to
/// `$XDG_CONFIG_HOME/aether/projects/<name>.toml`, registers the project in memory, and
/// activates it for the calling client. The follow-up is normally `project/add_root` (via the
/// project settings overlay, which the TUI auto-opens after create).
///
/// Refuses if a project of that name already exists on disk. Name must be non-empty and contain
/// no path separators.
pub struct ProjectCreate;
impl RpcMethod for ProjectCreate {
    const NAME: &'static str = "project/create";
    type Params = ProjectCreateParams;
    type Result = ProjectActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectCreateParams {
    pub name: String,
}

/// Open a file by absolute (or cwd-relative) path, resolving the project context for it. This is
/// the project-agnostic entry point used by `ae /path/to/file`, the `Space Alt-w` open-from-path
/// overlay, and goto-definition into a file outside the active project. The server:
///
/// - canonicalizes the path;
/// - if the calling client has an active project whose roots **contain** the path, opens it there
///   as an ordinary (internal) buffer;
/// - if a project is active but the path is **outside** its roots, opens it there as an *external*
///   buffer (the project hosts it as a guest — no git, trust-restricted LSP) — it is **not**
///   re-homed into whichever other configured project might contain it;
/// - if **no** project is active, synthesizes a fresh *ephemeral* project (no name, no roots, auto-
///   removed when its last buffer closes), activates it, and opens the file there.
///
/// Returns the (possibly newly-activated) project alongside the opened buffer, so the client adopts
/// the project id exactly as it does after `project/activate`.
pub struct ProjectOpenPath;
impl RpcMethod for ProjectOpenPath {
    const NAME: &'static str = "project/open_path";
    type Params = ProjectOpenPathParams;
    type Result = ProjectActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectOpenPathParams {
    /// File to open. Absolute, or relative to the server's current working directory. `~/` is
    /// expanded server-side. Must exist on disk (open-from-path is for existing files).
    pub path: String,
    /// Open the buffer as transient (auto-closes once hidden) — used when the open is a preview.
    /// Defaults to a permanent open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
}

/// Add a root path to an existing project. Server canonicalizes the path, refuses duplicates,
/// updates the TOML, watches the new path for external changes, and invalidates the project's
/// workspace index (so the next picker open re-walks).
pub struct ProjectAddRoot;
impl RpcMethod for ProjectAddRoot {
    const NAME: &'static str = "project/add_root";
    type Params = ProjectAddRootParams;
    type Result = ProjectInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectAddRootParams {
    /// Project to modify. Doesn't have to be the caller's active project (the TUI only uses it
    /// for the active project today, but the protocol stays general).
    pub project: String,
    /// Path on disk. Must exist and be canonicalizable. Leading `~/` is expanded server-side.
    pub path: String,
}

/// Remove a root path from a project. The server closes any file-backed buffers under this root
/// that aren't covered by another remaining root, and refuses the whole operation if any such
/// buffer is dirty (with error code `DIRTY_BUFFERS_PREVENT_REMOVE`). Scratch buffers in the
/// project are unaffected (they have no path and aren't tied to any root).
///
/// The `next_buffer_id` field follows the same convention as `buffer/close`: when the client's
/// currently-displayed buffer is one of the closed ones, attach to this next id (or spawn a
/// scratch if `None`).
pub struct ProjectRemoveRoot;
impl RpcMethod for ProjectRemoveRoot {
    const NAME: &'static str = "project/remove_root";
    type Params = ProjectRemoveRootParams;
    type Result = ProjectRemoveRootResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRemoveRootParams {
    pub project: String,
    /// The root to remove. Server matches against the project's stored canonical paths after
    /// canonicalizing this value too — so callers can pass either the canonical form (what they
    /// got back from `ProjectInfo.paths`) or a user-typed equivalent.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRemoveRootResult {
    pub project: ProjectInfo,
    /// File-backed buffers that were closed as part of this remove. Scratch buffers and buffers
    /// still covered by other roots are not in this list.
    #[serde(default)]
    pub closed_buffer_ids: Vec<crate::BufferId>,
    /// Buffer for the requesting client to attach to if its current buffer was one of the
    /// closed ones. `None` means "no buffers left for you in this project — spawn a scratch."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_buffer_id: Option<crate::BufferId>,
}

/// Rename a project. Moves the on-disk config (`<old>.toml` → `<new>.toml`) and re-keys the
/// project's in-memory state: the server's project map, every open buffer's project association,
/// and every client's active-project pointer. Open buffers are untouched — they keep their ids
/// and paths — so a rename is safe even with dirty buffers in the project, and nothing is closed
/// or reloaded.
///
/// Refuses if a project named `new_name` already exists, or if `new_name` is empty / contains
/// path separators (same constraints as `project/create`). Renaming a project to its current
/// name is a no-op that returns the current info.
pub struct ProjectRename;
impl RpcMethod for ProjectRename {
    const NAME: &'static str = "project/rename";
    type Params = ProjectRenameParams;
    type Result = ProjectInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRenameParams {
    /// The project to rename (its current name).
    pub project: String,
    /// The desired new name.
    pub new_name: String,
}

/// Delete a project: remove its on-disk config (`<name>.toml`) and drop its in-memory state,
/// closing any buffers that belonged to it. This forgets the project *definition* — it does NOT
/// touch the source files under the project's roots.
///
/// Refuses if the project is any connected client's active project (`ACTIVE_PROJECT_PREVENTS_-
/// DELETE`) — the caller must switch away first — or if any buffer in the project has unsaved
/// changes (`DIRTY_BUFFERS_PREVENT_DELETE`). Invoked from the project switcher; you can't delete
/// a project you're not looking at the list of, and you can't delete the one you're in.
pub struct ProjectDelete;
impl RpcMethod for ProjectDelete {
    const NAME: &'static str = "project/delete";
    type Params = ProjectDeleteParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectDeleteParams {
    pub name: String,
}

// ---- project/renamed (notification) -------------------------------------------------------------

/// Pushed to a client when its active project is renamed by *another* client. The server has
/// already re-keyed the receiver's server-side state (active project, buffers) to the new name; this
/// tells the client so it can update its local name — which drives both the display and the
/// reconnect baseline (reconnect is by name). Only sent to clients whose active project was renamed;
/// the renaming client learns the new name from its `project/rename` RPC result instead.
pub struct ProjectRenamed;
impl NotificationMethod for ProjectRenamed {
    const NAME: &'static str = "project/renamed";
    type Params = ProjectRenamedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectRenamedParams {
    pub old_name: String,
    pub new_name: String,
}
