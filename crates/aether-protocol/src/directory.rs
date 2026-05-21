//! Directory browsing — list filesystem entries under a project path.

use crate::envelope::RpcMethod;
use serde::{Deserialize, Serialize};

// ---- directory/list -----------------------------------------------------------------------------

pub struct DirectoryList;
impl RpcMethod for DirectoryList {
    const NAME: &'static str = "directory/list";
    type Params = DirectoryListParams;
    type Result = DirectoryListResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryListParams {
    /// Absolute path to list. The server validates it falls within the project's access
    /// boundary (the union of `project_paths`). `None` defaults to the first project path.
    pub path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryListResult {
    /// Canonical absolute path that was actually listed.
    pub path: String,
    /// Parent directory's canonical path, or `None` when no in-project parent exists (i.e. the
    /// current path is at or above a project root).
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

// ---- directory/create ---------------------------------------------------------------------------

/// Create a directory (and any missing intermediate directories) at `path`. Path must be inside
/// the project. Errors if the path already exists as a file.
pub struct DirectoryCreate;
impl RpcMethod for DirectoryCreate {
    const NAME: &'static str = "directory/create";
    type Params = DirectoryCreateParams;
    type Result = DirectoryCreateResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryCreateParams {
    /// Absolute path to create.
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryCreateResult {
    /// Canonical absolute path that was created (or already existed as a directory).
    pub path: String,
}
