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
    /// No repo to act on. Either a git RPC named a `RepoId` the caller's active workspace can't
    /// reach (a stale id, or one the client invented — validation is what stops either acting on a
    /// repo the user never opened), or it named no repo and the buffer it was resolved against
    /// couldn't supply one: a scratch buffer, or a file outside any repository.
    ///
    /// Not retryable as sent. The message says which case it is and what would fix it, so clients
    /// should surface it and stop rather than falling back to a repo of their own choosing.
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
    // -32043 was AMBIGUOUS_REPO, retired when repo resolution stopped ranging over the workspace:
    // a repo now comes from the buffer or from an explicit `repo_id`, and neither can be ambiguous.
    // Left unused rather than recycled so an old client's error tables can't misread a new code.
    /// `git/show` could not materialise the revision — an unresolvable rev, a path that doesn't
    /// exist at it, or binary content. Carries git's own wording, since it says it better than a
    /// paraphrase would.
    pub const GIT_SHOW_FAILED: Self = Self(-32044);
    /// An edit, save or reload was addressed to a **read-only** buffer — a virtual buffer holding
    /// a revision's content (`git/show`), which has no file behind it and nothing an edit could
    /// mean. Clients decline these locally too; this is the authoritative refusal.
    pub const READ_ONLY_BUFFER: Self = Self(-32045);
    /// `shell/run` was asked to start a command in a shell that is already running one. One run
    /// at a time per shell, so the client says which command is in the way and offers the key
    /// that stops it — and deliberately keeps the text the user typed, since typing ahead of a
    /// build is a reasonable thing to do. Its own code (rather than a generic refusal) exactly so
    /// the client can tell this apart from a shell that has gone away.
    pub const SHELL_BUSY: Self = Self(-32050);
    /// `shell/run` refused the line before running anything: it did not parse, or it named a
    /// command, directory, file or variable that does not exist. The message says which, the
    /// server has already selected the offending word in the input, and the text is left as
    /// typed so it can be corrected rather than retyped. Its own code so the client can say
    /// "not accepted" rather than "failed".
    pub const SHELL_REJECTED: Self = Self(-32051);
    /// `agent/prompt` was asked to start a turn in a conversation that is already running one.
    /// One turn at a time, so the client says so and offers the key that stops it — and keeps the
    /// text the user typed, since typing ahead of an agent is a reasonable thing to do. Its own
    /// code (rather than a generic refusal) exactly so the client can tell this apart from a
    /// conversation whose agent has gone away.
    pub const AGENT_BUSY: Self = Self(-32060);
    /// The conversation's agent could not be reached: it failed to launch, the handshake failed,
    /// or it exited. Distinct from [`Self::AGENT_BUSY`] because there is nothing to wait for —
    /// the view is still there, but it has no agent behind it.
    pub const AGENT_UNAVAILABLE: Self = Self(-32061);

    pub fn code(self) -> i32 {
        self.0
    }
}

impl From<ErrorCode> for i32 {
    fn from(c: ErrorCode) -> i32 {
        c.0
    }
}
