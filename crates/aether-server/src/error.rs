//! Server-side RPC error type. Converts to the on-the-wire `ErrorObject`.

use aether_protocol::envelope::ErrorObject;
use aether_protocol::error::ErrorCode;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    pub fn invalid_params(detail: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::INVALID_PARAMS,
            format!("invalid params: {detail}"),
        )
    }

    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::INTERNAL_ERROR,
            format!("internal error: {detail}"),
        )
    }

    pub fn invalid_path(detail: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::INVALID_PATH, detail.to_string())
    }

    pub fn buffer_not_found(id: u64) -> Self {
        Self::new(
            ErrorCode::BUFFER_NOT_FOUND,
            format!("unknown buffer_id: {id}"),
        )
    }

    pub fn no_active_workspace() -> Self {
        Self::new(
            ErrorCode::NO_ACTIVE_WORKSPACE,
            "no active workspace — call workspace/activate first",
        )
    }

    pub fn unknown_workspace(name: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::UNKNOWN_WORKSPACE,
            format!("no configured workspace named {name}"),
        )
    }

    pub fn file_io(detail: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::FILE_IO, format!("file I/O error: {detail}"))
    }

    pub fn unknown_revision(rev: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::UNKNOWN_REVISION,
            format!("no such revision in this repo: {rev}"),
        )
    }

    /// The buffer a git RPC was resolved against has no repo to name: a scratch buffer, or nothing
    /// open at all. Phrased as the remedy rather than the condition — "no repo" invites the
    /// question this answers, which is *whose* repo the command would have used.
    pub fn repo_needs_file() -> Self {
        Self::new(
            ErrorCode::REPO_NOT_FOUND,
            "Open a file in the repository first",
        )
    }

    /// The buffer has a path, but it isn't inside a repository. Worded exactly like the hunk
    /// commands' long-standing refusal — one condition should not read two ways depending on which
    /// key produced it.
    pub fn not_in_repo() -> Self {
        Self::new(ErrorCode::REPO_NOT_FOUND, "Not in a git repository")
    }

    pub fn repo_not_writable(repo_id: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::REPO_NOT_WRITABLE,
            format!(
                "{repo_id} is not a repo of this workspace — open it as a workspace root to \
                 write to it"
            ),
        )
    }

    pub fn repo_not_found(repo_id: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::REPO_NOT_FOUND,
            format!("no such repo in this workspace: {repo_id}"),
        )
    }

    pub fn git_show_failed(detail: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::GIT_SHOW_FAILED, format!("git show: {detail}"))
    }

    /// A **re**-open of a working-changes view whose tree has since gone clean — a nav-history step
    /// back onto it, or a session's pinned buffer. Nothing to materialise, and unlike a fresh
    /// `Space g w` (which answers `opened: None` and toasts) there is a caller here expecting a
    /// buffer, so it has to be an error. Worded for what actually happened: "buffer not found"
    /// would blame the wrong thing.
    pub fn nothing_to_show() -> Self {
        Self::new(ErrorCode::GIT_SHOW_FAILED, "no working changes to show")
    }

    /// Whether this is [`Self::nothing_to_show`] — a clean tree, not a git failure. Code and
    /// message both, since the code is shared with every other way `git show` can fail.
    pub fn is_nothing_to_show(&self) -> bool {
        let nothing = Self::nothing_to_show();
        self.code == nothing.code && self.message == nothing.message
    }

    pub fn read_only_buffer(buffer_id: aether_protocol::BufferId) -> Self {
        Self::new(
            ErrorCode::READ_ONLY_BUFFER,
            format!("buffer {buffer_id} is read-only"),
        )
    }

    pub fn buffer_has_no_path() -> Self {
        Self::new(
            ErrorCode::BUFFER_HAS_NO_PATH,
            "buffer has no associated file path",
        )
    }

    pub fn would_overwrite(detail: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::WOULD_OVERWRITE,
            format!("would overwrite existing file: {detail}"),
        )
    }

    pub fn path_owned_by_buffer(buffer_id: aether_protocol::BufferId) -> Self {
        Self::new(
            ErrorCode::PATH_OWNED_BY_BUFFER,
            format!("buffer {buffer_id} is already open at this path"),
        )
    }

    pub fn externally_modified(buffer_id: aether_protocol::BufferId) -> Self {
        Self::new(
            ErrorCode::EXTERNALLY_MODIFIED,
            format!("buffer {buffer_id} has been modified on disk since it was loaded"),
        )
    }

    pub fn externally_deleted(buffer_id: aether_protocol::BufferId) -> Self {
        Self::new(
            ErrorCode::EXTERNALLY_DELETED,
            format!("buffer {buffer_id}'s file has been removed from disk"),
        )
    }

    pub fn would_discard_changes(buffer_id: aether_protocol::BufferId) -> Self {
        Self::new(
            ErrorCode::WOULD_DISCARD_CHANGES,
            format!("buffer {buffer_id} has unsaved changes; reload would discard them"),
        )
    }
}

impl From<RpcError> for ErrorObject {
    fn from(e: RpcError) -> Self {
        ErrorObject {
            code: e.code,
            message: e.message,
            data: e.data,
        }
    }
}

impl From<serde_json::Error> for RpcError {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid_params(e)
    }
}
