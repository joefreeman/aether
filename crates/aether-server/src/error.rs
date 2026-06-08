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

    pub fn no_active_project() -> Self {
        Self::new(
            ErrorCode::NO_ACTIVE_PROJECT,
            "no active project — call project/activate first",
        )
    }

    pub fn unknown_project(name: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::UNKNOWN_PROJECT,
            format!("no configured project named {name}"),
        )
    }

    pub fn file_io(detail: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::FILE_IO, format!("file I/O error: {detail}"))
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
