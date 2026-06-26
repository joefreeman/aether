//! Deleting a file or directory by path. Used by the Files and Explorer pickers.

use crate::envelope::RpcMethod;
use crate::BufferId;
use serde::{Deserialize, Serialize};

/// Delete a file or directory, moving it to the OS trash (recoverable). Directories go to the
/// trash whole, contents and all. The path must resolve inside one of the active workspace's roots;
/// a workspace root itself can't be deleted this way (use workspace settings to remove a root).
///
/// Refuses if the target — or, for a directory, anything under it — is open in a buffer with
/// unsaved changes (`DIRTY_BUFFERS_PREVENT_DELETE`, with `data.dirty_buffer_ids`). Clean buffers
/// under the path are closed; `next_buffer_id` follows the `buffer/close` convention for the
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
    pub next_buffer_id: Option<BufferId>,
}
