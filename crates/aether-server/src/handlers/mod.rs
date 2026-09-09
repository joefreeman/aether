//! RPC method handlers. One function per protocol method, split by protocol namespace.
//!
//! This module is the shared prelude: the `use` block below is imported wholesale by every
//! submodule via `use super::*`, which is why it stays here rather than being split up. Items a
//! sibling module needs are `pub` — `handlers` is private in `lib.rs`, so that is crate-scoped at
//! most, and it lets the flat re-exports below carry them between siblings without per-symbol
//! import lists.

use crate::case;
use crate::cursor as motion;
use crate::error::RpcError;
use crate::grep;
use crate::picker as picker_state;
use crate::state::MOTION_HISTORY_CAP;
use crate::state::{
    BlameCache, Buffer, BufferRange, DeferredToken, Document, DocumentId, EditKindTag,
    ElementBinding, LineEnding, NavEntry, SearchEntry, ServerState, SharedState, SneakCandidate,
    SneakEntry, ViewLayout, Viewport,
};
use crate::surround;
use crate::wrap;
use aether_protocol::app::{AppInfo, AppInfoParams};
use aether_protocol::buffer::{
    BufferChanged, BufferChangedParams, BufferCloseParams, BufferClosed, BufferClosedParams,
    BufferContentParams, BufferContentResult, BufferCopyParams, BufferCopyResult, BufferCutResult,
    BufferOpenParams, BufferOpenResult, BufferReloadParams, BufferReloadResult, BufferSaveParams,
    BufferSaveResult, BufferSetTransientParams, BufferSetTransientResult, BufferState,
    BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorMoveParams, CursorSelectAllParams, CursorSelectLineParams, CursorSelectWordParams,
    CursorSetParams, CursorState, CursorSwapAnchorParams, CursorTreeSelectParams, CursorUndoParams,
    CursorUndoResult, Direction, Granularity, JumplistPosition, Motion, TreeSelectDirection,
    VerticalDirection, WordBoundary,
};
use aether_protocol::directory::{
    DirectoryCreateParams, DirectoryCreateResult, DirectoryEntry, DirectoryListParams,
    DirectoryListResult,
};
use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::git::{
    ApplyHunkStatus, ApplyScope, GitAbortOperationParams, GitAbortOperationResult, GitAbortStatus,
    GitApplyHunkParams, GitApplyHunkResult, GitBaselineChoice, GitBaselineSource, GitBlameChanged,
    GitBlameChangedParams, GitBlameLineParams, GitBlameLineResult, GitBufferStatus,
    GitCancelParams, GitCancelResult, GitChangeCounts, GitCheckoutParams, GitCheckoutResult,
    GitCheckoutStatus, GitCommitParams, GitCommitResult, GitDeleteBranchParams,
    GitDeleteBranchResult, GitDeleteBranchStatus, GitFetchParams, GitFetchResult, GitFetchStatus,
    GitHead, GitNavigateHunkParams, GitNavigateHunkResult, GitOperation, GitOperationChanged,
    GitOperationChangedParams, GitOperationKind, GitPrepareCommitParams, GitPrepareCommitResult,
    GitPullParams, GitPullResult, GitPullStatus, GitPushParams, GitPushResult, GitPushStatus,
    GitRefreshParams, GitRefreshResult, GitRepoInfo, GitRepoOperation, GitResetParams,
    GitResetResult, GitResolveConflictParams, GitResolveConflictResult, GitSetBaselineParams,
    GitSetBaselineResult, GitSetBlameFollowParams, GitSetDiffViewParams, GitStashApplyParams,
    GitStashDropParams, GitStashPushParams, GitStashResult, GitStashStatus, GitUpstreamStatus,
    GitWorktreeAddParams, GitWorktreeAddResult, GitWorktreeAddStatus, GitWorktreeRemoveParams,
    GitWorktreeRemoveResult, GitWorktreeRemoveStatus, GitWorktreeRow, HunkAction, HunkDirection,
    RepoId, ResolveConflictStatus, StagedFile,
};
use aether_protocol::hints::{
    HintsRecordParams, HintsRecordResult, HintsStateParams, HintsStateResult,
};
use aether_protocol::history::{
    HistoryRecordParams, HistoryRecordResult, HistoryStateParams, HistoryStateResult,
};
use aether_protocol::input::{
    BlockDepthParams, BlockEditResult, BlockUnit, BufferOnlyParams, CaseKind, CommentStyle,
    CountedEditParams, EditResult, InputAdjustNumberParams, InputDeleteWordParams,
    InputMoveLinesParams, InputNewlineAndIndentParams, InputOpenLineParams, InputSurroundParams,
    InputTextParams, InputTransformCaseParams, InputUnsurroundParams, LineSide, MoveBlockParams,
    OpenBlockParams, PasteBlockParams, SurroundTarget, ToggleCommentParams, ToggleTaskParams,
    UndoRedoParams, UndoResult,
};
use aether_protocol::jumplist::{
    JumplistCaptureParams, JumplistCaptureResult, JumplistClearParams, JumplistClearResult,
    JumplistStepParams, JumplistStepResult, JumplistStepScope, JumplistStepTarget,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspDocumentHighlightParams, LspFormatResult,
    LspGotoDefinitionResult, LspHoverResult, LspLocation, LspNavigateDiagnosticParams,
    LspNavigateDiagnosticResult, LspReadiness, LspRestartServerParams, LspStatus,
    LspSymbolPathChanged, LspSymbolPathChangedParams, SymbolCrumb,
};
use aether_protocol::nav::{NavGotoParams, NavStepParams, NavStepResult};
use aether_protocol::path::{PathDeleteParams, PathDeleteResult};
use aether_protocol::picker::{
    BufferDirtyState, GroupHeader, MatchOptions, PickerGroupAction, PickerHideParams, PickerItem,
    PickerKind, PickerQueryParams, PickerReset, PickerSelectParams, PickerSelectResult,
    PickerSetGroupParams, PickerSetGroupResult, PickerUpdate, PickerUpdateParams, PickerViewParams,
    PickerViewResult,
};
use aether_protocol::search::{
    SearchClearParams, SearchMatchRange, SearchNavResult, SearchSetParams, SearchSetResult,
    SearchStateChanged, SearchStepParams, SearchSummary,
};
use aether_protocol::settings::{AppSettings, SettingsChanged, SettingsGetParams};
use aether_protocol::sneak::{
    SneakCancelParams, SneakSelectParams, SneakTarget, SneakUpdateParams, SneakUpdateResult,
};
use aether_protocol::viewport::{
    BaselineRow, BufferStatusSnapshot, ConflictLine, DiagnosticSpan, DiffMarker, DiffStage,
    Element, EmphasisRange, LineChange, LogicalLineRange, LogicalLineRender, PatchLine,
    ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams, ViewportResizeParams,
    ViewportScrollParams, ViewportSetWrapParams, ViewportSubscribeParams, ViewportSubscribeResult,
    ViewportWindowResult, Window,
};
use aether_protocol::workspace::{
    WorkspaceActivateParams, WorkspaceActivateResult, WorkspaceAddProjectParams,
    WorkspaceAddRootParams, WorkspaceBindWorktreeParams, WorkspaceCreateParams,
    WorkspaceDeleteParams, WorkspaceInferLanguageParams, WorkspaceInferLanguageResult,
    WorkspaceInfo, WorkspaceListParams, WorkspaceListResult, WorkspaceOpenPathParams,
    WorkspaceProject, WorkspaceRemoveProjectParams, WorkspaceRemoveRootParams,
    WorkspaceRemoveRootResult, WorkspaceRenameParams, WorkspaceRenamed, WorkspaceRenamedParams,
    WorkspaceSummary,
};
use aether_protocol::LogicalPosition;
use aether_protocol::{BufferId, ClientId, Revision, ViewId};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Notifications collected while holding the state lock, paired with their target senders —
/// emitted by the caller after the lock drops so a slow client can't stall the lock.
pub(crate) type PendingPushes = Vec<(mpsc::Sender<Notification>, Notification)>;

/// Per-connection context handed to handlers. Mutable bits live here; the durable state is in
/// [`SharedState`].
pub struct ConnectionCtx {
    /// Assigned at WebSocket-accept time, after the query-string token check. Always set by the
    /// time a handler runs.
    pub client_id: ClientId,
}

// One module per protocol namespace, plus `edit` (the shared apply/undo/push pipeline every
// mutating handler routes through). Each is re-exported flat, so `handlers::buffer_save` still
// resolves for `connection.rs`'s dispatch table and for the dozen other modules that call in.
mod app;
mod blocks;
mod buffer;
mod cursor;
mod edit;
mod git;
mod input;
mod lsp;
mod nav;
mod picker;
mod search;
mod sneak;
mod viewport;
mod workspace;

pub use app::*;
pub use blocks::*;
pub use buffer::*;
pub use cursor::*;
pub use edit::*;
pub use git::*;
pub use input::*;
pub use lsp::*;
pub use nav::*;
pub use picker::*;
pub use search::*;
pub use sneak::*;
pub use viewport::*;
pub use workspace::*;
