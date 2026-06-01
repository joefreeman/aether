//! Plain filesystem-directory queries. Distinct from the Explorer picker: no candidate cache,
//! no ranked window, no per-client state. The server reads the directory, validates it's inside
//! the active project's boundary, and returns the full entry list. Used by status-line prompts
//! (save-as, new-file) that need to know what's in a directory without standing up a picker.

use crate::envelope::RpcMethod;
use serde::{Deserialize, Serialize};

/// List a single directory's immediate children. The server canonicalizes `path` and refuses any
/// path outside the active project's access boundary (same rule the Explorer picker uses).
/// Returns every entry the server can stat, sorted dirs-then-files, alphabetical within each —
/// the same order the Explorer picker presents.
pub struct DirectoryList;
impl RpcMethod for DirectoryList {
    const NAME: &'static str = "directory/list";
    type Params = DirectoryListParams;
    type Result = DirectoryListResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryListParams {
    /// Absolute path to list. Need not be canonical; the server canonicalizes before stat'ing.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryListResult {
    /// Canonical absolute path of the listed directory (post-canonicalization of the requested
    /// path). Clients use this to anchor their local "current directory" state.
    pub path: String,
    /// Canonical absolute path of the parent, if it's still inside the project's access
    /// boundary. `None` when at (or above) a project root — same convention as the Explorer
    /// picker's `directory_parent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    /// Leaf name (no path separators).
    pub name: String,
    /// True for subdirectories, false for files.
    pub is_dir: bool,
}

/// Create a directory (and any missing intermediates), then return the canonical absolute
/// path. Used by the Explorer's "+ create directory" affordance — the client suffixes a
/// query with `/` to switch the synthetic row to dir-creation, and selecting it routes here.
/// The server canonicalizes against the deepest existing ancestor and refuses paths outside
/// the active project's boundary.
pub struct DirectoryCreate;
impl RpcMethod for DirectoryCreate {
    const NAME: &'static str = "directory/create";
    type Params = DirectoryCreateParams;
    type Result = DirectoryCreateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryCreateParams {
    /// Absolute path of the directory to create. Need not be canonical; the server resolves
    /// against the deepest existing ancestor before applying the boundary check.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryCreateResult {
    /// Canonical absolute path of the created directory. Clients can feed this directly into
    /// `directory/list` or the Explorer picker's `directory_path` to navigate into it.
    pub path: String,
}
