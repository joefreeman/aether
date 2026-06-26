//! Workspace selection. The server hosts many workspaces; each client has a single active workspace
//! at a time. The client picks one with `workspace/activate` (also used to switch). `workspace/list`
//! enumerates the workspaces the server has configured on disk.

use crate::buffer::BufferOpenResult;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// Enumerate configured workspaces (the `*.toml` files under `$XDG_CONFIG_HOME/aether/workspaces/`).
/// Does not indicate which one — if any — the calling client has active; the client tracks that
/// locally. Issued directly by the web client's bootstrap chooser (the native shells reach the
/// workspace list through the workspaces picker instead).
pub struct WorkspaceList;
impl RpcMethod for WorkspaceList {
    const NAME: &'static str = "workspace/list";
    type Params = WorkspaceListParams;
    type Result = WorkspaceListResult;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceListParams {}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub name: String,
}

/// Activate a workspace for this client. Used both for the initial selection (the client has just
/// connected and has no active workspace) and for switching (already active, picking a different
/// one). Switching tears down the client's per-buffer state for the previously-active workspace.
/// The buffers themselves stay in the server, available to other clients.
pub struct WorkspaceActivate;
impl RpcMethod for WorkspaceActivate {
    const NAME: &'static str = "workspace/activate";
    type Params = WorkspaceActivateParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceActivateParams {
    pub name: String,
    /// Also open the landing buffer — the workspace's `last_buffer_id` when there is one, a
    /// fresh *transient* scratch otherwise — and return it in `opened`. The bootstrap
    /// convention (activate, then land somewhere) folded into one round-trip
    /// (docs/protocol-composites.md, C).
    #[serde(default)]
    pub open_last: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceActivateResult {
    pub workspace: WorkspaceInfo,
    /// The most-recently-used buffer in this workspace for the calling client, if any. Populated
    /// from the server's per-client MRU. `None` means the client has no history in this workspace
    /// (first visit, or every prior buffer has been closed). The client should attach to this
    /// buffer rather than spawn a fresh scratch, so switching back to a workspace lands you on the
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

/// Describes the active workspace: its name and absolute root paths. Returned by
/// `workspace/activate`. Paths are server-canonicalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub paths: Vec<String>,
}

/// Create a fresh workspace with no roots. The client uses the workspace picker's "create new" row
/// to invoke this; the server writes an empty-`paths` TOML to
/// `$XDG_CONFIG_HOME/aether/workspaces/<name>.toml`, registers the workspace in memory, and
/// activates it for the calling client. The follow-up is normally `workspace/add_root` (via the
/// workspace settings overlay, which the TUI auto-opens after create).
///
/// Refuses if a workspace of that name already exists on disk. Name must be non-empty and contain
/// no path separators.
pub struct WorkspaceCreate;
impl RpcMethod for WorkspaceCreate {
    const NAME: &'static str = "workspace/create";
    type Params = WorkspaceCreateParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    pub name: String,
}

/// Open a file by absolute path, resolving the workspace context for it. This is the workspace-agnostic
/// entry point used by `ae /path/to/file` and the `Space Alt-w` open-from-path overlay — the cases
/// that may need to *activate* a workspace (an ephemeral one when none is active). The path must be
/// absolute (a leading `~/` is fine): the server will **not** resolve it against its own working
/// directory, which isn't the user's. (`ae path` resolves its arg client-side before sending.)
/// Goto-definition into a file outside the active workspace doesn't go through here: it already has an
/// active workspace to host the guest, so it opens the external buffer directly via `buffer/open`'s
/// `absolute_path` (same external-buffer machinery, no workspace activation). The server:
///
/// - canonicalizes the path;
/// - if the calling client has an active workspace whose roots **contain** the path, opens it there
///   as an ordinary (internal) buffer;
/// - if a workspace is active but the path is **outside** its roots, opens it there as an *external*
///   buffer (the workspace hosts it as a guest — no git, trust-restricted LSP) — it is **not**
///   re-homed into whichever other configured workspace might contain it;
/// - if **no** workspace is active, synthesizes a fresh *ephemeral* workspace (no name, no roots, auto-
///   removed when its last buffer closes), activates it, and opens the file there.
///
/// Returns the (possibly newly-activated) workspace alongside the opened buffer, so the client adopts
/// the workspace id exactly as it does after `workspace/activate`.
pub struct WorkspaceOpenPath;
impl RpcMethod for WorkspaceOpenPath {
    const NAME: &'static str = "workspace/open_path";
    type Params = WorkspaceOpenPathParams;
    type Result = WorkspaceActivateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceOpenPathParams {
    /// File to open. Must be absolute; a leading `~/` is expanded server-side and also counts as
    /// absolute. A relative path is rejected (the server won't resolve it against its own cwd). Must
    /// exist on disk (open-from-path is for existing files).
    pub path: String,
    /// Open the buffer as transient (auto-closes once hidden) — used when the open is a preview.
    /// Defaults to a permanent open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<bool>,
}

/// Add a root path to an existing workspace. Server canonicalizes the path, refuses duplicates,
/// updates the TOML, watches the new path for external changes, and invalidates the workspace's
/// workspace index (so the next picker open re-walks).
pub struct WorkspaceAddRoot;
impl RpcMethod for WorkspaceAddRoot {
    const NAME: &'static str = "workspace/add_root";
    type Params = WorkspaceAddRootParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceAddRootParams {
    /// Workspace to modify. Doesn't have to be the caller's active workspace (the TUI only uses it
    /// for the active workspace today, but the protocol stays general).
    pub workspace: String,
    /// Path on disk. Must exist and be canonicalizable. Leading `~/` is expanded server-side.
    pub path: String,
}

/// Remove a root path from a workspace. The server closes any file-backed buffers under this root
/// that aren't covered by another remaining root, and refuses the whole operation if any such
/// buffer is dirty (with error code `DIRTY_BUFFERS_PREVENT_REMOVE`). Scratch buffers in the
/// workspace are unaffected (they have no path and aren't tied to any root).
///
/// The `next_buffer_id` field follows the same convention as `buffer/close`: when the client's
/// currently-displayed buffer is one of the closed ones, attach to this next id (or spawn a
/// scratch if `None`).
pub struct WorkspaceRemoveRoot;
impl RpcMethod for WorkspaceRemoveRoot {
    const NAME: &'static str = "workspace/remove_root";
    type Params = WorkspaceRemoveRootParams;
    type Result = WorkspaceRemoveRootResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRemoveRootParams {
    pub workspace: String,
    /// The root to remove. Server matches against the workspace's stored canonical paths after
    /// canonicalizing this value too — so callers can pass either the canonical form (what they
    /// got back from `WorkspaceInfo.paths`) or a user-typed equivalent.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRemoveRootResult {
    pub workspace: WorkspaceInfo,
    /// File-backed buffers that were closed as part of this remove. Scratch buffers and buffers
    /// still covered by other roots are not in this list.
    #[serde(default)]
    pub closed_buffer_ids: Vec<crate::BufferId>,
    /// Buffer for the requesting client to attach to if its current buffer was one of the
    /// closed ones. `None` means "no buffers left for you in this workspace — spawn a scratch."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_buffer_id: Option<crate::BufferId>,
}

/// Rename a workspace. Moves the on-disk config (`<old>.toml` → `<new>.toml`) and re-keys the
/// workspace's in-memory state: the server's workspace map, every open buffer's workspace association,
/// and every client's active-workspace pointer. Open buffers are untouched — they keep their ids
/// and paths — so a rename is safe even with dirty buffers in the workspace, and nothing is closed
/// or reloaded.
///
/// Refuses if a workspace named `new_name` already exists, or if `new_name` is empty / contains
/// path separators (same constraints as `workspace/create`). Renaming a workspace to its current
/// name is a no-op that returns the current info.
pub struct WorkspaceRename;
impl RpcMethod for WorkspaceRename {
    const NAME: &'static str = "workspace/rename";
    type Params = WorkspaceRenameParams;
    type Result = WorkspaceInfo;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    /// The workspace to rename (its current name).
    pub workspace: String,
    /// The desired new name.
    pub new_name: String,
}

/// Delete a workspace: remove its on-disk config (`<name>.toml`) and drop its in-memory state,
/// closing any buffers that belonged to it. This forgets the workspace *definition* — it does NOT
/// touch the source files under the workspace's roots.
///
/// Refuses if the workspace is any connected client's active workspace (`ACTIVE_WORKSPACE_PREVENTS_-
/// DELETE`) — the caller must switch away first — or if any buffer in the workspace has unsaved
/// changes (`DIRTY_BUFFERS_PREVENT_DELETE`). Invoked from the workspace switcher; you can't delete
/// a workspace you're not looking at the list of, and you can't delete the one you're in.
pub struct WorkspaceDelete;
impl RpcMethod for WorkspaceDelete {
    const NAME: &'static str = "workspace/delete";
    type Params = WorkspaceDeleteParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceDeleteParams {
    pub name: String,
}

// ---- workspace/renamed (notification) -------------------------------------------------------------

/// Pushed to a client when its active workspace is renamed by *another* client. The server has
/// already re-keyed the receiver's server-side state (active workspace, buffers) to the new name; this
/// tells the client so it can update its local name — which drives both the display and the
/// reconnect baseline (reconnect is by name). Only sent to clients whose active workspace was renamed;
/// the renaming client learns the new name from its `workspace/rename` RPC result instead.
pub struct WorkspaceRenamed;
impl NotificationMethod for WorkspaceRenamed {
    const NAME: &'static str = "workspace/renamed";
    type Params = WorkspaceRenamedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceRenamedParams {
    pub old_name: String,
    pub new_name: String,
}
