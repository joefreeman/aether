//! Project selection. The server hosts many projects; each client has a single active project
//! at a time. The client picks one with `project/activate` (also used to switch). `project/list`
//! enumerates the projects the server has configured on disk.

use crate::envelope::RpcMethod;
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
}

/// Describes the active project: its name and absolute root paths. Returned by
/// `project/activate`. Paths are server-canonicalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub paths: Vec<String>,
}
