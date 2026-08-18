//! Error codes used in JSON-RPC error responses.
//!
//! Reserved JSON-RPC 2.0 codes (`-32700`, `-32600`, `-32601`, `-32602`, `-32603`) coexist with
//! application-specific codes in the implementation-defined `-32000` to `-32099` range.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(pub i32);

impl ErrorCode {
    // JSON-RPC reserved
    pub const PARSE_ERROR: Self = Self(-32700);
    pub const INVALID_REQUEST: Self = Self(-32600);
    pub const METHOD_NOT_FOUND: Self = Self(-32601);
    pub const INVALID_PARAMS: Self = Self(-32602);
    pub const INTERNAL_ERROR: Self = Self(-32603);

    // Aether application errors
    pub const INVALID_TOKEN: Self = Self(-32001);
    /// The connecting client has not yet activated a workspace via `workspace/activate`. Every
    /// buffer/cursor/viewport/picker/search/input RPC requires an active workspace; only
    /// `workspace/list` and `workspace/activate` work before activation.
    pub const NO_ACTIVE_WORKSPACE: Self = Self(-32002);
    /// `workspace/activate` named a workspace that has no config file under
    /// `$XDG_CONFIG_HOME/aether/workspaces/`.
    pub const UNKNOWN_WORKSPACE: Self = Self(-32003);
    /// `workspace/remove_root` rejected because at least one buffer under the root being removed
    /// has unsaved changes. The error's `data` field carries `{ "dirty_buffer_ids": [u64] }` so
    /// the client can name them in a prompt. The user has to save or revert those buffers
    /// before retrying.
    pub const DIRTY_BUFFERS_PREVENT_REMOVE: Self = Self(-32004);
    /// `workspace/delete` rejected because the named workspace is the active workspace of at least one
    /// connected client. The client must switch away (activate a different workspace) before the
    /// workspace can be deleted — this is what prevents pulling the rug out from under an open
    /// session.
    pub const ACTIVE_WORKSPACE_PREVENTS_DELETE: Self = Self(-32005);
    /// `workspace/delete` rejected because at least one buffer in the workspace has unsaved changes.
    /// Like [`Self::DIRTY_BUFFERS_PREVENT_REMOVE`], the `data` field carries
    /// `{ "dirty_buffer_ids": [u64] }`. The user has to save or revert those buffers first.
    pub const DIRTY_BUFFERS_PREVENT_DELETE: Self = Self(-32006);
    pub const INVALID_PATH: Self = Self(-32010);
    pub const BUFFER_NOT_FOUND: Self = Self(-32011);
    pub const VIEWPORT_NOT_FOUND: Self = Self(-32012);
    pub const INVALID_POSITION: Self = Self(-32013);
    pub const STALE_REVISION: Self = Self(-32014);
    pub const BUFFER_HAS_NO_PATH: Self = Self(-32015);
    /// Save-as target points at an on-disk file that isn't the saving buffer's current path.
    /// The client should confirm with the user and retry with `overwrite: true`.
    pub const WOULD_OVERWRITE: Self = Self(-32016);
    /// Save-as target is already the canonical path of another open buffer. The client could
    /// (eventually) react by offering to switch to that buffer.
    pub const PATH_OWNED_BY_BUFFER: Self = Self(-32017);
    /// The buffer's on-disk file changed externally since it was last loaded or saved. The
    /// client should confirm with the user and retry the save with `overwrite: true`, or call
    /// `buffer/reload` to discard local changes and pick up the disk version.
    pub const EXTERNALLY_MODIFIED: Self = Self(-32018);
    /// The buffer's on-disk file was removed externally. The client should confirm with the
    /// user and retry the save with `overwrite: true` to recreate it, or close the buffer.
    pub const EXTERNALLY_DELETED: Self = Self(-32019);
    pub const FILE_IO: Self = Self(-32020);
    /// `buffer/reload` called on a dirty buffer without `force: true`. The client should
    /// confirm with the user and retry with `force: true` to discard the local edits.
    pub const WOULD_DISCARD_CHANGES: Self = Self(-32021);
    pub const LANGUAGE_NOT_FOUND: Self = Self(-32030);
    /// A git RPC named a `RepoId` the caller's active workspace can't reach. Either the id was
    /// never returned by `git/repos`, or the workspace has changed since it was. The client
    /// should re-list the repos rather than retrying; ids are server-validated precisely so a
    /// stale or invented one can't act on a repo the user never opened.
    pub const REPO_NOT_FOUND: Self = Self(-32040);
    /// `git/set_baseline` was given a revision `git rev-parse` doesn't recognise in that repo (a
    /// typo, or a branch that only exists on a remote). The previous baseline is left in force —
    /// a bad revision never silently drops the user back to HEAD.
    pub const UNKNOWN_REVISION: Self = Self(-32041);
    /// A *mutating* git RPC named a repo the workspace can only see through an open buffer — a
    /// dependency checkout a goto-definition wandered into, say. Reads there are fine; writes are
    /// refused, because the user never opened it for editing. Add it as a workspace root to write
    /// to it. See `GitRepoInfo::roots`.
    pub const REPO_NOT_WRITABLE: Self = Self(-32042);
    /// A git RPC that resolves its own repo found more than one candidate and no hint good enough
    /// to choose between them. The client should ask the user (a repo chooser) and retry with an
    /// explicit `repo_id`. Guessing is not an option: committing to the wrong repo isn't
    /// recoverable by pressing undo.
    pub const AMBIGUOUS_REPO: Self = Self(-32043);
    /// `git/show` could not materialise the revision — an unresolvable rev, a path that doesn't
    /// exist at it, or binary content. Carries git's own wording, since it says it better than a
    /// paraphrase would.
    pub const GIT_SHOW_FAILED: Self = Self(-32044);
    /// An edit, save or reload was addressed to a **read-only** buffer — a virtual buffer holding
    /// a revision's content (`git/show`), which has no file behind it and nothing an edit could
    /// mean. Clients decline these locally too; this is the authoritative refusal.
    pub const READ_ONLY_BUFFER: Self = Self(-32045);

    pub fn code(self) -> i32 {
        self.0
    }
}

impl From<ErrorCode> for i32 {
    fn from(c: ErrorCode) -> i32 {
        c.0
    }
}
