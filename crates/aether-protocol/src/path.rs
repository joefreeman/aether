//! Deleting and renaming a file or directory by path. Used by the Files and Explorer pickers.

use crate::envelope::RpcMethod;
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// Delete a file or directory, moving it to the OS trash (recoverable). Directories go to the
/// trash whole, contents and all. The path must resolve inside one of the active workspace's roots;
/// a workspace root itself can't be deleted this way (use workspace settings to remove a root).
///
/// Refuses if the target — or, for a directory, anything under it — is open in a buffer with
/// unsaved changes (`DIRTY_BUFFERS_PREVENT_DELETE`, with `data.dirty_buffer_ids`). Clean buffers
/// under the path are closed; `next_view_id` follows the `view/close` convention for the
/// requesting client.
pub struct PathDelete;
impl RpcMethod for PathDelete {
    const NAME: &'static str = "path/delete";
    type Params = PathDeleteParams;
    type Result = PathDeleteResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathDeleteParams {
    /// Absolute path of the file or directory to delete. The server canonicalizes it and checks
    /// it falls within a workspace root.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathDeleteResult {
    /// Buffers closed because their backing file was deleted (the file itself, or files under the
    /// deleted directory).
    #[serde(default)]
    pub closed_buffer_ids: Vec<BufferId>,
    /// If the requesting client's current buffer was one of the closed ones, attach to this next
    /// id (or spawn a scratch when `None`). Mirrors `workspace/remove_root`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_view_id: Option<crate::ViewId>,
}

/// Rename or move a file or directory. Both paths must resolve inside the active workspace's
/// roots (either root — a move may cross them), and neither may be a workspace root itself.
///
/// Refuses rather than overwrites: a `to` that already exists is `WOULD_OVERWRITE`, and a `to`
/// some open document is already bound to (a new file not yet saved) is `PATH_OWNED_BY_BUFFER`.
/// The one existing `to` allowed is `from` itself under another spelling — a case-only rename on
/// a case-insensitive filesystem. Missing parent directories of `to` are created.
///
/// Open buffers **follow** the move, unsaved ones included: their documents are re-pointed at the
/// new path, keeping their ids, cursors and undo, and every client showing one gets a
/// `buffer/state` push carrying the new path.
pub struct PathRename;
impl RpcMethod for PathRename {
    const NAME: &'static str = "path/rename";
    type Params = PathRenameParams;
    type Result = PathRenameResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathRenameParams {
    /// Absolute path of the file or directory to move. Must exist.
    pub from: String,
    /// Absolute path it moves to.
    pub to: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathRenameResult {
    /// Buffers whose backing file moved (the file itself, or files under the moved directory).
    #[serde(default)]
    pub moved_buffer_ids: Vec<BufferId>,
}
