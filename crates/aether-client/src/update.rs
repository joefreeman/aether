//! The core update function, grown arm by arm: each migrated subsystem moves its `Message`
//! variants into [`Event`], its handler logic into
//! [`Session::on_event`], and its RPC chains into effect-returning methods here. The shell bridges
//! with a single `Message::Core(Event)` variant and an effect executor.

use super::chips::{self, ChipEditor, ChipEditorField, ChipId};
use super::effect::{
    Effect, Effects, RevealStyle, ShellAction, ToastKind, WindowOpen, WindowTarget,
};
use super::hints::{
    ContextId as HintCtx, HintFacts, HintView, PickerCmd, WireEvent as HintWireEvent,
};
use super::keymap::{lookup, Action, InsertWhere, KeyCode, KeyContext, Mods};
use super::path_editor::{PathBase, PathEditor};

/// What the two absolute-path fields open seeded with, so their completions are on screen before
/// the first keystroke instead of after the user has guessed a prefix.
///
/// Stays a literal `~/` in the field rather than being expanded to a home path: the client core
/// compiles to wasm for the browser shell and has no `$HOME` to expand against, so every consumer —
/// `directory/list`, `workspace/add_root`, `workspace/open_path` — resolves it server-side. It is
/// also simply shorter to read and to edit back out of.
const HOME_PREFIX: &str = "~/";
use super::picker::{GroupLanding, PickerLevel, PickerState, Reveal, FETCH_LIMIT, VISIBLE_ROWS};
use super::session::{
    buffer_info, min_pos, severity_label, step_font_size, step_markdown_width, strip_longest_root,
    AfterSave, AppSettingId, AppSettingsOverlay, CommitDetails, ConfirmAction, ConfirmKind,
    ConnState, HoverBlock, HoverText, Mode, PasteKind, Pending, PendingCommit, Prompt, ReadView,
    ReloadTry, RepeatTarget, SaveTry, SearchSnapshot, Session, SettingsRow, SneakState, TextField,
    ViewState, WorkspaceSettings,
};
use super::transport::RpcError;
use aether_protocol::app::{AppInfoGet, AppInfoParams};
use aether_protocol::buffer::{
    BufferChanged, BufferChangedParams, BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut,
    BufferCutResult, BufferReload, BufferReloadParams, BufferSave, BufferSaveParams, BufferState,
    BufferStateParams, CopyScope,
};
// `ViewState` is the session's here; the protocol notification of that name is spelled out at its
// one use site rather than aliased into the crate's vocabulary.
use aether_protocol::coords::VisualRow;
use aether_protocol::cursor::{
    CursorMove, CursorMoveParams, CursorRedo, CursorSelectAll, CursorSelectAllParams,
    CursorSelectLine, CursorSelectLineParams, CursorSelectWord, CursorSelectWordParams, CursorSet,
    CursorSetParams, CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorTreeSelect,
    CursorTreeSelectParams, CursorUndo, CursorUndoParams, CursorUndoResult, Granularity, Motion,
    SelectionEdge, TreeSelectDirection,
};
use aether_protocol::cursor::{Direction, VerticalDirection};
use aether_protocol::directory::{
    DirectoryCreate, DirectoryCreateParams, DirectoryCreateResult, DirectoryList,
    DirectoryListParams, DirectoryListResult,
};
use aether_protocol::envelope::RpcMethod;
use aether_protocol::envelope::{Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::git::{
    ApplyHunkStatus, ApplyScope, ConflictSide, GitAbortOperation, GitAbortOperationParams,
    GitAbortOperationResult, GitAbortStatus, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult,
    GitBlameChanged, GitBlameChangedParams, GitBlameLine, GitBlameLineParams, GitCancel,
    GitCancelParams, GitCancelResult, GitCheckout, GitCheckoutParams, GitCheckoutResult,
    GitCheckoutStatus, GitCommit, GitCommitParams, GitCommitResult, GitDeleteBranch,
    GitDeleteBranchParams, GitDeleteBranchResult, GitDeleteBranchStatus, GitFetch, GitFetchParams,
    GitFetchResult, GitFetchStatus, GitOperationChanged, GitOperationChangedParams,
    GitPrepareCommit, GitPrepareCommitParams, GitPrepareCommitResult, GitPull, GitPullParams,
    GitPullResult, GitPullStatus, GitPush, GitPushParams, GitPushResult, GitPushStatus,
    GitRepoOperation, GitReset, GitResetParams, GitResetResult, GitResolveConflict,
    GitResolveConflictParams, GitResolveConflictResult, GitSetBlameFollow, GitSetBlameFollowParams,
    GitSetDiffView, GitSetDiffViewParams, GitStashApply, GitStashApplyParams, GitStashDrop,
    GitStashDropParams, GitStashPush, GitStashPushParams, GitStashResult, GitStashStatus,
    GitUpstreamStatus, GitWorktreeAdd, GitWorktreeAddParams, GitWorktreeAddResult,
    GitWorktreeAddStatus, GitWorktreeRemove, GitWorktreeRemoveParams, GitWorktreeRemoveResult,
    GitWorktreeRemoveStatus, HunkAction, ResolveConflictStatus,
};
use aether_protocol::hints::{
    HintsRecord, HintsRecordParams, HintsState, HintsStateParams, HintsStateResult,
};
use aether_protocol::history::{
    HistoryEntry, HistoryKind, HistoryRecord, HistoryRecordParams, HistoryState,
    HistoryStateParams, HistoryStateResult,
};
use aether_protocol::input::{
    BlockDepthParams, BlockEditResult, BufferOnlyParams, CaseKind, CountedEditParams, EditRedo,
    EditResult, EditUndo, InputAdjustNumber, InputAdjustNumberParams, InputBackspace,
    InputBlockDepth, InputChange, InputChangeLine, InputDedent, InputDelete, InputDeleteBlock,
    InputDeleteLine, InputDeleteWord, InputDeleteWordParams, InputIndent, InputJoinLines,
    InputMoveBlock, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent,
    InputNewlineAndIndentParams, InputOpenBlock, InputOpenLine, InputOpenLineParams,
    InputPasteBlock, InputReplaceLine, InputReplaceLineParams, InputSurround, InputSurroundParams,
    InputTab, InputText, InputTextParams, InputToggleComment, InputToggleTask, InputTransformCase,
    InputTransformCaseParams, InputUnsurround, InputUnsurroundParams, LineSide, MoveBlockParams,
    OpenBlockParams, PasteBlockParams, ToggleCommentParams, ToggleTaskParams, UndoRedoParams,
    UndoResult,
};
use aether_protocol::jumplist::{
    JumplistCapture, JumplistCaptureParams, JumplistCaptureResult, JumplistChanged, JumplistClear,
    JumplistClearParams, JumplistClearResult, JumplistStep, JumplistStepParams, JumplistStepResult,
    JumplistStepScope,
};
use aether_protocol::lsp::{
    DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspDocumentHighlight, LspDocumentHighlightParams, LspFormat,
    LspFormatResult, LspGotoDefinition, LspGotoDefinitionResult, LspHover, LspHoverResult,
    LspNavigateDiagnostic, LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspReadiness,
    LspRestartServer, LspRestartServerParams, LspServerStatus, LspStatusChanged,
    LspSymbolPathChanged, LspSymbolPathChangedParams,
};
use aether_protocol::nav::NavStepResult;
use aether_protocol::nav::{NavStep, NavStepParams};
use aether_protocol::path::{PathDelete, PathDeleteParams, PathDeleteResult};
use aether_protocol::picker::{
    BufferDirtyState, CaseMode, GroupHeader, GroupRunRows, MatchOptions, PickerFilters,
    PickerGroupAction, PickerHide, PickerHideParams, PickerItem, PickerKind, PickerQuery,
    PickerQueryParams, PickerReset, PickerSelect, PickerSelectParams, PickerSelectResult,
    PickerSetGroup, PickerSetGroupParams, PickerUpdate, PickerUpdateParams, PickerView,
    PickerViewParams, PickerViewResult, ScopedPath, MIN_GREP_QUERY_LEN,
};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavResult, SearchSet, SearchSetParams, SearchSetResult,
    SearchStateChanged, SearchStep, SearchStepParams, SearchSummary,
};
use aether_protocol::settings::{
    AppSettings, MarkdownWidth, SettingsChanged, SettingsGet, SettingsGetParams, SettingsSet,
    ThemeMode,
};
use aether_protocol::sneak::{
    SneakCancel, SneakCancelParams, SneakSelect, SneakSelectParams, SneakUpdate, SneakUpdateParams,
    SneakUpdateResult,
};
use aether_protocol::syntax::{SyntaxHighlightSnippet, SyntaxHighlightSnippetParams};
use aether_protocol::view::{
    ViewClose, ViewCloseParams, ViewClosed, ViewClosedParams, ViewOpen, ViewOpenParams,
    ViewOpenResult, ViewSetTransient, ViewSetTransientParams, ViewStateParams,
};
use aether_protocol::viewport::{
    DiagnosticSeverity, Element, FocusStep, FocusTarget, NavigateGrain, ViewSave, ViewSaveParams,
    ViewportFocusElement, ViewportFocusElementParams, ViewportFocusElementResult,
    ViewportLinesChanged, ViewportLinesChangedParams, ViewportNavigateChange,
    ViewportNavigateChangeParams, ViewportSubscribeResult, ViewportWindowResult, WrapMode,
};
use aether_protocol::workspace::{
    WorkspaceActivate, WorkspaceActivateParams, WorkspaceActivateResult, WorkspaceAddProject,
    WorkspaceAddProjectParams, WorkspaceAddRoot, WorkspaceAddRootParams, WorkspaceBindWorktree,
    WorkspaceBindWorktreeParams, WorkspaceCreate, WorkspaceCreateParams, WorkspaceDelete,
    WorkspaceDeleteParams, WorkspaceInferLanguage, WorkspaceInferLanguageParams, WorkspaceInfo,
    WorkspaceOpenPath, WorkspaceOpenPathParams, WorkspaceRemoveProject,
    WorkspaceRemoveProjectParams, WorkspaceRemoveRoot, WorkspaceRemoveRootParams,
    WorkspaceRemoveRootResult, WorkspaceRename, WorkspaceRenameParams, WorkspaceRenamed,
    WorkspaceRenamedParams,
};
use aether_protocol::{BufferId, LogicalPosition, ViewId};

/// A core event: an async result (or shell-forwarded input) the core's update consumes.
#[derive(Debug)]
pub enum Event {
    /// `git/set_baseline` resolved: what the repo's gutter now compares against, or `None` for the
    /// default (the index). Only the confirmation toast reads it — the gutter itself follows the
    /// server's own refresh pushes, and the standing state reaches the status bar through
    /// `GitBufferStatus::baseline`.
    BaselineSet(Result<Option<aether_protocol::git::GitBaselineSource>, String>),
    SaveTried(Result<SaveTry, String>),
    ReloadTried(Result<ReloadTry, String>),
    /// A cursor-returning RPC resolved (motions, selections, clicks). Reveals as a `Follow`.
    CursorMsg(Result<CursorState, String>),
    /// As [`CursorMsg`](Event::CursorMsg), but the move was a targeted jump (go-to-line) so the
    /// reveal rests the cursor a quarter down rather than scrolling the minimum.
    CursorJump(Result<CursorState, String>),
    /// An edit resolved: adopt the new revision + cursor.
    EditDone(Result<EditResult, String>),
    /// Focus moved to another editor element by *navigation* — `Tab`, `c` — where the element being
    /// moved to is not necessarily on screen: adopt its cursor and buffer, and frame it.
    ElementFocused(Result<ViewportFocusElementResult, String>),
    /// Focus moved because the user **clicked** an element: adopt its cursor and buffer, and frame
    /// nothing. A click is not blind navigation — the user is looking at what they clicked — so
    /// scrolling it to a rest position moves the text out from under the pointer, and a press whose
    /// content then moves turns the next drag event into a selection nobody asked for.
    ElementClicked(Result<ViewportFocusElementResult, String>),
    UndoRedoDone(Result<UndoResult, String>),
    /// A structural block edit resolved: adopt revision + cursor, clipboard the cut payload, toast
    /// a reasoned refusal, refresh the reading view's parse.
    BlockEditDone(Result<BlockEditResult, String>),
    /// `Ctrl-o`/`Ctrl-Alt-o` resolved: the same adoption, and — only once the server says the
    /// block was actually opened — the hand-over to the editor in Insert.
    OpenBlockDone(Result<BlockEditResult, String>),
    CopyDone(Result<BufferCopyResult, String>),
    CutDone(Result<BufferCutResult, String>),
    /// The shell read the system clipboard for a paste gesture.
    ClipboardRead(PasteKind, Option<String>),
    /// A buffer switch resolved (close, new scratch, path opens): rebind to this buffer. An open
    /// picker survives the switch (see [`Session::adopt_switch`]) — closing it is the pick path's
    /// own job — so the view picker closing the active buffer keeps its list up.
    Switched(Result<ViewOpenResult, String>),
    /// A `git/show` resolved: an ordinary switch onto the materialised buffer, or — for the
    /// working-changes view of a clean tree, the only target that can answer with nothing — a
    /// toast, since there is no buffer and never was one.
    Shown(Result<aether_protocol::git::GitShowResult, String>),
    /// `Enter` in a composed view resolved (or didn't) to the file the line under the cursor
    /// named — a patch line's blob, a shell line's `path:line:col`.
    LineFollowed(Result<aether_protocol::view::ViewFollowLineResult, String>),
    /// `Space b` answered with the shell to show and which of its elements to type into.
    ShellOpened(Result<aether_protocol::shell::ShellOpenResult, RpcError>),
    /// A submit landed, or was refused — the refusal is the interesting half, since it names the
    /// command in the way and the typed text is deliberately still there.
    InputSubmitted(Result<aether_protocol::view::ViewSubmitInputResult, RpcError>),
    AgentOpened(Result<aether_protocol::agent::AgentOpenResult, RpcError>),
    AgentAnswered(Result<aether_protocol::agent::AgentRespondResult, RpcError>),
    AgentCancelled(Result<aether_protocol::agent::AgentCancelResult, String>),
    /// `Space Alt-b` answered. Nothing to do either way: the finish arrives as a push.
    ShellCancelled(Result<aether_protocol::shell::ShellCancelResult, String>),
    /// `view/open` for the current buffer's other view (`Space u`, an edit transition out of
    /// the reader) resolved: adopt the sibling, or report the failure.
    SiblingOpened(Result<ViewOpenResult, String>),
    /// A `syntax/highlight_snippet` result for one fenced code block of the reading view, keyed
    /// by the fence's span start at `(buffer, revision)` parse time — stale results are dropped.
    ReadHighlights {
        buffer_id: BufferId,
        revision: u64,
        block_start: u32,
        result: Result<aether_protocol::syntax::SyntaxHighlightSnippetResult, String>,
    },
    /// A `jumplist/capture` resolved (picker `Ctrl-j`): the list is snapshotted server-side and
    /// the source picker swaps to the Jumplist picker. `Ok(None)` = nothing to capture (the
    /// picker's filtered set was empty); any previously captured list survives. The source
    /// `PickerKind` rides alongside so the confirmation toast can tell a fresh capture from a
    /// re-capture (`kind == Jumplist`, i.e. narrowing the list in place).
    JumplistCaptured(Result<Option<JumplistCaptureResult>, String>, PickerKind),
    /// A `jumplist/step` resolved (`]` / `[` / `Alt-]` / `Alt-[`): `Moved` carries the opened
    /// target; `AtEnd` / `NoneInFile` / `Empty` are no-ops turned into a keyed toast. The
    /// `Direction` and `JumplistStepScope` ride alongside so the boundary toast can name the end
    /// reached (forward = last, backward = first) and whether it was file-scoped, without the
    /// server echoing them back.
    JumplistStepped(
        Result<JumplistStepResult, String>,
        Direction,
        JumplistStepScope,
    ),
    /// A `jumplist/clear` resolved (`Space Alt-j`): the context's list is gone. `cleared == 0`
    /// means there was nothing captured — not a failure, so it toasts rather than errors.
    JumplistCleared(Result<JumplistClearResult, String>),
    /// An `app/info` snapshot resolved (`Space ?`): open the info dialog, or toast the failure.
    AppInfoLoaded(Result<aether_protocol::app::AppInfo, String>),
    /// The prompt's Yes/Save button (keyboard accept routes through `on_prompt_key`).
    PromptAccept,
    PromptCancel,
    /// Incremental `search/set` (cursor follows the match; zero matches revert it).
    SearchApplied(Result<SearchSetResult, String>),
    /// Non-incremental `search/set` (abort-restore, search-from-selection revive): summary
    /// only, the cursor wasn't moved server-side.
    SearchRestored(Result<SearchSetResult, String>),
    SearchNav(Result<SearchNavResult, String>),
    /// A `sneak/update` resolved: adopt the live label set so the next keystroke can be classified
    /// as a label (jump) or a refinement. (The select result routes through [`Event::CursorMsg`].)
    SneakUpdated(Result<SneakUpdateResult, String>),
    SearchFromSel(Result<Option<(String, SearchSetResult)>, String>),
    NavDone {
        forward: bool,
        result: Result<NavStepResult, String>,
    },
    Definition(Result<LspGotoDefinitionResult, String>),
    DiagNav(Result<LspNavigateDiagnosticResult, String>),
    HoverInfo(Result<LspHoverResult, String>),
    FormatDone(Result<LspFormatResult, String>),
    CommitLookup(Result<CommitDetails, String>),
    /// `view/navigate_change` came back: `c`/`Alt-c` stepped a change, or `o`/`Alt-o` an outline
    /// entry. The same focus-shaped reply either way; the grain names the toast when nothing moved.
    ViewStepped {
        grain: NavigateGrain,
        result: Result<ViewportFocusElementResult, String>,
    },
    /// `git/prepare_commit` came back: the message file is written and ready to open.
    CommitPrepared {
        amend: bool,
        result: Result<GitPrepareCommitResult, String>,
    },
    /// `git/commit` came back — created, or refused with git's own words.
    Committed(Result<GitCommitResult, String>),
    /// `git/reset` came back.
    Uncommitted(Result<GitResetResult, String>),
    /// `git/checkout` came back. `branch` is echoed so the toast can name it without re-reading
    /// the picker, which has closed by then.
    CheckedOut {
        branch: String,
        result: Result<GitCheckoutResult, String>,
    },
    /// `git/worktree_add` resolved. `branch` is echoed so the message can name it without the
    /// client having to remember what it asked for.
    WorktreeAdded {
        branch: String,
        result: Result<GitWorktreeAddResult, String>,
    },
    /// `workspace/bind_worktree` resolved. Carries the whole activate result: binding *is* a
    /// workspace switch, plus the report of what didn't follow.
    WorktreeBound(Result<WorkspaceActivateResult, String>),
    /// `git/worktree_remove` resolved. `name` is the admin name that was removed.
    WorktreeRemoved {
        name: String,
        result: Result<GitWorktreeRemoveResult, String>,
    },
    /// `git/delete_branch` came back. `branch` is echoed for the same reason, and `forced` says
    /// whether this was already the escalated attempt — a `NotMerged` refusal of a forced delete
    /// would be a bug, not something to offer forcing again.
    BranchDeleted {
        branch: String,
        forced: bool,
        result: Result<GitDeleteBranchResult, String>,
    },
    /// Any `git/stash_*` outcome. One event for all three because the client does the same thing
    /// with each: toast what happened, and refresh the stash picker if it's open.
    ///
    /// `staged` distinguishes the two pushes, which is a wording difference the *result* can't
    /// carry: "stashed the working tree" after a `--staged` push would claim the unstaged work
    /// went with it, when it is still sitting there.
    StashDone {
        staged: bool,
        result: Result<GitStashResult, String>,
    },
    /// A `Space g f` fetch finished. Only the *asked-for* fetch reports: the periodic one runs
    /// server-side and speaks through the status bar, since a toast every quarter of an hour is
    /// exactly the interruption a background refresh is supposed to avoid.
    FetchDone(Result<GitFetchResult, String>),
    /// A `Space g p` push finished.
    PushDone(Result<GitPushResult, String>),
    /// A `Space g Alt-f` pull finished. Unlike fetch and push this one may have rewritten the
    /// working tree, so its toast reports the reconciliation as checkout's does.
    PullDone(Result<GitPullResult, String>),
    /// A `Space g x` cancel was acknowledged. Silent on success — the operation's own result
    /// arrives right behind it and says what happened — and silent when nothing was running,
    /// which just means the operation finished before the keystroke landed.
    CancelDone(Result<GitCancelResult, String>),
    HunkApplied {
        action: HunkAction,
        /// What the action was aimed at — carried back so the toast can say "file" or "change"
        /// rather than making the user infer how much just moved.
        scope: ApplyScope,
        result: Result<GitApplyHunkResult, String>,
    },
    /// A conflict block was resolved by taking a side. The side rides along so the toast can name
    /// what was kept — the buffer just changed under the user, and "took theirs" is the only
    /// confirmation of *which* change that was.
    ConflictResolved {
        side: ConflictSide,
        result: Result<GitResolveConflictResult, String>,
    },
    /// `Space g d`: a stopped merge/rebase was abandoned (or refused).
    OperationAborted(Result<GitAbortOperationResult, String>),
    DiffViewSet {
        enabled: bool,
        result: Result<ViewportWindowResult, String>,
    },
    PickerViewed {
        initial: bool,
        result: Result<PickerViewResult, String>,
    },
    PickerSelected {
        result: Result<PickerSelectResult, String>,
    },
    /// A picker row was clicked (absolute index) — highlight it and accept.
    PickerClicked(u32),
    /// A filter chip was clicked — select it (virtual selection, like the keyboard path).
    PickerChipClicked(usize),
    /// A root row's delete button was clicked in the workspace-settings overlay — open the shared
    /// confirm prompt for that root (same path as the Delete key → [`Session::request_remove_root`]).
    WorkspaceSettingsRemoveRoot(usize),
    /// A shell-driven remove of project row `index` (the iced overlay's delete button), the
    /// pointer-driven sibling of the `Delete` / `Ctrl-d` chord.
    WorkspaceSettingsRemoveProject(usize),
    /// A setting's checkbox was clicked in the app-settings overlay (flat row index) — toggle it.
    /// The keyboard path (Enter/Space) doesn't use this; it toggles the focused row directly.
    AppSettingToggle(usize),
    /// `directory/list` for the dir-chip editor resolved; `abs` is the staleness key.
    PickerChipListing {
        abs: String,
        result: Result<DirectoryListResult, String>,
    },
    /// `directory/list` for one of the [`PathEditor`] surfaces resolved, keyed by the absolute
    /// directory it was requested for so a stale reply (the editor moved on) is dropped.
    ///
    /// One event for all four rather than one each: they differ only in which editor to route the
    /// answer to, which is exactly what `owner` says. (The dir-chip editor keeps its own
    /// [`Event::PickerChipListing`] — it drives a [`crate::chips::ChipEditor`], and resolving its
    /// listing also re-applies live filters.)
    PathEditorListing {
        owner: PathEditorOwner,
        abs: String,
        result: Result<DirectoryListResult, String>,
    },
    /// `workspace/infer_language` for the add-project row resolved, keyed by the
    /// `(path_index, relative_path)` it asked about so a stale reply (the editor moved on) is
    /// dropped. Errors collapse to `None` at the request site — a background suggestion has
    /// nothing useful to say about failure.
    AddProjectLanguageInferred {
        key: (u32, String),
        language: Option<String>,
    },
    /// `picker/set_group` resolved: the selected run's geometry in the reshaped row space, plus the
    /// landing the gesture asked for (captured at request time — the wire carries only the
    /// geometry; the client picks the row). `None` = the group re-ranked away mid-flight, or a step
    /// ran off the ends (both benign stops).
    GroupSet(Result<Option<GroupRunRows>, String>, GroupLanding),
    /// `path/delete` (Explorer/Files trash) resolved. `noun` labels the success toast; the
    /// open picker re-lists. Buffer closes for the deleted path arrive via the `view/closed`
    /// push, which already switches us off a deleted current buffer.
    PathDeleted {
        noun: &'static str,
        result: Result<PathDeleteResult, String>,
    },
    /// `view/set_transient` (the `Space k` keep toggle) resolved. The bool is the view's new
    /// transient flag; the toast confirms it (`view_transient` itself rides the `view/state`
    /// push). Errors surface as an error toast.
    KeepToggled(Result<bool, String>),
    /// `directory/create` (Explorer "+ Create … name/") resolved: navigate into the new directory.
    DirCreated(Result<DirectoryCreateResult, String>),
    /// Workspace switch resolved: the activated workspace + the buffer to land on.
    WorkspaceActivated(Result<(WorkspaceInfo, ViewOpenResult), String>),
    /// `workspace/create` resolved: the new workspace is active. A fresh workspace has no roots, so
    /// `opened` may be absent — the handler then keeps the current buffer and opens the settings
    /// overlay to add a root.
    WorkspaceCreated(Result<WorkspaceActivateResult, String>),
    /// `workspace/rename` (from the settings overlay) resolved: update the committed name or set
    /// the overlay's error.
    WorkspaceRenamed(Result<WorkspaceInfo, String>),
    /// `workspace/add_root` (from the settings overlay) resolved: refresh the roots or set the error.
    WorkspaceRootAdded(Result<WorkspaceInfo, String>),
    /// `workspace/add_project` landed — the workspace's projects (and its pinned servers) changed.
    WorkspaceProjectAdded(Result<WorkspaceInfo, String>),
    /// `workspace/remove_project` landed.
    WorkspaceProjectRemoved(Result<WorkspaceInfo, String>),
    /// `workspace/remove_root` (from the settings overlay) resolved: refresh the roots and, when the
    /// active buffer was closed, switch to the next one.
    WorkspaceRootRemoved(Result<WorkspaceRemoveRootResult, String>),
    /// `workspace/delete` (from the workspace switcher) resolved: toast success — the refreshed list
    /// arrives via a `picker/update` push — or surface the refusal (active / dirty).
    WorkspaceDeleted(Result<(), String>),
    /// `view/close` resolved for a buffer in an *ephemeral* ("(workspace N)") context, closed
    /// without an `open_next` scratch. Carries the workspace's next remaining buffer: `Some` →
    /// attach to it; `None` → the context is empty, so leave it (quit on native, chooser on web —
    /// see [`App::leave_ephemeral_workspace`]).
    EphemeralClosed(Result<Option<ViewId>, String>),
    /// `view/close` resolved for the [tether](Session::tether): the client's job is done, so
    /// exit. No successor to adopt — the close was issued without `open_next`.
    TetherClosed(Result<(), String>),
    /// `view/set_transient` resolved for the un-keep that *releases* the tether (`Space k` on
    /// the tethered buffer): drop the tether — one-way — and toast the release. The transient
    /// flag itself rides the `buffer/state` push, as with [`Event::KeepToggled`].
    TetherReleased(Result<bool, String>),
    /// `settings/get` resolved at boot: seed the session from the persisted app settings (notably
    /// the soft-wrap default). A failure is non-fatal — we keep the defaults.
    AppSettingsLoaded(Result<AppSettings, String>),
    /// `hints/state` resolved at boot (alongside the settings fetch): adopt the hint learning
    /// snapshot. The engine stays dormant until this lands — which is also the "server connection
    /// succeeded" gate for the very first hint. Failure is non-fatal: no hints this session.
    HintsStateLoaded(Result<HintsStateResult, String>),
    /// `history/state` resolved: adopt the active workspace's `Up`/`Down` recall lists. Fetched at
    /// boot and after every workspace switch. Failure is non-fatal — the lists stay as they were
    /// and recall just has less to offer.
    HistoryLoaded(Result<HistoryStateResult, String>),
    /// `settings/set` (from the app-settings overlay) resolved: a failure surfaces as a toast (the
    /// optimistic local change already applied; this only reports persistence trouble).
    AppSettingsSaved(Result<AppSettings, String>),
    /// A server notification arrived on the session's stream.
    ServerPush(Notification),
    /// The notification stream ended: the connection is gone.
    ConnectionLost,
    /// A reconnect dial failed (no daemon yet) — bump the attempt and retry.
    ReconnectRetry,
    /// A server answered but re-establishing the session failed — terminal.
    ReconnectFatal(String),
    /// The shell re-dialled and re-opened; adopt the fresh session. `restarted` compares the
    /// daemon's start stamp (discovery data the shell holds).
    Reestablished {
        workspace: WorkspaceInfo,
        open: ViewOpenResult,
        restarted: bool,
    },
    /// A fire-and-forget RPC completed; result ignored.
    Noop,
}

/// Which [`PathEditor`] a `directory/list` round-trip belongs to.
///
/// The four surfaces split cleanly by what they name, and that split is what decides whether the
/// listing may leave the workspace: the two `Rooted` ones complete *within* it, the two `Absolute`
/// ones exist precisely to reach outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathEditorOwner {
    /// The save-as prompt (`Alt-s`) — root-relative, files included.
    SaveAs,
    /// The workspace-settings add-project row — root-relative, directories only.
    AddProject,
    /// The workspace-settings add-root row — absolute, directories only.
    AddRoot,
    /// The open-from-path prompt (`Space Alt-w`) — absolute, files included.
    OpenPath,
}

/// The toast for a step that passed over entries whose view no longer holds them — none when it
/// passed over none, which is every ordinary step.
fn skipped_toast(skipped: u32) -> Effects {
    match skipped {
        0 => Effects::none(),
        1 => Effects::toast_grouped(
            "Skipped an entry that is no longer in the review",
            ToastKind::Info,
            "jumplist",
        ),
        n => Effects::toast_grouped(
            format!("Skipped {n} entries that are no longer in the review"),
            ToastKind::Info,
            "jumplist",
        ),
    }
}

impl Session {
    /// The editor `owner` names, when its surface is open.
    ///
    /// The single place that resolves an owner to an editor, so the request site, the response site
    /// and the key router can't disagree about which one they mean.
    fn path_editor_mut(&mut self, owner: PathEditorOwner) -> Option<&mut PathEditor> {
        match owner {
            PathEditorOwner::SaveAs => match self.prompt.as_mut() {
                Some(Prompt::SaveAs(ed)) => Some(ed),
                _ => None,
            },
            PathEditorOwner::AddProject => self
                .workspace_settings
                .as_mut()
                .map(|s| s.add_project.as_mut()),
            PathEditorOwner::AddRoot => self.workspace_settings.as_mut().map(|s| s.add.as_mut()),
            PathEditorOwner::OpenPath => match self.prompt.as_mut() {
                Some(Prompt::OpenPath(ed)) => Some(ed),
                _ => None,
            },
        }
    }

    /// Fire `directory/list` for `owner`'s current dir portion, if its surface is open and has one.
    /// The requested path rides on the result event as the staleness key, so a response that lands
    /// after the editor has moved on is discarded rather than applied.
    ///
    /// This is the **only** place `unrestricted` is set, and it reads the answer off the editor's
    /// own [`PathBase`] rather than taking it from the caller — so an absolute editor cannot be
    /// added later and quietly get a bounded listing that completes nothing.
    fn refresh_path_editor_listing(&mut self, owner: PathEditorOwner) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(ed) = self.path_editor_mut(owner) else {
            return Effects::none();
        };
        let unrestricted = ed.base == PathBase::Absolute;
        let Some(path) = ed.dir_listing_path(&workspace_paths) else {
            return Effects::none();
        };
        let abs = path.clone();
        self.request::<DirectoryList>(DirectoryListParams { path, unrestricted }, move |__r| {
            Event::PathEditorListing {
                owner,
                abs,
                result: __r.map_err(|e| e.message),
            }
        })
    }
}

impl Session {
    /// Dispatch one core event. The shell feeds these from its bridge variant and executes
    /// the returned effects.
    ///
    /// Wraps [`Self::dispatch_event`] to keep the server-side cursor-following decorations in
    /// sync (see [`Self::sync_decoration_follow`]); the twin wrapper on [`Self::on_key`] covers
    /// the key paths.
    pub fn on_event(&mut self, event: Event) -> Effects {
        let fx = self.dispatch_event(event);
        // Events move the hint context too (a picker/view result opens the picker overlay, a
        // buffer switch lands, …) — keep the corner in sync outside the key path as well.
        let fx = fx.and(self.sync_hint_context());
        fx.and(self.sync_decoration_follow())
    }

    /// After a reducer step, reconcile the server's cursor-following decorations — the symbol
    /// highlight set and the cursor-line blame label — with the session's mode/search/buffer.
    /// Follow is **subscription-shaped**: while enabled for a buffer, the server re-resolves the
    /// decoration itself (debounced) on every cursor change, so nothing is sent per move — only
    /// on transitions (mode changes, search start/end, buffer switches), where the previous
    /// subscription is dropped and the new one raised. Both toggles arm an immediate refresh
    /// server-side, so entering Normal mode (or landing in a buffer) paints without waiting for
    /// a move. Run by both reducer entry points so every transition is covered exactly once.
    fn sync_decoration_follow(&mut self) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        let mut fx = Effects::none();

        // Blame label: Normal mode on anything with a history to attribute — a file-backed buffer,
        // or a file at a revision (which has no path but blames at its own revision). Insert mode
        // unfollows so typing never has the server recomputing whole-file blame in the pauses.
        let want_blame = (self.view.mode == Mode::Normal
            && buffer_id != 0
            && (self.view.buffer.path.is_some() || self.view.buffer.is_revision_file()))
        .then_some(buffer_id);
        if self.blame_follow_on != want_blame {
            if let Some(old) = self.blame_follow_on {
                fx = fx.and(self.request::<GitSetBlameFollow>(
                    GitSetBlameFollowParams {
                        buffer_id: old,
                        enabled: false,
                    },
                    |_r| Event::Noop,
                ));
            }
            if let Some(new) = want_blame {
                fx = fx.and(self.request::<GitSetBlameFollow>(
                    GitSetBlameFollowParams {
                        buffer_id: new,
                        enabled: true,
                    },
                    |_r| Event::Noop,
                ));
            }
            self.blame_follow_on = want_blame;
        }

        // Symbol highlights: Normal mode, no active search (a search owns the highlight layer),
        // and only for buffers with a language server, so plain-text buffers never round-trip.
        let want_hl = (self.view.mode == Mode::Normal
            && !self.view.search.active
            && buffer_id != 0
            && self.view.buffer.lsp_server.is_some())
        .then_some(buffer_id);
        if self.highlight_follow_on != want_hl {
            if let Some(old) = self.highlight_follow_on {
                fx = fx.and(self.request::<LspDocumentHighlight>(
                    LspDocumentHighlightParams {
                        buffer_id: old,
                        active: false,
                    },
                    |_r| Event::Noop,
                ));
            }
            if let Some(new) = want_hl {
                fx = fx.and(self.request::<LspDocumentHighlight>(
                    LspDocumentHighlightParams {
                        buffer_id: new,
                        active: true,
                    },
                    |_r| Event::Noop,
                ));
            }
            self.highlight_follow_on = want_hl;
        }
        fx
    }

    fn dispatch_event(&mut self, event: Event) -> Effects {
        match event {
            Event::CursorMsg(Ok(cursor)) => {
                self.view.buffer.cursor = cursor;
                // A staged cross-file-anchor parse installs now — the cursor is on the
                // heading, so the first paint lands in place.
                self.install_staged_read()
                    .and(Effects::one(Effect::RevealCursor(RevealStyle::Follow)))
            }
            Event::CursorMsg(Err(e)) => self
                .install_staged_read()
                .and(Effects::error_detail("Move failed", e)),

            // Go-to-line and other targeted motions reveal as a jump (rest a quarter down).
            Event::CursorJump(Ok(cursor)) => self.jump_to_cursor(cursor),
            Event::CursorJump(Err(e)) => Effects::error_detail("Jump failed", e),

            Event::EditDone(Ok(r)) => {
                self.adopt_edit(r.buffer, r.revision, r.cursor);
                Effects::one(Effect::RevealCursor(RevealStyle::Follow))
            }
            Event::EditDone(Err(e)) => Effects::error_detail("Edit failed", e),

            Event::ElementFocused(Ok(r)) => {
                // Navigating to another element is a **jump**, and reveals like every other one
                // (goto-definition, a search hit, a grep result): off screen, it rests near the top
                // with context below; already on screen, it leaves the view exactly where it is.
                //
                // Not a *minimal* reveal, which is what this used to be: that scrolls the target
                // just far enough to touch the bottom row, so moving to a tall hunk showed one line
                // of it — "Tab didn't scroll". And not an unconditional re-frame either, which
                // scrolls when the thing you asked for is already in front of you.
                //
                // Focus that didn't move (`Tab` at the last element) still reveals rather than
                // jumps: nothing was navigated to, so there is nothing to rest.
                let style = if self.adopt_focus(r) {
                    RevealStyle::Jump
                } else {
                    RevealStyle::Follow
                };
                Effects::one(Effect::RevealCursor(style))
            }
            Event::ElementFocused(Err(e)) => Effects::error_detail("Focus failed", e),

            // A click frames nothing — see [`Event::ElementClicked`]. The `cursor/set` the press
            // sends next is what moves the cursor, and its own reveal keeps it on screen if the
            // click landed at the very edge.
            Event::ElementClicked(Ok(r)) => {
                self.adopt_focus(r);
                Effects::none()
            }
            Event::ElementClicked(Err(e)) => Effects::error_detail("Focus failed", e),

            Event::UndoRedoDone(Ok(r)) => {
                self.adopt_edit(r.buffer, r.revision, r.cursor);
                let mut fx = if r.applied {
                    Effects::none()
                } else {
                    // Grouped so mashing undo/redo at the ends of the stack updates one toast
                    // in place instead of stacking duplicates on every shell.
                    Effects::toast_grouped("Nothing to undo or redo", ToastKind::Info, "undo-redo")
                };
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                // A `Ctrl-z` in the reading view changes the text under the parse; the window
                // the server pushes for it re-parses (`sync_read_presentation`).
                self.mark_read_stale(r.revision);
                fx
            }
            Event::UndoRedoDone(Err(e)) => Effects::error_detail("Undo/redo failed", e),

            Event::BlockEditDone(Ok(r)) => {
                self.adopt_edit(r.buffer, r.revision, r.cursor);
                let mut fx = Effects::none();
                if let Some(text) = r.text {
                    // The cut payload.
                    fx.push(Effect::WriteClipboard(text));
                }
                if let Some(reason) = r.reason {
                    // A reasoned refusal; quiet boundary no-ops carry none.
                    fx = fx.and(Effects::toast_grouped(
                        reason,
                        ToastKind::Info,
                        "block-edit",
                    ));
                }
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                // The edit changed the text under the reading view's parse; the window the
                // server pushes for it re-parses (`sync_read_presentation`).
                if r.applied {
                    self.mark_read_stale(r.revision);
                }
                fx
            }
            Event::BlockEditDone(Err(e)) => Effects::error_detail("Edit failed", e),

            Event::OpenBlockDone(Ok(r)) => {
                self.view.buffer.revision = r.revision;
                self.view.buffer.cursor = r.cursor;
                let mut fx = Effects::none();
                if r.applied {
                    // The caret is already parked in the block the server opened (its landing is
                    // collapsed), so the hand-over is just the mode: no Goto of our own, and
                    // nothing to correct for. A refusal leaves the reading view exactly as it
                    // was — the transition is the *edit's* to make, not the keypress's.
                    self.read_exit_for_edit();
                    self.view.mode = Mode::Insert;
                    fx = fx.and(self.open_sibling(aether_protocol::ui::ViewKind::Editor));
                } else if let Some(reason) = r.reason {
                    fx = fx.and(Effects::toast_grouped(
                        reason,
                        ToastKind::Info,
                        "block-edit",
                    ));
                }
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                fx
            }
            Event::OpenBlockDone(Err(e)) => Effects::error_detail("Edit failed", e),

            // Opening replaces whatever prompt was up: `Space ?` is only reachable from Normal mode
            // via the leader, so nothing that owns the keyboard can be underneath it.
            Event::AppInfoLoaded(Ok(info)) => {
                self.prompt = Some(Prompt::AppInfo(Some(Box::new(info))));
                Effects::none()
            }
            Event::AppInfoLoaded(Err(e)) => Effects::error_detail("App info failed", e),

            Event::CopyDone(Ok(r)) => {
                let mut fx =
                    Effects::toast(format!("Copied {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx
            }
            Event::CopyDone(Err(e)) => Effects::error_detail("Copy failed", e),

            Event::CutDone(Ok(r)) => {
                self.view.buffer.revision = r.revision;
                self.view.buffer.cursor = r.cursor;
                let mut fx =
                    Effects::toast(format!("Cut {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                fx
            }
            Event::CutDone(Err(e)) => Effects::error_detail("Cut failed", e),

            Event::ClipboardRead(kind, text) => {
                let Some(text) = text.filter(|t| !t.is_empty()) else {
                    return Effects::error("Clipboard is empty");
                };
                self.paste(kind, text)
            }

            Event::Switched(Ok(open)) => self.adopt_open(open),
            Event::SiblingOpened(Ok(open)) => self.adopt_sibling(open),
            Event::SiblingOpened(Err(e)) => self.open_failed(e),

            // Worded for the working tree because that is the only target that can answer nothing:
            // a commit or a file at a revision always materialises, and one that can't be
            // resolved is an error, not an empty answer.
            Event::Shown(Ok(shown)) => match shown.opened {
                Some(open) => self.adopt_open(open),
                None => Effects::toast(nothing_to_commit(shown.baseline.as_ref()), ToastKind::Info),
            },
            Event::Shown(Err(e)) => self.open_failed(e),

            // Same landing as any other switch. `opened: None` means the cursor was on the
            // metadata block or the message — nothing to follow, and deliberately silent: `Enter`
            // is a common key and a toast for pressing it on the subject line would be noise.
            // The shell is now on screen; the caret goes into its input, in Insert, so
            // `Space b`, type, `Enter` reads like a REPL. The focused element is set before the
            // resubscribe so the server is told which element to focus rather than being asked to
            // guess from a scroll the client may not have adopted yet.
            // The same landing a shell gets: adopt, focus the input, and start typing. Both are
            // composed views whose last element is a field, and the arrival should feel identical.
            Event::AgentOpened(Ok(r)) => {
                let input = r.input;
                let same_view = r.opened.view_id == self.view.view_id;
                let fx = self.adopt_open(r.opened);
                self.view.focused_element = input;
                self.view.mode = Mode::Insert;
                if same_view {
                    return fx.and(Effects::one(Effect::Resubscribe));
                }
                fx
            }
            Event::AgentOpened(Err(e)) if e.code == ErrorCode::AGENT_UNAVAILABLE.code() => {
                Effects::toast_detail("No agent available", e.message, ToastKind::Info)
            }
            Event::AgentOpened(Err(e)) => {
                Effects::error_detail("Couldn't start an agent", e.message)
            }
            // Nothing to say when it worked — the block's chrome stops showing the question, which
            // is the feedback. Saying nothing happened is worth a word, though: it means the agent
            // stopped asking before you answered.
            Event::AgentAnswered(Ok(r)) if !r.answered => {
                Effects::toast("Nothing was waiting for an answer", ToastKind::Info)
            }
            Event::AgentAnswered(Ok(_)) => Effects::none(),
            Event::AgentAnswered(Err(e)) => {
                Effects::error_detail("Couldn't answer that", e.message)
            }
            Event::ShellOpened(Ok(r)) => {
                let input = r.input;
                let same_view = r.opened.view_id == self.view.view_id;
                let fx = self.adopt_open(r.opened);
                self.view.focused_element = input;
                self.view.mode = Mode::Insert;
                // A switch already resubscribes; landing on the shell you were already looking at
                // does not, and the focus move is what that subscribe carries.
                if same_view {
                    return fx.and(Effects::one(Effect::Resubscribe));
                }
                fx
            }
            Event::ShellOpened(Err(e)) => Effects::error_detail("Couldn't open a shell", e.message),
            // The server says which recall list the line belongs to, so the client files it
            // without ever learning what sort of view it was typed in.
            Event::InputSubmitted(Ok(r)) => {
                if let (Some(line), Some(kind)) = (self.pending_shell_submit.take(), r.history) {
                    self.history.record(kind, HistoryEntry::bare(line));
                }
                Effects::none()
            }
            // Busy is not a failure: the run you asked for is queued in your head, not lost, and
            // the text you typed is still in the input. A refusal is not one either: the line did
            // not pass, the word at fault is already selected, and the message says why. Anything
            // else is an error.
            Event::InputSubmitted(Err(e))
                if e.code == ErrorCode::SHELL_BUSY.code()
                    || e.code == ErrorCode::AGENT_BUSY.code() =>
            {
                self.pending_shell_submit = None;
                Effects::toast_detail("Already running", e.message, ToastKind::Info)
            }
            Event::InputSubmitted(Err(e)) if e.code == ErrorCode::SHELL_REJECTED.code() => {
                self.pending_shell_submit = None;
                Effects::toast_detail("Not accepted", e.message, ToastKind::Info)
            }
            Event::InputSubmitted(Err(e)) => {
                self.pending_shell_submit = None;
                Effects::error_detail("Couldn't run that", e.message)
            }
            Event::ShellCancelled(_) => Effects::none(),
            Event::AgentCancelled(_) => Effects::none(),
            // Same landing as any other switch, and the same silence when the line leads nowhere:
            // `Enter` is a common key, and being told off for pressing it on a line of output
            // would be noise.
            Event::LineFollowed(Ok(r)) => match r.opened {
                Some(open) => self.adopt_navigation(open),
                None => Effects::none(),
            },
            Event::LineFollowed(Err(e)) => Effects::error_detail("Couldn't open the file", e),

            Event::Switched(Err(e)) => self.open_failed(e),

            Event::ReadHighlights {
                buffer_id,
                revision,
                block_start,
                result,
            } => {
                // Best-effort colour: a failed/absent result leaves the fence monochrome.
                let Ok(r) = result else {
                    return Effects::none();
                };
                let Some(read) = self.view.read.as_mut() else {
                    return Effects::none();
                };
                if read.buffer_id != buffer_id
                    || read.revision != revision
                    || r.highlights.is_empty()
                {
                    return Effects::none();
                }
                read.code_highlights.insert(block_start, r.highlights);
                read.hl_gen += 1;
                Effects::none()
            }

            // Last/only buffer of an ephemeral context closed (no scratch was spawned).
            Event::EphemeralClosed(Ok(Some(next))) => {
                // Another view still lives in this ephemeral context — present it.
                self.request_str::<ViewOpen>(
                    ViewOpenParams {
                        view_id: Some(next),
                        ..Default::default()
                    },
                    Event::Switched,
                )
            }
            Event::EphemeralClosed(Ok(None)) => self.leave_ephemeral_workspace(),
            Event::EphemeralClosed(Err(e)) => Effects::error_detail("Close failed", e),

            // The tether closed cleanly — the quick edit this client was launched for is over.
            Event::TetherClosed(Ok(())) => Effects::one(Effect::Exit),
            Event::TetherClosed(Err(e)) => Effects::error_detail("Close failed", e),
            Event::TetherReleased(Ok(_)) => {
                self.tether = None;
                // Same toast group as the plain keep toggle, so repeated presses update in place.
                Effects::toast_grouped("Tether released", ToastKind::Success, "transient")
            }
            Event::TetherReleased(Err(e)) => Effects::error_detail("Release failed", e),

            // Captured: swap the source picker for the Jumplist picker, framed on the row that
            // was highlighted at capture time (its `index` in the new list) — Enter from here jumps
            // through the ordinary select path. Also how a re-capture from the Jumplist picker
            // itself lands: same picker, narrowed list, query cleared. Because the Jumplist picker
            // now looks much like its source, also toast the count so the swap reads as an action.
            Event::JumplistCaptured(Ok(Some(r)), source) => {
                let noun = if r.total == 1 { "result" } else { "results" };
                let msg = if source == PickerKind::Jumplist {
                    format!("Narrowed jumplist to {} {noun}", r.total)
                } else {
                    format!("Captured {} {noun} to the jumplist", r.total)
                };
                let toast = Effects::toast_grouped(msg, ToastKind::Success, "jumplist");
                let hide = self.close_picker();
                toast.and(hide).and(self.open_picker(
                    PickerKind::Jumplist,
                    None,
                    None,
                    false,
                    Some(PickerItem::JumplistEntry {
                        index: r.index,
                        // Only `index` identifies the row for centering; line/display unused.
                        line: None,
                        display: String::new(),
                        match_indices: Vec::new(),
                    }),
                ))
            }
            // Nothing to capture (empty filtered set) — the source picker stays open; say why
            // nothing visibly happened.
            Event::JumplistCaptured(Ok(None), _) => {
                Effects::toast_grouped("Nothing to capture", ToastKind::Info, "jumplist")
            }
            Event::JumplistCaptured(Err(e), _) => Effects::error_detail("Capture failed", e),

            // Captured from a view that is still open: land *in* it, exactly as selecting the same
            // row in the picker that captured it does. `element` is only ever set when the server
            // still holds a viewport on that view, so there is nothing to open — the file is
            // already on screen, as one of the view's windows onto it.
            Event::JumplistStepped(Ok(JumplistStepResult::Moved(t)), _, _)
                if t.seat.is_some() && t.position.is_some() =>
            {
                let t = *t;
                let (seat, position) = (t.seat.unwrap(), t.position.unwrap());
                let seated = self.seat_in_view_element(
                    t.opened,
                    seat.element,
                    seat.buffer_id,
                    position,
                    t.anchor,
                );
                skipped_toast(t.skipped).and(seated)
            }
            // Nothing further in this direction is still in its view. The view is shown when it
            // had to be brought back to look — that is where the entries are — and the toast says
            // why nothing moved.
            Event::JumplistStepped(
                Ok(JumplistStepResult::Gone {
                    skipped, opened, ..
                }),
                _,
                _,
            ) => {
                let shown = match opened {
                    Some(open) => self.adopt_navigation(*open),
                    None => Effects::none(),
                };
                let msg = if skipped > 1 {
                    format!("{skipped} entries are no longer in the review")
                } else {
                    "This entry is no longer in the review".to_string()
                };
                shown.and(Effects::toast_grouped(msg, ToastKind::Info, "jumplist"))
            }
            Event::JumplistStepped(Ok(JumplistStepResult::Moved(t)), _, _) => {
                let skipped = skipped_toast(t.skipped);
                match t.opened {
                    Some(open) => {
                        // A step is jump-shaped exactly when its entry carries a position — the
                        // same test `open_path_at` applies, so `]` and Enter on the same row present
                        // the target identically. A positioned entry (a grep hit, a diagnostic)
                        // opened with a `jump_to`, which the server lands in the editor, where a
                        // line:col means something; a whole-target entry is "open this file", so
                        // a markdown one comes back as it last was.
                        skipped.and(self.adopt_navigation(open))
                    }
                    None => skipped, // open:true is always sent; defensive
                }
            }
            // At the boundary — no wrap. Name the end reached (and, when file-scoped, that the
            // list continues in other files); keyed so holding the key coalesces.
            Event::JumplistStepped(Ok(JumplistStepResult::AtEnd), direction, scope) => {
                let msg = match (direction, scope) {
                    (Direction::Forward, JumplistStepScope::Full) => "Last jumplist entry",
                    (Direction::Backward, JumplistStepScope::Full) => "First jumplist entry",
                    (Direction::Forward, JumplistStepScope::CurrentFile) => {
                        "Last jumplist entry in this file"
                    }
                    (Direction::Backward, JumplistStepScope::CurrentFile) => {
                        "First jumplist entry in this file"
                    }
                };
                Effects::toast_grouped(msg, ToastKind::Info, "jumplist")
            }
            // File-scoped (`Alt-]`/`Alt-[`) with no entries in the current file — `]`/`[` would
            // instead cross into another file.
            Event::JumplistStepped(Ok(JumplistStepResult::NoneInFile), _, _) => {
                Effects::toast_grouped(
                    "No jumplist entries in this file",
                    ToastKind::Info,
                    "jumplist",
                )
            }
            // Grouped so repeatedly pressing `]` with nothing captured coalesces to one toast.
            Event::JumplistStepped(Ok(JumplistStepResult::Empty), _, _) => {
                Effects::toast_grouped("Jumplist is empty", ToastKind::Info, "jumplist")
            }
            Event::JumplistStepped(Err(e), _, _) => Effects::error_detail("Jump failed", e),

            // `Space Alt-j`. Adopt the re-decorated cursor so the status bar's `k/N` segment goes
            // with the list rather than lingering until the next keystroke.
            Event::JumplistCleared(Ok(r)) => {
                if let Some(cursor) = r.cursor {
                    self.view.buffer.cursor = cursor;
                }
                let msg = if r.cleared == 0 {
                    "Jumplist is already empty".to_string()
                } else {
                    let noun = if r.cleared == 1 { "result" } else { "results" };
                    format!("Cleared {} {noun} from the jumplist", r.cleared)
                };
                let kind = if r.cleared == 0 {
                    ToastKind::Info
                } else {
                    ToastKind::Success
                };
                Effects::toast_grouped(msg, kind, "jumplist")
            }
            Event::JumplistCleared(Err(e)) => Effects::error_detail("Clear failed", e),

            Event::PromptAccept => self.accept_prompt(),
            Event::PromptCancel => self.decline_prompt(),

            Event::SearchApplied(Ok(r)) => {
                self.view.buffer.cursor = r.cursor;
                let zero = r.summary.total == 0;
                self.view.search.summary = Some(r.summary);
                if zero {
                    // A failed keystroke shouldn't strand the user wherever the previous
                    // query had jumped them.
                    self.revert_to_snapshot_cursor()
                } else {
                    Effects::one(Effect::RevealCursor(RevealStyle::Jump))
                }
            }
            Event::SearchApplied(Err(_)) => {
                // Most commonly an invalid regex mid-type (e.g. a trailing `\`): treat as a
                // transient zero-match state.
                self.view.search.summary = Some(SearchSummary {
                    buffer_id: self.view.buffer.buffer_id,
                    total: 0,
                    truncated: false,
                    current_index: 0,
                });
                // Re-fires on every keystroke of an in-progress bad pattern; grouped so the shells
                // refresh one toast in place rather than stacking one per key.
                Effects::toast_grouped("Invalid regex", ToastKind::Warning, "search-error")
                    .and(self.revert_to_snapshot_cursor())
            }

            Event::SearchRestored(Ok(r)) => {
                self.view.search.summary = Some(r.summary);
                Effects::none()
            }
            Event::SearchRestored(Err(e)) => Effects::error_detail("Search failed", e),

            Event::SearchNav(Ok(r)) => {
                self.view.search.summary = Some(r.summary);
                self.jump_to_cursor(r.cursor)
            }
            Event::SearchNav(Err(e)) => Effects::error_detail("Search failed", e),

            Event::SneakUpdated(Ok(result)) => {
                // The session may have ended (label pressed, Esc) before this result landed; only
                // adopt labels while still sneaking.
                if let Some(sneak) = self.view.sneak.as_mut() {
                    sneak.labels = result.labels;
                }
                Effects::none()
            }
            Event::SneakUpdated(Err(e)) => Effects::error_detail("Sneak failed", e),

            Event::SearchFromSel(Ok(Some((query, r)))) => {
                self.view.search.query = query.clone();
                // Mirror the defaults the request went out with, so the committed search's state
                // matches how the server is actually matching it.
                self.view.search.options = MatchOptions::default();
                self.view.search.active = true;
                self.view.search.summary = Some(r.summary);
                let entry = HistoryEntry::with_options(query, self.view.search.options);
                self.record_history(HistoryKind::Search, entry)
            }
            Event::SearchFromSel(Ok(None)) => Effects::none(), // empty selection
            Event::SearchFromSel(Err(e)) => Effects::error_detail("Search failed", e),

            Event::NavDone { forward, result } => match result {
                // Same-buffer step glides, cross-buffer step switches — see `adopt_navigation`.
                Ok(NavStepResult { target: Some(open) }) => self.adopt_navigation(open),
                // Grouped so mashing back/forward at an end of the nav history updates one toast.
                Ok(_) => Effects::toast_grouped(
                    if forward {
                        "No later location in history"
                    } else {
                        "No earlier location in history"
                    },
                    ToastKind::Info,
                    "nav-history",
                ),
                Err(e) => Effects::error_detail("Navigation failed", e),
            },

            Event::Definition(Ok(r)) => match lsp_readiness_message(r.readiness) {
                Some((msg, why)) => Effects::toast_detail(msg, why, ToastKind::Info),
                None => match r.location {
                    Some(location) => {
                        // Land the identifier selected (anchor at its start, cursor on its last
                        // char) — consistent with the outline and references pickers. A point when
                        // the server gave no distinct span (`end == position`).
                        let start = location.position;
                        let end = location.end;
                        self.open_path_at(location.path, Some(end), (end != start).then_some(start))
                    }
                    None => Effects::toast("No definition found", ToastKind::Info),
                },
            },
            Event::Definition(Err(e)) => Effects::error_detail("Go to definition failed", e),

            Event::DiagNav(Ok(r)) => self.step_to_cursor(r.cursor, r.moved, "No more diagnostics"),
            Event::DiagNav(Err(e)) => Effects::error_detail("Diagnostic navigation failed", e),

            Event::HoverInfo(Ok(r)) => match r.contents {
                // Render per the server-reported kind: Markdown as Markdown, plaintext literally
                // (a single block) so its `*`/`_`/`#`/backticks aren't misinterpreted as Markdown.
                Some(text) if r.markdown => Effects::one(Effect::ShowHover(HoverText::Markdown(
                    crate::markdown::parse(&text),
                ))),
                Some(text) => {
                    Effects::one(Effect::ShowHover(HoverText::Blocks(vec![HoverBlock {
                        severity: None,
                        text,
                    }])))
                }
                // No content: say *why* — a server still starting / crashed isn't the same as a
                // ready server that simply has nothing here ("No hover info").
                None => {
                    let (msg, why) =
                        lsp_readiness_message(r.readiness).unwrap_or(("No hover info", ""));
                    let mut fx = Effects::one(Effect::DismissHover);
                    fx.push(Effect::Toast {
                        title: msg.into(),
                        body: (!why.is_empty()).then(|| why.to_string()),
                        kind: ToastKind::Info,
                        group: None,
                    });
                    fx
                }
            },
            Event::HoverInfo(Err(e)) => Effects::error_detail("Hover failed", e),

            Event::FormatDone(Ok(r)) => {
                self.view.buffer.cursor = r.cursor;
                // Specific feedback per outcome — "nothing happened" has several causes.
                let note = match r.status {
                    FormatStatus::Applied => None,
                    FormatStatus::NoChange => Some("Already formatted".to_string()),
                    FormatStatus::NotReady => Some("Language server still starting".to_string()),
                    FormatStatus::Unavailable => Some("Language server unavailable".to_string()),
                    FormatStatus::Unsupported => Some(match self.view.buffer.language.as_deref() {
                        Some(lang) => format!("No formatter for {lang}"),
                        None => "No formatter for this file".to_string(),
                    }),
                };
                let mut fx = match note {
                    Some(n) => Effects::toast(n, ToastKind::Info),
                    None => Effects::none(),
                };
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                fx
            }
            Event::FormatDone(Err(e)) => Effects::error_detail("Format failed", e),

            Event::CommitLookup(Ok(CommitDetails::Info(info))) => {
                // Mirror `git show`'s header: commit / Author / Date, blank line, message.
                let text = format!(
                    "commit {}\nAuthor: {} <{}>\nDate:   {}\n\n{}",
                    info.commit, info.author, info.email, info.date, info.message
                );
                Effects::one(Effect::ShowHover(HoverText::Blocks(vec![HoverBlock {
                    severity: None,
                    text,
                }])))
            }
            Event::CommitLookup(Ok(CommitDetails::Note(note))) => {
                Effects::toast(note, ToastKind::Info)
            }
            Event::CommitLookup(Err(e)) => Effects::error_detail("Commit info failed", e),

            Event::ViewStepped {
                grain,
                result: Ok(r),
            } => {
                // Moved or not is read off the reply: the server answers with where the cursor is,
                // and "nowhere further" is the same place it was — in the same element.
                let before = (self.view.focused_element, self.view.buffer.cursor);
                let crossed = self.adopt_focus(r);
                let moved = crossed || self.view.buffer.cursor != before.1;
                let mut fx = if moved {
                    Effects::none()
                } else {
                    // Grouped so repeatedly stepping with nowhere left to go coalesces to one toast.
                    let exhausted = match grain {
                        NavigateGrain::Change => "No more changes",
                        NavigateGrain::Outline => "No more symbols",
                    };
                    Effects::toast_grouped(exhausted, ToastKind::Info, "step-nav")
                };
                // A change is a jump, as it always was; a symbol step within one buffer follows
                // like the motion it always was, and only jumps when it crosses into another
                // element.
                fx.push(Effect::RevealCursor(
                    if grain == NavigateGrain::Change || crossed {
                        RevealStyle::Jump
                    } else {
                        RevealStyle::Follow
                    },
                ));
                fx
            }
            Event::ViewStepped { result: Err(e), .. } => {
                Effects::error_detail("Navigation failed", e)
            }

            Event::CommitPrepared { amend, result } => match result {
                Ok(prepared) => {
                    // Conflicts outstanding: git would refuse this commit, so nothing was prepared.
                    // Named files, because "resolve them" is only actionable if you know which.
                    if !prepared.blocked_by_conflicts.is_empty() {
                        return Effects::toast_detail(
                            "Still conflicted",
                            name_a_few(&prepared.blocked_by_conflicts),
                            ToastKind::Warning,
                        );
                    }
                    // Nothing staged is the overwhelmingly common mistake, and `git commit` would
                    // only refuse *after* we'd opened a buffer and made the user write a message.
                    // Amending is exempt: rewording the previous commit stages nothing.
                    if prepared.staged.is_empty() && !amend {
                        return Effects::toast("Nothing staged to commit", ToastKind::Info);
                    }
                    let summary = if amend {
                        "Amending".to_string()
                    } else {
                        format!(
                            "Committing {} file{}",
                            prepared.staged.len(),
                            if prepared.staged.len() == 1 { "" } else { "s" }
                        )
                    };
                    let repo_id = prepared.repo_id.clone();
                    // Not transient: a preview auto-closes when hidden, and a half-written commit
                    // message vanishing because you glanced at another file would be its own bug.
                    let mut fx = self.request_str::<ViewOpen>(
                        ViewOpenParams {
                            absolute_path: Some(prepared.path),
                            transient: Some(false),
                            record_nav_from: Some(self.view.buffer.buffer_id),
                            ..Default::default()
                        },
                        Event::Switched,
                    );
                    // The buffer id isn't known until the open lands, so park the rest and let
                    // `Switched` attach it (see `adopt_pending_commit`).
                    self.pending_commit = Some(PendingCommit {
                        buffer_id: 0,
                        view_id: ViewId::default(),
                        repo_id,
                        amend,
                    });
                    fx.push(Effect::Toast {
                        title: summary,
                        body: Some("Write the message, then close this view".into()),
                        kind: ToastKind::Info,
                        group: None,
                    });
                    fx
                }
                Err(e) => Effects::error_detail("Commit failed", e),
            },

            Event::Committed(result) => match result {
                // An empty message is the user changing their mind — git's own abort rule. Close
                // the buffer as an ordinary close would, and say so quietly: this is the path
                // "open the message, think better of it, press close" takes, and it must not
                // strand them in a buffer they can't leave without writing something.
                Ok(res) if res.empty_message => {
                    self.pending_commit = None;
                    self.close_view()
                        .and(Effects::toast("Commit abandoned", ToastKind::Info))
                }
                Ok(res) => {
                    match res.commit {
                        Some(commit) => {
                            // Cleared only on success — a refusal leaves the entry standing so the
                            // *next* close retries the commit. Without that, fixing what a
                            // `pre-commit` hook complained about and pressing close again would
                            // silently abandon the message instead of committing it.
                            self.pending_commit = None;
                            let subject = commit
                                .message
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            let short: String = commit.commit.chars().take(7).collect();
                            let mut fx = self.close_view();
                            // What the commit *concluded*, when it was part of something bigger.
                            // A rebase that stopped again is the case worth spelling out: the
                            // commit worked, and there is more to resolve before it's over.
                            let detail = match (&res.operation, res.conflicts.is_empty()) {
                                (Some(op), false) => format!(
                                    "{} stopped at {}",
                                    op.noun(),
                                    name_a_few(&res.conflicts)
                                ),
                                (Some(op), true) => format!("finished the {}", op.noun()),
                                (None, _) => subject,
                            };
                            fx.push(Effect::Toast {
                                title: format!("Committed {short}"),
                                body: Some(detail),
                                kind: if res.conflicts.is_empty() {
                                    ToastKind::Success
                                } else {
                                    ToastKind::Warning
                                },
                                group: None,
                            });
                            fx
                        }
                        // A refusal keeps the buffer open with the message intact: a failing
                        // `pre-commit` hook is something you fix and retry, not something that
                        // should cost you what you wrote. git's own words, unedited — in the
                        // detail line, where every other git refusal puts them.
                        None => {
                            Effects::toast_detail("Commit refused", res.message, ToastKind::Warning)
                        }
                    }
                }
                Err(e) => Effects::error_detail("Commit failed", e),
            },

            Event::OperationAborted(result) => match result {
                Ok(res) => match res.status {
                    GitAbortStatus::Aborted => Effects::toast(
                        match res.operation {
                            Some(op) => format!("Abandoned the {}", op.noun()),
                            None => "Abandoned the operation".to_string(),
                        },
                        ToastKind::Success,
                    ),
                    // Pressing the way-out key on a repo that isn't stuck should say so, not fail.
                    GitAbortStatus::NothingInProgress => {
                        Effects::toast("Nothing in progress to abandon", ToastKind::Info)
                    }
                    GitAbortStatus::BlockedByDirtyBuffers => Effects::toast_detail(
                        format!("{} unsaved file(s)", res.blocked.len()),
                        "Save first, then retry",
                        ToastKind::Warning,
                    ),
                    GitAbortStatus::Refused => Effects::error_detail("Abandon failed", res.message),
                },
                Err(e) => Effects::error_detail("Abandon failed", e),
            },

            Event::Uncommitted(result) => match result {
                Ok(res) if res.head.is_some() => {
                    // Name what came back rather than reporting a hash movement: "Uncommitted:
                    // Add a line" is the sentence the user can check against their intent.
                    // Shaped like the commit toast it undoes: the hash names *which*, the
                    // subject in the detail line is what you check against your intent.
                    let (title, detail) = match res.undone.first() {
                        Some(c) => {
                            let short: String = c.commit.chars().take(7).collect();
                            (
                                format!("Uncommitted {short}"),
                                format!(
                                    "{} — its changes are staged",
                                    c.message.lines().next().unwrap_or_default()
                                ),
                            )
                        }
                        None => (
                            "Uncommitted".to_string(),
                            "Its changes are staged".to_string(),
                        ),
                    };
                    Effects::toast_detail(title, detail, ToastKind::Success)
                }
                // Refused: no parent commit (the initial commit has nothing behind it), or a
                // repo-level objection. git's own words.
                Ok(res) => {
                    Effects::toast_detail("Uncommit failed", res.message, ToastKind::Warning)
                }
                Err(e) => Effects::error_detail("Uncommit failed", e),
            },

            // One `match` on the status, because that's what the status enum is for: every arm
            // has a different message and a different thing for the user to do next.
            Event::BaselineSet(result) => match result {
                Err(e) => Effects::error_detail("Couldn't change the diff baseline", e),
                // Grouped so cycling through a few baselines leaves one toast, not a stack. The
                // status bar carries the standing state; this is only the moment-of-change
                // confirmation, which is why it says nothing when the picker was dismissed.
                Ok(baseline) => Effects::toast_grouped(
                    match &baseline {
                        None => "Diffing against the index".to_string(),
                        Some(aether_protocol::git::GitBaselineSource::Saved) => {
                            "Diffing against the files on disk".to_string()
                        }
                        Some(aether_protocol::git::GitBaselineSource::Rev { label, .. }) => {
                            format!("Diffing against {label}")
                        }
                    },
                    ToastKind::Success,
                    "git-baseline",
                ),
            },
            Event::CheckedOut { branch, result } => match result {
                Err(e) => Effects::error_detail(format!("Couldn't switch to {branch}"), e),
                Ok(res) => match res.status {
                    GitCheckoutStatus::Switched | GitCheckoutStatus::Created => {
                        let verb = if res.status == GitCheckoutStatus::Created {
                            "Created"
                        } else {
                            "Switched to"
                        };
                        // Name the buffers the switch disturbed. A checkout that quietly left
                        // three buffers pointing at content from another branch is exactly the
                        // surprise the reconciliation pass exists to prevent, so say so — as the
                        // detail line, since the switch itself is the headline.
                        let mut detail = String::new();
                        let moved = res.refreshed.reloaded.len();
                        if moved > 0 {
                            detail.push_str(&format!("Reloaded {moved} file(s)"));
                        }
                        if !res.refreshed.missing.is_empty() {
                            if detail.is_empty() {
                                detail.push_str(&format!(
                                    "{} file(s) not on this branch",
                                    res.refreshed.missing.len()
                                ));
                            } else {
                                detail.push_str(&format!(
                                    ", {} not on this branch",
                                    res.refreshed.missing.len()
                                ));
                            }
                        }
                        Effects::toast_detail(
                            format!("{verb} {branch}"),
                            detail,
                            ToastKind::Success,
                        )
                    }
                    // The one refusal with a concrete next step, so it gets one.
                    GitCheckoutStatus::BlockedByDirtyBuffers => Effects::toast_detail(
                        format!("{} unsaved file(s)", res.blocked.len()),
                        "Save first, then retry",
                        ToastKind::Warning,
                    ),
                    GitCheckoutStatus::AlreadyCheckedOut => Effects::toast(
                        format!("{branch} is checked out in {}", res.message),
                        ToastKind::Warning,
                    ),
                    GitCheckoutStatus::Refused => Effects::toast_detail(
                        format!("Couldn't switch to {branch}"),
                        res.message,
                        ToastKind::Warning,
                    ),
                },
            },

            // Creating a worktree is a checkout into a directory nothing is looking at, so the
            // report is the whole feedback: where it went, and the two facts that decide whether
            // it will actually work — what was seeded, and whether submodules are in play.
            Event::WorktreeAdded { branch, result } => {
                match result {
                    Err(e) => {
                        Effects::error_detail(format!("Couldn't create a worktree for {branch}"), e)
                    }
                    Ok(res) => match res.status {
                        GitWorktreeAddStatus::Created => {
                            // Creating never moves you. `Ctrl-o` is a lifecycle verb and the picker
                            // stays open on the row, which now renders its worktree marker — that
                            // change *is* the feedback. This used to chain a bind off the result,
                            // back when selecting a branch row meant "create a tree and go there";
                            // splitting the two is what stopped a cancelled or refused create from
                            // leaving a half-applied gesture behind.
                            let mut detail = String::new();
                            if res.seeded_files > 0 {
                                detail.push_str(&format!("Seeded {} file(s)", res.seeded_files));
                            }
                            if res.has_submodules {
                                // git's own BUGS section advises against this and the failure mode is
                                // silent commit loss, so it is said out loud rather than logged.
                                if !detail.is_empty() {
                                    detail.push_str("; ");
                                }
                                detail.push_str("repo has submodules — see git-worktree(1) BUGS");
                            }
                            Effects::toast_detail(
                                format!("Created worktree for {branch}"),
                                detail,
                                ToastKind::Success,
                            )
                        }
                        // The refusal with a concrete next step: the branch is already open somewhere,
                        // and going there is what the user wanted anyway.
                        GitWorktreeAddStatus::AlreadyCheckedOut => Effects::toast(
                            format!(
                                "{branch} is already checked out in {}",
                                res.checked_out_in.unwrap_or_default()
                            ),
                            ToastKind::Warning,
                        ),
                        GitWorktreeAddStatus::NoSuchBranch => {
                            Effects::toast(format!("No branch {branch}"), ToastKind::Warning)
                        }
                        GitWorktreeAddStatus::InvalidBranchName => Effects::toast(
                            format!("{branch} isn't a usable branch name"),
                            ToastKind::Warning,
                        ),
                        GitWorktreeAddStatus::Unborn => Effects::toast_detail(
                            "No commits yet",
                            "Commit before creating a worktree",
                            ToastKind::Warning,
                        ),
                        GitWorktreeAddStatus::Cancelled => {
                            Effects::toast("Worktree creation cancelled", ToastKind::Info)
                        }
                        GitWorktreeAddStatus::Refused => Effects::toast_detail(
                            format!("Couldn't create a worktree for {branch}"),
                            res.message,
                            ToastKind::Warning,
                        ),
                    },
                }
            }

            Event::WorktreeRemoved { name, result } => match result {
                Err(e) => Effects::error_detail(format!("Couldn't remove {name}"), e),
                Ok(res) => match res.status {
                    GitWorktreeRemoveStatus::Removed => Effects::toast_detail(
                        format!("Removed worktree {name}"),
                        "Its branch was kept",
                        ToastKind::Success,
                    ),
                    // Itemise rather than ask again: the point of the refusal is that the user can
                    // see what a forced removal would destroy.
                    GitWorktreeRemoveStatus::Dirty => {
                        let at = res.at_risk.unwrap_or_default();
                        let mut parts: Vec<String> = Vec::new();
                        if at.modified > 0 {
                            parts.push(format!("{} modified", at.modified));
                        }
                        if at.untracked > 0 {
                            parts.push(format!("{} untracked", at.untracked));
                        }
                        if at.operation_in_progress {
                            parts.push("an operation in progress".to_string());
                        }
                        Effects::toast_detail(
                            format!("{name} has {}", parts.join(", ")),
                            "Force the removal to discard it",
                            ToastKind::Warning,
                        )
                    }
                    GitWorktreeRemoveStatus::IsMain => Effects::toast_detail(
                        "Not a worktree",
                        "That's the repository itself",
                        ToastKind::Warning,
                    ),
                    GitWorktreeRemoveStatus::Locked => Effects::toast_detail(
                        format!("{name} is locked"),
                        "Unlock it first",
                        ToastKind::Warning,
                    ),
                    GitWorktreeRemoveStatus::NotFound => {
                        Effects::toast(format!("No worktree {name}"), ToastKind::Warning)
                    }
                    GitWorktreeRemoveStatus::Refused => Effects::toast_detail(
                        format!("Couldn't remove {name}"),
                        res.message,
                        ToastKind::Warning,
                    ),
                },
            },

            Event::BranchDeleted {
                branch,
                forced,
                result,
            } => match result {
                Err(e) => Effects::error_detail(format!("Couldn't delete {branch}"), e),
                Ok(res) => match res.status {
                    GitDeleteBranchStatus::Deleted => {
                        Effects::toast(format!("Deleted {branch}"), ToastKind::Success)
                    }
                    // Escalate rather than dead-end: the user gets told what they'd lose and
                    // presses Enter to accept it. Not offered on an already-forced attempt —
                    // that would be a loop.
                    GitDeleteBranchStatus::NotMerged if !forced => {
                        self.prompt = Some(Prompt::Confirm {
                            kind: ConfirmKind::DeleteUnmergedBranch {
                                name: branch.clone(),
                            },
                            action: ConfirmAction::DeleteBranch {
                                name: branch,
                                force: true,
                            },
                        });
                        Effects::none()
                    }
                    GitDeleteBranchStatus::NotMerged => {
                        Effects::toast(format!("{branch} isn't merged"), ToastKind::Warning)
                    }
                    GitDeleteBranchStatus::IsCurrentBranch => Effects::toast_detail(
                        format!("{branch} is checked out here"),
                        "Switch away first",
                        ToastKind::Warning,
                    ),
                    GitDeleteBranchStatus::Refused => Effects::toast_detail(
                        format!("Couldn't delete {branch}"),
                        res.message,
                        ToastKind::Warning,
                    ),
                },
            },

            Event::StashDone { staged, result } => match result {
                Ok(r) => {
                    // `(title, body, kind)` — an empty body is no body, as in `HunkApplied`.
                    let (msg, detail, kind) = match r.status {
                        GitStashStatus::Pushed if staged => {
                            ("Stashed staged changes", "", ToastKind::Success)
                        }
                        GitStashStatus::Pushed => ("Stashed working tree", "", ToastKind::Success),
                        GitStashStatus::NothingToStash if staged => {
                            ("Nothing staged to stash", "", ToastKind::Info)
                        }
                        GitStashStatus::NothingToStash => ("Nothing to stash", "", ToastKind::Info),
                        GitStashStatus::Applied => ("Applied stash", "", ToastKind::Success),
                        GitStashStatus::Popped => ("Popped stash", "", ToastKind::Success),
                        GitStashStatus::Dropped => ("Dropped stash", "", ToastKind::Success),
                        GitStashStatus::BlockedByDirtyBuffers => (
                            "Unsaved changes",
                            "Save first, then retry",
                            ToastKind::Warning,
                        ),
                        // The picker was showing a snapshot; something removed the entry since.
                        GitStashStatus::Gone => ("That stash is gone", "", ToastKind::Warning),
                        GitStashStatus::Refused => ("", "", ToastKind::Error),
                        // Names the version, because the fix is entirely outside the editor and
                        // "unsupported" alone would send them looking for a setting.
                        GitStashStatus::StagedUnsupported => {
                            return Effects::toast_detail(
                                "Can't stash only the staged changes",
                                "That needs git 2.35 or newer",
                                ToastKind::Warning,
                            );
                        }
                    };
                    if r.status == GitStashStatus::Refused {
                        return Effects::error_detail("Stash refused", r.message);
                    }
                    Effects::toast_detail(msg, detail, kind)
                }
                Err(e) => Effects::error_detail("Stash failed", e),
            },

            Event::FetchDone(result) => match result {
                Ok(r) => match r.status {
                    // Report the divergence, not the transfer: "fetched" alone leaves the user
                    // looking for what changed, and the counts are the whole reason to fetch.
                    GitFetchStatus::Fetched => {
                        let (title, detail) = fetch_summary(r.upstream.as_ref());
                        Effects::toast_detail(title, detail, ToastKind::Success)
                    }
                    GitFetchStatus::NoRemote => {
                        Effects::toast("No remote configured", ToastKind::Info)
                    }
                    // Acknowledged, not celebrated or mourned: the user asked for this.
                    GitFetchStatus::Cancelled => Effects::toast("Fetch cancelled", ToastKind::Info),
                    GitFetchStatus::Refused => Effects::error_detail("Fetch refused", r.message),
                },
                Err(e) => Effects::error_detail("Fetch failed", e),
            },

            Event::PushDone(result) => match result {
                Ok(r) => match r.status {
                    GitPushStatus::Pushed => {
                        let (title, detail) = push_summary(&r);
                        Effects::toast_detail(title, detail, ToastKind::Success)
                    }
                    GitPushStatus::NothingToPush => {
                        Effects::toast("Nothing to push", ToastKind::Info)
                    }
                    // The one refusal with a next step worth naming. Git's own wording here is
                    // several lines of hint text; what the user needs is the number and the verb.
                    GitPushStatus::Behind => Effects::toast_detail(
                        match r.upstream.as_ref() {
                            Some(u) => format!("{} behind {}", u.behind, u.name),
                            None => "Behind the remote".to_string(),
                        },
                        "Fetch and merge first",
                        ToastKind::Warning,
                    ),
                    GitPushStatus::DetachedHead => Effects::toast_detail(
                        "Not on a branch",
                        "Nothing to push",
                        ToastKind::Warning,
                    ),
                    GitPushStatus::NoRemote => {
                        Effects::toast("No remote configured", ToastKind::Info)
                    }
                    GitPushStatus::AmbiguousRemote => Effects::toast_detail(
                        "Several remotes and no upstream",
                        "Set one with git push -u",
                        ToastKind::Warning,
                    ),
                    GitPushStatus::Cancelled => Effects::toast("Push cancelled", ToastKind::Info),
                    GitPushStatus::Refused => Effects::error_detail("Push refused", r.message),
                },
                Err(e) => Effects::error_detail("Push failed", e),
            },

            Event::PullDone(result) => match result {
                Ok(r) => match r.status {
                    GitPullStatus::UpToDate => Effects::toast(
                        match r.upstream.as_ref() {
                            Some(u) => format!("Already up to date with {}", u.name),
                            None => "Already up to date".to_string(),
                        },
                        ToastKind::Info,
                    ),
                    // The three moves read differently on purpose: the user's local history was
                    // left alone, gained a merge commit, or was rewritten, and which one happened
                    // is the thing they'd otherwise have to go and check.
                    GitPullStatus::FastForwarded
                    | GitPullStatus::Merged
                    | GitPullStatus::Rebased => {
                        let (title, detail) = pull_summary(&r);
                        Effects::toast_detail(title, detail, ToastKind::Success)
                    }
                    // Not an error toast: the files are on disk with markers in them and the next
                    // step is to open one, so name them rather than relaying git's stderr.
                    GitPullStatus::Conflicted => Effects::toast_detail(
                        format!("Conflicts in {}", name_a_few(&r.conflicts)),
                        format!(
                            "Resolve them, then {}",
                            // The follow-up differs by operation and guessing costs the user a
                            // wrong command: a merge is finished by committing, a rebase by
                            // `--continue`. The server read which one stopped, so say it.
                            match r.operation {
                                Some(GitRepoOperation::Rebase) => "continue the rebase",
                                Some(GitRepoOperation::Merge) => "commit the merge",
                                _ => "finish the operation",
                            }
                        ),
                        ToastKind::Warning,
                    ),
                    // The mirror of push's `Behind`, and like it the number and the verb are what
                    // the user needs — git's own answer here is a paragraph of hint text.
                    GitPullStatus::Diverged => Effects::toast_detail(
                        match r.upstream.as_ref() {
                            Some(u) => format!(
                                "Diverged from {} ({} ahead, {} behind)",
                                u.name, u.ahead, u.behind
                            ),
                            None => "Diverged from the remote".to_string(),
                        },
                        "Merge or rebase to reconcile",
                        ToastKind::Warning,
                    ),
                    // The one refusal whose fix is another action in this same sub-leader.
                    GitPullStatus::NoUpstream => Effects::toast_detail(
                        "No upstream",
                        "Push this branch first to set one",
                        ToastKind::Warning,
                    ),
                    GitPullStatus::DetachedHead => Effects::toast_detail(
                        "Not on a branch",
                        "Nothing to pull",
                        ToastKind::Warning,
                    ),
                    GitPullStatus::NoRemote => {
                        Effects::toast("No remote configured", ToastKind::Info)
                    }
                    GitPullStatus::BlockedByDirtyBuffers => Effects::toast_detail(
                        format!("{} unsaved file(s)", r.blocked.len()),
                        "Save first, then retry",
                        ToastKind::Warning,
                    ),
                    // Already stopped part-way through something. Names the operation and the
                    // conflicts still outstanding — the state the user has forgotten they're in,
                    // which is precisely why the pull they just asked for made no sense.
                    GitPullStatus::OperationInProgress => {
                        let what = r
                            .operation
                            .map(|op| op.label().to_string())
                            .unwrap_or_else(|| "an operation".to_string());
                        let next = if r.conflicts.is_empty() {
                            "Finish or abandon it first".to_string()
                        } else {
                            format!("Resolve {} first", name_a_few(&r.conflicts))
                        };
                        Effects::toast_detail(format!("Still {what}"), next, ToastKind::Warning)
                    }
                    // A stranded lock isn't an aside: until it's gone, every git operation in this
                    // repo fails. Say where it is, since removing it is the user's call.
                    GitPullStatus::Cancelled if r.index_locked => Effects::toast_detail(
                        "Pull cancelled",
                        "Remove .git/index.lock before running git again",
                        ToastKind::Warning,
                    ),
                    GitPullStatus::Cancelled => Effects::toast("Pull cancelled", ToastKind::Info),
                    GitPullStatus::Refused => Effects::error_detail("Pull refused", r.message),
                },
                Err(e) => Effects::error_detail("Pull failed", e),
            },

            Event::CancelDone(result) => match result {
                // Nothing to say either way: a successful cancel is followed immediately by the
                // operation's own `Cancelled` result, and "nothing was running" means it beat the
                // keystroke, which is not a failure the user needs telling about.
                Ok(_) => Effects::none(),
                Err(e) => Effects::error_detail("Cancel failed", e),
            },

            Event::HunkApplied {
                action,
                scope,
                result,
            } => match result {
                Ok(r) => {
                    self.view.buffer.cursor = r.cursor;
                    // The wording names what was acted on — "Staged file" after `Space g Alt-s`
                    // and "Staged change" after `Space g s` — because at a glance the toast is the
                    // only confirmation of *how much* just moved into the index.
                    let whole_file = scope == ApplyScope::File;
                    // `(title, body, kind)`: an empty body is no body — most of these outcomes are
                    // a single scannable phrase, and only the refusals have a next step to add.
                    let (msg, detail, kind) = match r.status {
                        ApplyHunkStatus::Staged if whole_file => {
                            ("Staged file", "", ToastKind::Success)
                        }
                        ApplyHunkStatus::Unstaged if whole_file => {
                            ("Unstaged file", "", ToastKind::Success)
                        }
                        ApplyHunkStatus::Reverted if whole_file => {
                            ("Reverted file", "", ToastKind::Success)
                        }
                        ApplyHunkStatus::Staged => ("Staged change", "", ToastKind::Success),
                        ApplyHunkStatus::Unstaged => ("Unstaged change", "", ToastKind::Success),
                        ApplyHunkStatus::Reverted => ("Reverted change", "", ToastKind::Success),
                        // Worded from the action *sent*, which is the only thing that knows which
                        // question was asked: with explicit directions, "no change here" on a
                        // hunk that's sitting there staged would read as a bug.
                        ApplyHunkStatus::NoChange => (
                            match (action, whole_file) {
                                (HunkAction::Stage, false) => "Nothing to stage here",
                                (HunkAction::Unstage, false) => "Nothing to unstage here",
                                (HunkAction::Revert, false) => "No change to revert here",
                                (HunkAction::Stage, true) => "Nothing to stage in this file",
                                (HunkAction::Unstage, true) => "Nothing to unstage in this file",
                                (HunkAction::Revert, true) => "Nothing to revert in this file",
                            },
                            "",
                            ToastKind::Info,
                        ),
                        ApplyHunkStatus::DirtyBuffer => (
                            "Unsaved changes",
                            "Save first, then retry",
                            ToastKind::Warning,
                        ),
                        ApplyHunkStatus::Unavailable => {
                            ("Not in a git repository", "", ToastKind::Info)
                        }
                        // Says where the action *does* live, since the answer is one key away:
                        // `Enter` on the block opens the file it came from.
                        ApplyHunkStatus::NeedsFile => (
                            "Reverting needs the file itself",
                            "Enter opens it from here",
                            ToastKind::Info,
                        ),
                        // Names the cause, not just the refusal: the user set this baseline, and
                        // the way out is to unset it.
                        ApplyHunkStatus::NotAgainstHead => (
                            "Diffing against a revision",
                            "Restore the HEAD baseline to stage",
                            ToastKind::Warning,
                        ),
                        // Deliberately no "do it anyway" escape: marking a conflict resolved is a
                        // decision, and this key is not where it's made.
                        ApplyHunkStatus::Conflicted => {
                            ("Conflicted file", "Resolve it first", ToastKind::Warning)
                        }
                        ApplyHunkStatus::Resolved => ("Marked resolved", "", ToastKind::Success),
                        // The classic way to break a merge, and silent — nothing else would say so.
                        ApplyHunkStatus::MarkersRemain => (
                            "Conflict markers still in this file",
                            "Remove them, then mark it resolved",
                            ToastKind::Warning,
                        ),
                    };
                    Effects::toast_detail(msg, detail, kind)
                }
                Err(e) => Effects::error_detail(
                    match action {
                        HunkAction::Stage => "Staging failed",
                        HunkAction::Unstage => "Unstaging failed",
                        HunkAction::Revert => "Revert failed",
                    },
                    e,
                ),
            },

            Event::ConflictResolved { side, result } => match result {
                Ok(r) => {
                    self.view.buffer.cursor = r.cursor;
                    match r.status {
                        // The side kept and how many are left — the buffer just changed under the
                        // user, and reaching zero is what says the file is done. No instructions:
                        // the keys that do the next step are the ones they just used.
                        ResolveConflictStatus::Resolved => {
                            // Positional, like the keys: "ours" is the word the `<`/`>` bindings
                            // exist to avoid, and during a rebase it names the side that *isn't*
                            // the user's work. The marker order never moves, so this is always
                            // true and always checkable against what's on screen.
                            let kept = match side {
                                ConflictSide::Ours => "the top section",
                                ConflictSide::Theirs => "the bottom section",
                                ConflictSide::Both => "both sections",
                            };
                            let took = if r.resolved == 1 {
                                format!("Kept {kept}")
                            } else {
                                format!("Kept {kept} in {} conflicts", r.resolved)
                            };
                            let left = match r.remaining {
                                0 => "No conflicts left in this file".to_string(),
                                1 => "1 conflict left".to_string(),
                                n => format!("{n} conflicts left"),
                            };
                            Effects::toast_detail(took, left, ToastKind::Success)
                        }
                        // Includes the "this file has no conflicts at all" case: one sentence
                        // answers both.
                        ResolveConflictStatus::NoConflict => {
                            Effects::toast("No conflict here", ToastKind::Info)
                        }
                    }
                }
                Err(e) => Effects::error_detail("Resolve failed", e),
            },

            Event::DiffViewSet { enabled, result } => match result {
                Ok(r) => {
                    self.diff_view = enabled;
                    self.replace_window(r.window);
                    let mut fx = Effects::one(Effect::WindowAdopted);
                    // Grouped so repeated toggling updates one toast in place rather than stacking.
                    fx.push(Effect::Toast {
                        title: format!("Diff {}", if enabled { "on" } else { "off" }),
                        body: None,
                        kind: ToastKind::Info,
                        group: Some("diff".into()),
                    });
                    fx
                }
                Err(e) => Effects::error_detail("Diff toggle failed", e),
            },

            Event::PickerViewed { initial, result } => match result {
                Ok(r) => {
                    let chase_offset = if let Some(p) = &mut self.picker {
                        // The single in-flight refetch slot is free again (Rule 2 below may re-arm
                        // it). Harmless for an initial open, which never set it.
                        p.refetch_in_flight = false;
                        p.offset = r.effective_offset;
                        // Adopt the layout gate first: the centring reveal just below branches on
                        // it, and for a Jumplist it can differ from the kind's default (a capture
                        // from a file-shaped picker renders flat).
                        p.collapsible = r.collapsible;
                        if let Some(center) = r.effective_center_on {
                            p.pending_center = Some(center);
                            // Collapsible centering (cursor-hit opens, file jumps) aligns the
                            // target to the top — its auto-expanded group's header sits just
                            // above and there's context below to read. The headerless kinds
                            // (GitChangesFile) have no header to clear, so minimal it is.
                            p.reveal_on_update = Some(if p.collapsible {
                                Reveal::Top
                            } else {
                                Reveal::Minimal
                            });
                        }
                        p.directory = r.directory_path;
                        p.directory_parent = r.directory_parent;
                        // Jumplist: whether this capture is worth path-scoping — gates the
                        // dir/glob chip chords ([`PickerState::filter_available`]).
                        p.path_filterable = r.path_filterable;
                        if initial && !p.generation_adopted {
                            // Adopt the resumed query (the changes pickers preserve theirs across
                            // opens; every other kind comes back empty) and the persisted filters
                            // (seeded opens get their seed echoed). Skipped when a query keystroke
                            // beat this response: the client's generation (and typed query) already
                            // own the slot — the server adopted them via `picker/query` — and
                            // regressing to the response's snapshot would orphan the query's push.
                            p.generation = r.generation;
                            p.query = r.query;
                            p.total_candidates = r.total_candidates;
                            p.adopt_filters(&r.filters);
                        }
                        p.generation_adopted = true;
                        // Apply the window folded into the response now that generation/offset
                        // are set, so a Grep resume renders its rows even when the redundant
                        // `picker/update` push raced ahead of this response and was discarded.
                        // `apply_update` is generation/offset-guarded — a no-op if it doesn't fit.
                        //
                        // But the folded window is a point-in-time snapshot: a streaming grep
                        // computes it right after the search starts, so it often comes back *empty*.
                        // For a live query (generation matches) the `picker/update` pushes are the
                        // authority and may already have delivered rows — an empty snapshot must not
                        // wipe them (the bug: results blank until you edit the query). Only fold the
                        // window in when it actually carries rows, or we have none yet (resume /
                        // non-streaming kinds, where it's the sole source).
                        let mut reveal = None;
                        if let Some(update) = r.update {
                            let window_has_rows =
                                update.items.as_ref().is_none_or(|it| !it.is_empty());
                            if (p.items.is_empty() || window_has_rows)
                                && p.apply_update(update)
                                && p.pending_center.is_none()
                            {
                                reveal = p.reveal_on_update.take();
                            }
                        }
                        // Rule 2 (trailing chase): while this window was in flight, coalesced moves
                        // (single-flight) may have run the highlight past it. If it landed outside
                        // the window we just loaded, fire ONE more refetch recomputed from the
                        // *current* selection — the window "chases" the highlight one hop per
                        // round-trip until it catches up. Only for selection-driven refetches: a
                        // free pixel scroll (`refetch_chases_selection == false`) deliberately moved
                        // the view away from the selection, so chasing it would fight the scroll
                        // (blank, oscillating scrollbar). Skip while a center is pending (that
                        // repositions the highlight itself) and when the window is empty. Recorded
                        // here and acted on below, once `p`'s borrow has ended.
                        let chase = p.refetch_chases_selection
                            && p.pending_center.is_none()
                            && !p.items.is_empty()
                            && (p.selected < p.offset
                                || p.selected >= p.offset + p.items.len() as u32);
                        if chase {
                            p.selected.saturating_sub(FETCH_LIMIT / 2)
                        } else {
                            return match reveal {
                                Some(reveal) => Effects::one(Effect::RevealPickerSelection(reveal)),
                                None => Effects::none(),
                            };
                        }
                    } else {
                        return Effects::none();
                    };
                    self.picker_refetch(chase_offset, true)
                }
                Err(e) => {
                    self.picker = None;
                    Effects::error_detail("Picker failed", e)
                }
            },

            // Selections open in place: the window shows one buffer, and the one being
            // replaced is a `Space v` away (buffers persist server-side). Opens are
            // transient previews — switching away from one closes it.
            Event::PickerSelected { result: Ok(result) } => match result {
                PickerSelectResult::File { path } => self.open_path_at(path, None, None),
                PickerSelectResult::FileAt {
                    path,
                    position,
                    anchor,
                } => self.open_path_at(path, Some(position), anchor),
                PickerSelectResult::View { view_id } => {
                    if view_id == self.view.view_id {
                        return Effects::none(); // already showing it
                    }
                    self.request_str::<ViewOpen>(
                        ViewOpenParams {
                            view_id: Some(view_id),
                            record_nav_from: Some(self.view.buffer.buffer_id),
                            ..Default::default()
                        },
                        Event::Switched,
                    )
                }

                // The pathless counterpart of `FileAt`: a generated patch was materialised, not
                // loaded, so its rows can only be addressed by the view. Usually the view you're
                // already reading — `Space c` lists that patch's own hunks — which makes this a
                // jump rather than a switch, and no nav-history entry: browser history is for
                // moving *between* files, and the jumplist already covers moving within one.
                // Inside the view already open: focus the element, then land the cursor in it —
                // the same pair, in the same order, that a click uses. Focus first because the
                // server resolves a cursor against whichever element holds it, so setting first
                // would apply the line to a different file; and a cursor in an *unfocused* element
                // is not drawn at all, which is how a jump that resolved and travelled correctly
                // still looked like nothing happening.
                PickerSelectResult::ViewElement {
                    element,
                    buffer_id,
                    position,
                    open,
                } => {
                    self.seat_in_view_element(open.map(|o| *o), element, buffer_id, position, None)
                }
                // The row's view no longer holds it. Shown when it had to be brought back, and
                // said either way — never the row's file in an editor.
                PickerSelectResult::Gone { open } => {
                    let shown = match open {
                        Some(open) => self.adopt_navigation(*open),
                        None => Effects::none(),
                    };
                    shown.and(Effects::toast_grouped(
                        "This entry is no longer in the review",
                        ToastKind::Info,
                        "jumplist",
                    ))
                }
                PickerSelectResult::ViewAt { view_id, position } => self.request_str::<ViewOpen>(
                    ViewOpenParams {
                        view_id: Some(view_id),
                        jump_to: Some(position),
                        record_nav_from: (view_id != self.view.view_id)
                            .then_some(self.view.buffer.buffer_id),
                        ..Default::default()
                    },
                    Event::Switched,
                ),

                PickerSelectResult::Workspace { name } => {
                    // Activate and land on the workspace's last buffer (or a fresh transient
                    // scratch) — the bootstrap convention, now one server-side composite.
                    self.request_str::<WorkspaceActivate>(
                        WorkspaceActivateParams {
                            name,
                            // Unset, not empty: "wherever I was". The server enters this
                            // workspace's most recently activated context, so picking it from the
                            // switcher lands you back in the worktree you were working in rather
                            // than always on the main checkout.
                            worktrees: None,
                            open_last: true,
                        },
                        |r| {
                            Event::WorkspaceActivated(r.and_then(|a| {
                                let opened = a.opened.ok_or_else(|| {
                                    "workspace/activate returned no landing view".to_string()
                                })?;
                                Ok((a.workspace, opened))
                            }))
                        },
                    )
                }
            },
            Event::PickerSelected { result: Err(e), .. } => {
                Effects::error_detail("Select failed", e)
            }

            Event::WorkspaceActivated(Ok((workspace, open))) => {
                self.workspace = workspace.name;
                self.workspace_paths = workspace.paths;
                self.workspace_worktrees = workspace.worktrees;
                self.workspace_projects = workspace.projects;
                // A deliberate switch means we're no longer in the launch context — release the
                // tether, so closing the launched buffer later behaves like any other close (and
                // an ephemeral context reached this way returns to the chooser, not quits).
                self.tether = None;
                // The recall lists are workspace-scoped, so the ones we hold are now the wrong
                // workspace's — not stale, wrong. Refetch before any overlay can read them.
                let fx = self.fetch_history();
                fx.and(self.adopt_switch(open))
            }
            Event::WorktreeBound(Ok(activate)) => {
                let Some(open) = activate.opened else {
                    return Effects::error_detail(
                        "Worktree switch failed",
                        "The server didn't say where to land",
                    );
                };
                self.workspace = activate.workspace.name;
                self.workspace_paths = activate.workspace.paths;
                self.workspace_worktrees = activate.workspace.worktrees;
                self.workspace_projects = activate.workspace.projects;
                self.tether = None;
                // Deliberately no toast for buffers that stayed behind. They are not lost — they
                // are open in the context you left, and the workspace switcher already carries a
                // per-workspace unsaved dot, which answers "where did my edits go" whenever you
                // ask rather than once, in passing, while you are looking at something else.
                self.fetch_history().and(self.adopt_switch(open))
            }
            Event::WorktreeBound(Err(e)) => Effects::error_detail("Worktree switch failed", e),

            Event::WorkspaceActivated(Err(e)) => {
                Effects::error_detail("Workspace switch failed", e)
            }

            Event::WorkspaceCreated(Ok(activate)) => {
                let WorkspaceActivateResult {
                    workspace, opened, ..
                } = activate;
                self.workspace = workspace.name.clone();
                self.workspace_paths = workspace.paths;
                self.workspace_worktrees = workspace.worktrees;
                self.workspace_projects = workspace.projects;
                self.tether = None;
                // Workspace-scoped recall lists — empty for a brand-new workspace, but the fetch
                // is what *clears* the previous workspace's (see `WorkspaceActivated`).
                let mut fx = self.fetch_history();
                fx = fx.and(match opened {
                    // The workspace came with a landing buffer (it had roots / history). Adopt it.
                    Some(open) => self.adopt_switch(open),
                    // A fresh workspace has no roots and so no landing buffer — open a scratch so the
                    // user lands in *some* editor (and the previous workspace's buffer doesn't linger
                    // behind the new workspace). `adopt_switch` leaves the settings overlay open.
                    None => self.request::<ViewOpen>(ViewOpenParams::default(), move |__r| {
                        Event::Switched(__r.map_err(|e| e.message))
                    }),
                });
                fx.push(Effect::Toast {
                    title: format!("Created workspace {}", workspace.name),
                    body: None,
                    kind: ToastKind::Success,
                    group: None,
                });
                // The natural next step for a freshly created (rootless) workspace is adding a root,
                // so — unlike the default open, which focuses the name field — land on the add-root
                // input here.
                let opened = self.open_workspace_settings();
                if let Some(s) = self.workspace_settings.as_mut() {
                    s.selected = s.input_index();
                }
                fx.and(opened)
            }
            Event::WorkspaceCreated(Err(e)) => Effects::error_detail("Create workspace failed", e),

            Event::WorkspaceRenamed(result) => {
                let Some(s) = self.workspace_settings.as_mut() else {
                    return Effects::none();
                };
                match result {
                    Ok(info) => {
                        if self.workspace == s.workspace_name {
                            self.workspace = info.name.clone();
                        }
                        let new_name = info.name.clone();
                        s.workspace_name = info.name.clone();
                        s.name.set(info.name);
                        s.error = None;
                        Effects::toast(
                            format!("Renamed workspace to {new_name}"),
                            ToastKind::Success,
                        )
                    }
                    Err(e) => {
                        s.error = Some(e);
                        Effects::none()
                    }
                }
            }

            Event::WorkspaceRootAdded(result) => {
                match result {
                    Ok(info) => {
                        let name = info.name.clone();
                        self.sync_workspace_info(info);
                        let workspace_paths = self.workspace_paths.clone();
                        if let Some(s) = self.workspace_settings.as_mut() {
                            // Back to the seed rather than to empty, so the next add starts where
                            // the last one did. `path_edited` re-keys the listing to `~/` — without
                            // it the just-added root's entries would keep ghosting into a field
                            // that no longer describes them.
                            s.add.input.set(HOME_PREFIX.to_string());
                            s.add.path_edited(&workspace_paths);
                            s.error = None;
                            // Re-focus the add-root input (now one row further down).
                            s.selected = s.input_index();
                        }
                        let relist = self.refresh_add_root_listing();
                        relist.and(Effects::toast(
                            format!("Added root to {name}"),
                            ToastKind::Success,
                        ))
                    }
                    Err(e) => {
                        if let Some(s) = self.workspace_settings.as_mut() {
                            s.error = Some(e);
                        }
                        Effects::none()
                    }
                }
            }

            Event::WorkspaceProjectAdded(result) | Event::WorkspaceProjectRemoved(result) => {
                // One arm for both: each returns the updated `WorkspaceInfo`, and the overlay
                // reconciles the same way — re-sync, clear the input, keep focus on the add row.
                match result {
                    Ok(info) => {
                        let name = info.name.clone();
                        let count = info.projects.len();
                        self.sync_workspace_info(info);
                        if let Some(s) = self.workspace_settings.as_mut() {
                            s.add_project.input.clear();
                            s.add_project.suggestion_idx = 0;
                            s.add_project_language.clear();
                            s.add_project_language_selected = 0;
                            s.on_add_project_language = false;
                            s.language_inferred = false;
                            s.inference_key = None;
                            s.error = None;
                            s.selected = s.add_project_index();
                        }
                        Effects::toast(
                            format!("{name} now has {count} project(s)"),
                            ToastKind::Success,
                        )
                    }
                    Err(e) => {
                        if let Some(s) = self.workspace_settings.as_mut() {
                            s.error = Some(e);
                        }
                        Effects::none()
                    }
                }
            }

            Event::WorkspaceRootRemoved(result) => match result {
                Ok(r) => {
                    let name = r.workspace.name.clone();
                    let closed = r.closed_buffer_ids.clone();
                    self.sync_workspace_info(r.workspace);
                    if let Some(s) = self.workspace_settings.as_mut() {
                        s.error = None;
                        // Keep the selection in range (the removed row is gone).
                        s.selected = s.selected.min(s.input_index());
                    }
                    let mut fx = Effects::toast_detail(
                        format!("Removed root from {name}"),
                        if closed.is_empty() {
                            String::new()
                        } else {
                            format!("Closed {} file(s)", closed.len())
                        },
                        ToastKind::Success,
                    );
                    // If our current buffer was one of the closed ones, switch to the server-
                    // indicated next buffer (or a fresh scratch).
                    if closed.contains(&self.view.buffer.buffer_id) {
                        fx = fx.and(self.request::<ViewOpen>(
                            ViewOpenParams {
                                view_id: r.next_view_id,
                                // Nothing left to land on: a placeholder, not a scratch to keep.
                                transient: r.next_view_id.is_none().then_some(true),
                                ..Default::default()
                            },
                            move |__r| Event::Switched(__r.map_err(|e| e.message)),
                        ));
                    }
                    fx
                }
                Err(e) => {
                    if let Some(s) = self.workspace_settings.as_mut() {
                        s.error = Some(e);
                        Effects::none()
                    } else {
                        Effects::error_detail("Remove root failed", e)
                    }
                }
            },
            Event::WorkspaceDeleted(result) => match result {
                // The switcher stays open; the refreshed list (sans the deleted row) arrives as a
                // `picker/update` push from the server's `refresh_workspace_pickers`.
                Ok(()) => Effects::toast("Deleted workspace", ToastKind::Success),
                // Covers the active-workspace and dirty-buffer refusals — the server messages are
                // already user-facing.
                Err(e) => Effects::error_detail("Workspace delete failed", e),
            },

            Event::PickerClicked(abs) => {
                if let Some(p) = &mut self.picker {
                    p.selected = abs;
                    // A header-row click is a *disclosure* gesture: toggle the group open or shut
                    // rather than jumping. (Enter on a header is the jump; the mouse path to a
                    // jump is clicking a visible item row.)
                    if let Some(PickerItem::Group {
                        header, expanded, ..
                    }) = p.selected_item()
                    {
                        let (header, expanded) = (header.clone(), *expanded);
                        p.level = PickerLevel::Group;
                        return if expanded {
                            self.picker_collapse_group(header)
                        } else {
                            self.picker_expand_group(header, GroupLanding::Header)
                        };
                    }
                }
                self.picker_accept()
            }

            Event::PickerChipClicked(i) => {
                if let Some(p) = &mut self.picker {
                    p.chip_selected = Some(i);
                }
                Effects::none()
            }

            Event::WorkspaceSettingsRemoveRoot(index) => self.request_remove_root(index),
            Event::WorkspaceSettingsRemoveProject(index) => self.request_remove_project(index),

            Event::AppSettingToggle(index) => self.app_settings_toggle(index),

            Event::AppSettingsLoaded(result) => match result {
                // Apply the persisted settings at boot.
                Ok(settings) => self.apply_app_settings(settings),
                // Non-fatal: keep the defaults already in place. Don't toast at boot.
                Err(_) => Effects::none(),
            },

            Event::HistoryLoaded(result) => {
                if let Ok(snap) = result {
                    self.history.adopt(snap.lists);
                }
                Effects::none()
            }

            Event::HintsStateLoaded(result) => match result {
                Ok(snap) => {
                    self.hints.adopt(snap);
                    // The engine is adopted but clockless (time only reaches it through the tick
                    // entry point) — ask the shell for one immediate tick so the first hint shows
                    // now rather than on the next periodic tick. Unconditional: the tick is
                    // self-gating (hints off / no context → it's a cheap no-op).
                    Effects::one(Effect::HintTickNow)
                }
                // Loud, not silent: with the engine dormant the corner just never appears, which
                // is undebuggable. The one realistic cause is a stale daemon from a dev rebuild
                // (identical version string, so the connect gate lets it through) that predates
                // the hints RPCs — say so, and say the fix.
                Err(_) if self.hints_enabled => Effects::toast_grouped_detail(
                    "Hints unavailable",
                    "Restart the Aether server (ae server stop)",
                    ToastKind::Warning,
                    "hints",
                ),
                Err(_) => Effects::none(),
            },

            Event::AppSettingsSaved(result) => match result {
                Ok(_) => Effects::none(),
                Err(e) => Effects::error_detail("Settings save failed", e),
            },

            Event::PickerChipListing { abs, result } => {
                // Stale responses (the editor moved to another directory, or closed) are
                // dropped by the abs-path staleness key.
                if let Some(ed) = self.picker.as_mut().and_then(|p| p.chip_editor.as_mut()) {
                    if ed.listing_dir_abs == abs {
                        match result {
                            Ok(r) => ed.set_dir_listing(r.entries),
                            // Typed-but-nonexistent segment, or outside the boundary — the
                            // path renders invalid until the next change re-syncs.
                            Err(_) => ed.set_dir_listing_failed(),
                        }
                    }
                }
                // The listing just resolved a held (Pending) preview — apply the now-validated
                // scope, or drop it if the path turned out invalid. No-op for a stale response.
                self.sync_live_filters()
            }

            Event::PathEditorListing { owner, abs, result } => {
                // Stale responses (the editor moved to another directory, or its surface closed)
                // are dropped by the abs-path staleness key. Refreshes only the ghost — none of
                // these surfaces has live results behind it, so there is nothing else to re-run.
                if let Some(ed) = self.path_editor_mut(owner) {
                    if ed.listing_dir_abs == abs {
                        match result {
                            Ok(r) => ed.set_dir_listing(r.entries),
                            Err(_) => ed.set_dir_listing_failed(),
                        }
                    }
                }
                Effects::none()
            }

            Event::AddProjectLanguageInferred { key, language } => {
                if let Some(s) = self.workspace_settings.as_mut() {
                    // Only the *latest* ask may touch the field, and only while the user hasn't:
                    // an inferred value is replaceable, a typed one is theirs.
                    if s.inference_key.as_ref() == Some(&key)
                        && (s.language_inferred || s.add_project_language.text.is_empty())
                    {
                        match language {
                            Some(l) => {
                                if s.add_project_language.text != l {
                                    s.add_project_language = crate::chips::Input::new(l);
                                    s.add_project_language_selected = 0;
                                }
                                s.language_inferred = true;
                            }
                            // Nothing inferred any more — an earlier suggestion goes away with
                            // the directory that produced it.
                            None if s.language_inferred => {
                                s.add_project_language.clear();
                                s.add_project_language_selected = 0;
                                s.language_inferred = false;
                            }
                            None => {}
                        }
                    }
                }
                Effects::none()
            }

            Event::GroupSet(result, landing) => {
                let Some(p) = &mut self.picker else {
                    return Effects::none();
                };
                let run = match result {
                    Err(e) => {
                        // No reshaping push follows a failed gesture — release repeats here.
                        p.group_gesture_in_flight = false;
                        return Effects::error_detail("Group select failed", e);
                    }
                    // `None`: the group re-ranked away under the gesture, or a `step` ran
                    // off the ends (a stop) — nothing to adopt, and no push follows, so
                    // release the repeat guard here too.
                    Ok(None) => {
                        p.group_gesture_in_flight = false;
                        return Effects::none();
                    }
                    // The reshaping push clears the in-flight guard when it's adopted
                    // (`apply_update`) — whichever side of this reply it lands on.
                    Ok(Some(run)) => run,
                };
                // The landing row within the focused run, per the gesture's intent: group
                // navigation and collapses land on the header; expanding into a group, or an
                // item-level spill, enters at the run's first/last item. A collapsed run has no
                // item rows (`len == 0`), so anything asking for one falls back to the header.
                let item_at = |offset: u32| (offset < run.len).then(|| run.header_row + 1 + offset);
                let landed = match landing {
                    GroupLanding::Header => None,
                    GroupLanding::RunStart => item_at(0),
                    GroupLanding::RunEnd => item_at(run.len.saturating_sub(1)),
                    GroupLanding::Keep { offset } => offset.and_then(item_at),
                };
                p.selected = landed.unwrap_or(run.header_row);
                // Stamp the level to match — stored, so a held repeat firing before the
                // reshaping push lands can't misread the new row against the stale run
                // interval (see `PickerLevel`).
                p.level = if landed.is_some() {
                    PickerLevel::Item
                } else {
                    PickerLevel::Group
                };
                // The reshaping push is offset-guarded like any other; when the adopted row
                // sits outside the subscribed window (a group step from deep inside a long
                // run, whose header scrolled off above), the reveal helper chases it with a
                // refetch. Group navigation frames the whole run it lands on (`Run`); everything
                // else reveals minimally — a continuous scan shouldn't yank the view around (and a
                // bottom landing in an over-tall run must stay visible, which the header-capped
                // run framing couldn't guarantee), and `Alt-a` deliberately keeps the view still.
                self.picker_reveal_selection(match landing {
                    // A collapse lands on a header with nothing under it — there's no run to
                    // frame, so it reveals like any other row.
                    GroupLanding::Header if run.len > 0 => Reveal::Run,
                    _ => Reveal::Minimal,
                })
            }
            Event::PathDeleted { noun, result } => match result {
                Err(e) => Effects::error_detail("Delete failed", e),
                Ok(_) => {
                    // Any close of *our* buffer rides the `view/closed` push (it switches us
                    // to the server's successor). Here we just confirm and re-list the picker.
                    let mut fx = Effects::toast(format!("Trashed {noun}"), ToastKind::Success);
                    if let Some(kind) = self.picker.as_ref().map(|p| p.kind) {
                        if kind == PickerKind::Explorer {
                            // Re-list the current directory but keep the query — re-running it
                            // re-reads the dir server-side (the trashed entry drops out) without
                            // resetting where the user was filtering.
                            fx = fx.and(self.picker_query_changed());
                        } else if kind == PickerKind::Files {
                            // Same idea, different mechanism: Files' candidates come from the
                            // workspace index, so re-running the query server-side wouldn't drop
                            // the trashed entry — the list has to be re-bound, which a `Keep`
                            // re-view does (`Arc::ptr_eq` fails against the re-walked index). A
                            // fresh *open* would re-bind too, but it would also wipe the query and
                            // chips, which is a surprising thing for a delete to do. Only the
                            // highlight resets, since the row under it just vanished.
                            if let Some(p) = self.picker.as_mut() {
                                p.selected = 0;
                            }
                            fx = fx
                                .and(Effects::one(Effect::PickerScrollReset))
                                .and(self.picker_refetch(0, false));
                        }
                    }
                    fx
                }
            },
            Event::KeepToggled(result) => match result {
                Err(e) => Effects::error_detail("Keep failed", e),
                // Grouped: toggling keep/release updates one toast rather than stacking a pair.
                Ok(transient) => {
                    // The reply is about the *view's* document. For an ordinary view that is also
                    // the focused buffer and the `buffer/state` push updates it too; for a composed
                    // one the push is about a document the client is not tracking, so this is the
                    // only thing that moves the flag.
                    self.view.view_transient = transient;
                    Effects::toast_grouped(
                        if transient {
                            "View released"
                        } else {
                            "View kept"
                        },
                        ToastKind::Success,
                        "transient",
                    )
                }
            },
            Event::DirCreated(Err(e)) => Effects::error_detail("Create directory failed", e),
            Event::DirCreated(Ok(r)) => {
                let mut fx = Effects::toast(format!("Created {}", r.path), ToastKind::Success);
                // Step into the new directory so the user can keep creating inside it.
                fx = fx.and(self.explorer_navigate(Some(r.path), false, None));
                fx
            }

            Event::ServerPush(n) => self.on_server_push(n),

            Event::ConnectionLost => {
                if self.conn != ConnState::Connected {
                    return Effects::none(); // already reconnecting (a late echo)
                }
                // Results from the dead connection never arrive; drop their mappings
                // rather than toasting a stray error per in-flight call.
                self.pending_rpcs.clear();
                self.conn = ConnState::Reconnecting {
                    attempt: 0,
                    had_unsaved: self.view.unsaved(),
                };
                // Drop out of Insert: edits can't reach the server while down, and a live insert
                // cursor with vanishing keystrokes reads as a freeze. We don't restore it on
                // reconnect (the buffer may have changed under us, or the daemon restarted and lost
                // it) — the user re-enters insert deliberately. A reading view stays a reading
                // view (it's client-rendered; only its refreshes need the server).
                self.view.mode = self.search_return_mode();
                tracing::warn!(buffer = %self.view.buffer.label, "connection lost; reconnecting");
                // Grouped "connection": the matching "Reconnected" toast replaces this one in place.
                let mut fx = Effects::toast_grouped_detail(
                    "Server disconnected",
                    "Reconnecting…",
                    ToastKind::Warning,
                    "connection",
                );
                fx.push(Effect::Reconnect { attempt: 0 });
                fx
            }
            Event::ReconnectRetry => {
                if let ConnState::Reconnecting { attempt, .. } = &mut self.conn {
                    *attempt += 1;
                    let attempt = *attempt;
                    return Effects::one(Effect::Reconnect { attempt });
                }
                Effects::none()
            }
            Event::ReconnectFatal(e) => {
                self.conn = ConnState::Failed;
                Effects::toast_grouped_detail("Reconnect failed", e, ToastKind::Error, "connection")
            }
            Event::Reestablished {
                workspace,
                open,
                restarted,
            } => {
                let had_unsaved = matches!(
                    self.conn,
                    ConnState::Reconnecting {
                        had_unsaved: true,
                        ..
                    }
                );
                tracing::info!(restarted, "reconnected");
                let old_cursor = self.view.buffer.cursor;
                let old_buffer_id = self.view.buffer.buffer_id;
                self.workspace = workspace.name;
                self.workspace_paths = workspace.paths;
                self.workspace_worktrees = workspace.worktrees;
                self.workspace_projects = workspace.projects;
                let same_file = open.path == self.view.buffer.path;
                self.view.rebind(open, &self.workspace_paths);
                // Buffer ids don't survive a daemon restart: remap the tether onto the reopened
                // buffer when it's the same file we were tethered to, else drop it — a stale id
                // could collide with an unrelated new buffer and exit under the user.
                if restarted {
                    self.tether = (same_file && self.tether == Some(old_buffer_id))
                        .then_some(self.view.buffer.buffer_id);
                }
                self.conn = ConnState::Connected;
                // Server-side per-client state died with the old connection; drop the client
                // overlays that fronted it. The frozen window stays rendered until the
                // resubscribe replaces it.
                self.view.viewport_id = None;
                self.view.blame = None;
                // Server-side follow state died with the old connection; forget ours so the
                // post-reconnect sync re-subscribes from scratch.
                self.blame_follow_on = None;
                self.highlight_follow_on = None;
                self.prompt = None;
                self.picker = None;
                let buffer_id = self.view.buffer.buffer_id;
                let mut fx = Effects::one(Effect::Resubscribe);
                // Restore a selection (jump_to only carried the cursor): same buffer only,
                // and a failure (the file shrank on disk) keeps the server's default.
                if same_file && old_cursor.anchor != old_cursor.position {
                    fx = fx.and(self.request::<CursorSet>(
                        CursorSetParams {
                            buffer_id,
                            position: old_cursor.position,
                            anchor: old_cursor.anchor,
                            granularity: Granularity::Char,
                        },
                        move |__r| match __r {
                            Ok(c) => Event::CursorMsg(Ok(c)),
                            Err(_) => Event::Noop,
                        },
                    ));
                }
                // Re-prime a committed search so highlights and `n` survive the drop.
                if same_file && self.view.search.active && !self.view.search.query.is_empty() {
                    fx = fx.and(self.request::<SearchSet>(
                        SearchSetParams {
                            buffer_id,
                            query: self.view.search.query.clone(),
                            anchor: None,
                            extend: false,
                            from_selection: false,
                            options: self.view.search.options,
                        },
                        move |__r| Event::SearchRestored(__r.map_err(|e| e.message)),
                    ));
                }
                fx.push(if restarted && had_unsaved {
                    Effect::Toast {
                        title: "Reconnected".into(),
                        body: Some("The server restarted, so unsaved changes were lost".into()),
                        kind: ToastKind::Warning,
                        group: Some("connection".into()),
                    }
                } else {
                    Effect::Toast {
                        title: "Reconnected".into(),
                        body: None,
                        kind: ToastKind::Success,
                        group: Some("connection".into()),
                    }
                });
                fx
            }

            Event::Noop => Effects::none(),
            Event::SaveTried(Ok(SaveTry::Saved {
                result,
                target,
                after,
            })) => {
                self.view.buffer.revision = result.revision;
                self.view.buffer.saved_revision = result.revision;
                self.view.view_transient = false; // saving promotes the view it was made in
                self.view.externally_modified = false;
                self.view.externally_deleted = false;
                let note = match target {
                    Some((path_index, rel)) => {
                        // Save-as: the buffer's identity changed — adopt the new path/label. The
                        // label takes the same canonical `"[root]: [path]"` form as buffer-open, so
                        // a renamed buffer reads identically in the status bar, title, and picker.
                        let root = self.workspace_paths.get(path_index as usize);
                        self.view.buffer.path =
                            root.map(|r| format!("{}/{rel}", r.trim_end_matches('/')));
                        let label = crate::labels::root_relative_display(
                            &self.workspace_paths,
                            path_index,
                            &rel,
                        );
                        self.view.relabel_focused(label);
                        format!("Saved as {rel} (rev {})", result.revision)
                    }
                    None => format!("Saved (rev {})", result.revision),
                };
                // A save that exists only to feed a commit isn't news — the commit's own toast is
                // the outcome, and stacking both makes one gesture look like two.
                let feeds_commit = after == AfterSave::Close
                    && self
                        .pending_commit
                        .as_ref()
                        .is_some_and(|p| p.buffer_id == self.view.buffer.buffer_id);
                let mut fx = if feeds_commit {
                    Effects::none()
                } else {
                    Effects::toast(note, ToastKind::Success)
                };
                match after {
                    AfterSave::Nothing => {}
                    // Save-and-quit (`Space Alt-q`): the save landed, so close the window — the
                    // server drops per-client state on disconnect, so this is exactly `Space q`.
                    AfterSave::Quit => fx.push(Effect::Exit),
                    // Save-and-close (`Space Alt-x`): the buffer is clean now, so this close
                    // never re-prompts — and when the buffer is the tether, it exits the client.
                    AfterSave::Close => fx = fx.and(self.close_view()),
                }
                fx
            }
            // A view save: N documents, so the news is how many rather than a revision. The focused
            // document's own result still updates the buffer state the client tracks, when it was
            // one of them — a save that wrote only *other* elements leaves this one as it was.
            Event::SaveTried(Ok(SaveTry::SavedView { result, after })) => {
                let note = match result.saved {
                    0 => "Nothing to save".to_string(),
                    1 => "Saved".to_string(),
                    n => format!("Saved {n} files"),
                };
                let kind = if result.saved == 0 {
                    ToastKind::Info
                } else {
                    ToastKind::Success
                };
                // A save that exists only to feed a commit isn't news — the commit's own toast is
                // the outcome, and stacking both makes one gesture look like two. Same rule the
                // single-document arm follows.
                let feeds_commit = after == AfterSave::Close
                    && self
                        .pending_commit
                        .as_ref()
                        .is_some_and(|p| p.buffer_id == self.view.buffer.buffer_id);
                let mut fx = if feeds_commit {
                    Effects::none()
                } else {
                    Effects::toast(note, kind)
                };
                match after {
                    AfterSave::Nothing => {}
                    AfterSave::Quit => fx.push(Effect::Exit),
                    AfterSave::Close => fx = fx.and(self.close_view()),
                }
                fx
            }
            Event::SaveTried(Ok(SaveTry::NeedsConfirm { kind, action })) => {
                self.prompt = Some(Prompt::Confirm { kind, action });
                Effects::none()
            }
            Event::SaveTried(Err(e)) => Effects::error_detail("Save failed", e),

            Event::ReloadTried(Ok(ReloadTry::Reloaded(r))) => {
                self.view.buffer.revision = r.revision;
                self.view.buffer.saved_revision = r.revision;
                self.view.view_transient = false; // reloading promotes, like save
                self.view.externally_modified = false;
                self.view.externally_deleted = false;
                Effects::toast(format!("Reloaded (rev {})", r.revision), ToastKind::Success)
            }
            Event::ReloadTried(Ok(ReloadTry::NeedsConfirm)) => {
                self.prompt = Some(Prompt::Confirm {
                    kind: ConfirmKind::DiscardOnReload,
                    action: ConfirmAction::ReloadDiscard,
                });
                Effects::none()
            }
            Event::ReloadTried(Err(e)) => Effects::error_detail("Reload failed", e),
        }
    }

    /// `buffer/save`, mapping the server's refusal codes to a `[y/N]` confirmation that
    /// retries with `overwrite: true`. `target` is the save-as `(path_index, relative_path)`.
    pub fn save(
        &mut self,
        target: Option<(u32, String)>,
        overwrite: bool,
        after: AfterSave,
    ) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        let (path_index, relative_path) = match &target {
            Some((i, p)) => (Some(*i), Some(p.clone())),
            None => (None, None),
        };

        // A plain save saves the **view**: every document its elements window, which for an
        // ordinary view is the one document and so is unchanged. Save-*as* stays per-document —
        // it names a path, and a path names one file.
        if target.is_none() {
            let view_id = self.view.view_id;
            {
                return self.request::<ViewSave>(
                    ViewSaveParams { view_id, overwrite },
                    move |__r| {
                        Event::SaveTried(match __r {
                            Ok(result) => Ok(SaveTry::SavedView { result, after }),
                            Err(e) if e.code == ErrorCode::WOULD_OVERWRITE.code() => {
                                Ok(SaveTry::NeedsConfirm {
                                    kind: ConfirmKind::Overwrite { path: None },
                                    action: ConfirmAction::Save {
                                        target: None,
                                        after,
                                    },
                                })
                            }
                            Err(e) if e.code == ErrorCode::EXTERNALLY_MODIFIED.code() => {
                                Ok(SaveTry::NeedsConfirm {
                                    kind: ConfirmKind::OverwriteModified,
                                    action: ConfirmAction::Save {
                                        target: None,
                                        after,
                                    },
                                })
                            }
                            Err(e) if e.code == ErrorCode::EXTERNALLY_DELETED.code() => {
                                Ok(SaveTry::NeedsConfirm {
                                    kind: ConfirmKind::RecreateDeleted,
                                    action: ConfirmAction::Save {
                                        target: None,
                                        after,
                                    },
                                })
                            }
                            Err(e) => Err(e.message),
                        })
                    },
                );
            }
        }

        self.request::<BufferSave>(
            BufferSaveParams {
                buffer_id,
                path_index,
                relative_path,
                overwrite,
            },
            move |__r| {
                Event::SaveTried(match __r {
                    Ok(result) => Ok(SaveTry::Saved {
                        result,
                        target,
                        after,
                    }),
                    Err(e) if e.code == ErrorCode::WOULD_OVERWRITE.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            kind: ConfirmKind::Overwrite {
                                path: target.as_ref().map(|(_, p)| p.clone()),
                            },
                            action: ConfirmAction::Save { target, after },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_MODIFIED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            kind: ConfirmKind::OverwriteModified,
                            action: ConfirmAction::Save { target, after },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_DELETED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            kind: ConfirmKind::RecreateDeleted,
                            action: ConfirmAction::Save { target, after },
                        })
                    }
                    Err(e) => Err(e.message),
                })
            },
        )
    }

    /// Fire an edit RPC; the result lands as [`Event::EditDone`]. Allocate a token, park the result
    /// mapping, and emit `Effect::Request` — the sans-IO replacement for spawning an RPC future.
    /// The shell performs the call and feeds the outcome back through [`Session::on_rpc_result`].
    fn request<M>(
        &mut self,
        params: M::Params,
        f: impl FnOnce(Result<M::Result, RpcError>) -> Event + Send + 'static,
    ) -> Effects
    where
        M: RpcMethod + 'static,
    {
        // A read-only buffer declines every text-changing method here rather than paying a round
        // trip to be refused — that's what keeps a held key quiet instead of streaming errors.
        // The property is declared on the method (`RpcMethod::MUTATES_TEXT`), not remembered at
        // each call site, so a method added later is covered by the funnel it already goes
        // through. The server is still the authority: `ServerState::editable_doc` refuses these
        // for real, whatever a client believes.
        if M::MUTATES_TEXT && self.view.buffer.read_only {
            return crate::session::read_only_toast();
        }
        // The socket is down: drop the request rather than parking a mapping that can never
        // resolve (and would fire stale on reconnect). The reconnect path re-subscribes from
        // scratch, so nothing is lost by not queuing here. This is the single place the
        // connection state gates server I/O — callers run their client-side logic regardless.
        if self.conn != ConnState::Connected {
            return Effects::none();
        }
        let token = self.next_token;
        self.next_token += 1;
        self.pending_rpcs.insert(
            token,
            Box::new(move |r| {
                f(r.and_then(|v| {
                    serde_json::from_value(v).map_err(|e| RpcError {
                        method: M::NAME,
                        code: 0,
                        message: format!("malformed result: {e}"),
                    })
                }))
            }),
        );
        Effects::one(Effect::Request {
            token,
            method: M::NAME,
            params: serde_json::to_value(params).expect("params serialize"),
        })
    }

    /// [`Session::request`] with the error stringified — the shape most events carry.
    fn request_str<M>(
        &mut self,
        params: M::Params,
        f: impl FnOnce(Result<M::Result, String>) -> Event + Send + 'static,
    ) -> Effects
    where
        M: RpcMethod + 'static,
    {
        // The server's *message*, not the stringified `RpcError` — its Display carries an "RPC
        // {method} returned error {code}: " prefix that reads as machine noise in the toast body
        // this string usually lands in. The method and code stay on the error itself for logging.
        self.request::<M>(params, move |r| f(r.map_err(|e| e.message)))
    }

    /// An RPC outcome arriving from the shell: run the parked mapping and process the
    /// event it builds. Unknown tokens are ignored (the pending set is cleared on
    /// connection loss; a late result from the old connection has nothing to say).
    pub fn on_rpc_result(
        &mut self,
        token: u64,
        result: Result<serde_json::Value, RpcError>,
    ) -> Effects {
        let Some(f) = self.pending_rpcs.remove(&token) else {
            return Effects::none();
        };
        // A request naming a buffer the server no longer has is **stale by definition**, and the
        // server is the authority on buffer lifetime: it has already closed that buffer and either
        // has told us (`view/closed`) or is about to. Anything we had in flight over it — a
        // viewport scroll, an edit, a status refresh — is about a buffer that stopped existing
        // mid-round-trip.
        //
        // Another client rebinding a worktree is what makes this ordinary rather than exotic: the
        // clean file-backed buffers under the roots that moved all close at once, under a client
        // that may be typing into one of them, and each in-flight request came back as its own
        // error toast.
        //
        // **Only the toast is noise; the callback still runs.** Dropping the callback too — as this
        // did — quietly took every state-clearing continuation with it. A `git/worktree_add` whose
        // `buffer_id` had just been closed never reached its result arm; a dropped `Event::Switched`
        // left the client rendering a buffer id the server had already closed; a `buffer/save` did
        // nothing at all, silently. So the error is delivered normally and only the resulting error
        // toast is stripped.
        let stale_buffer = result
            .as_ref()
            .err()
            .is_some_and(|e| e.code == ErrorCode::BUFFER_NOT_FOUND.0);
        // A git verb pressed where there's no repo to act on — a scratch buffer, or a file outside
        // one. Not a failure, just an answer: the command had nowhere to go and the message says
        // what would give it one. Errors pin until Esc, which is far too heavy for something you
        // resolve by pressing the key again somewhere else, so this one fades like a warning.
        // Downgraded here rather than at ~15 call sites because every git verb's error arm builds
        // its toast the same way, and a new one should inherit this without being told.
        let no_repo = result
            .as_ref()
            .err()
            .is_some_and(|e| e.code == ErrorCode::REPO_NOT_FOUND.0);
        let event = f(result);
        let mut effects = self.on_event(event);
        if stale_buffer {
            effects.0.retain(|e| {
                !matches!(
                    e,
                    Effect::Toast {
                        kind: ToastKind::Error,
                        ..
                    }
                )
            });
        }
        if no_repo {
            for e in &mut effects.0 {
                if let Effect::Toast { kind, .. } = e {
                    if *kind == ToastKind::Error {
                        *kind = ToastKind::Warning;
                    }
                }
            }
        }
        effects
    }

    /// Send an edit RPC: `request_str` with the shared `EditDone` continuation. The read-only
    /// refusal is *not* here — it lives in `request`, keyed off `RpcMethod::MUTATES_TEXT`, so it
    /// also covers the mutating methods this signature can't express (`buffer/cut` and
    /// `lsp/format` return their own result types) and the ones that reach the wire by another
    /// route.
    pub fn edit<M>(&mut self, params: M::Params) -> Effects
    where
        M: RpcMethod<Result = EditResult> + 'static,
    {
        self.request_str::<M>(params, Event::EditDone)
    }

    /// Insert clipboard text per the paste gesture (each one server-side edit; `Before`
    /// collapses to the selection start via `at` on the way in).
    pub fn paste(&mut self, kind: PasteKind, text: String) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        match kind {
            PasteKind::Before { count } => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: text.repeat(count.max(1) as usize),
                select_pasted: true,
                replace_selection: false,
                // Insert at the selection start — the collapse rides the edit instead of a prior
                // cursor/set.
                at: Some(SelectionEdge::Start),
            }),
            PasteKind::Replace { count } => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: text.repeat(count.max(1) as usize),
                select_pasted: true,
                // A point cursor is the 1-char selection under the Normal-mode block, so
                // replace-paste must swallow it too — without this the server treats the
                // point as a caret and pure-inserts before the char.
                replace_selection: true,
                at: None,
            }),
            PasteKind::AtCursor => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text,
                select_pasted: false,
                replace_selection: false,
                at: None,
            }),
            PasteKind::Line => {
                self.edit::<InputReplaceLine>(InputReplaceLineParams { buffer_id, text })
            }
            PasteKind::Block { replace } => self.request_str::<InputPasteBlock>(
                PasteBlockParams {
                    buffer_id,
                    text,
                    replace,
                },
                Event::BlockEditDone,
            ),
        }
    }

    /// A shell-delivered paste gesture over the buffer — the TUI's terminal bracketed paste
    /// (later, browser paste events). The whole point is that pasted bytes are *text*, never
    /// keystrokes: replayed as keys, a Normal-mode paste runs as commands and an Insert-mode one
    /// auto-indents at every newline. Routed by mode like the explicit paste chords — Insert
    /// inserts at the caret, Normal pastes before the selection. A paste while an overlay input is
    /// focused never reaches this (the shell's own editor takes it); any other keyboard-owning
    /// surface (prompt, picker, settings overlay, sneak, Search, read-only Read) drops it.
    pub fn paste_text(&mut self, text: String) -> Effects {
        // Terminals differ on pasted line endings (some translate LF to CR so apps see "Enter"):
        // normalize to `\n`, then filter the remaining control chars as typed input would be.
        let text: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        if text.is_empty()
            || self.prompt.is_some()
            || self.picker.is_some()
            || self.workspace_settings.is_some()
            || self.app_settings.is_some()
            || self.view.sneak.is_some()
        {
            return Effects::none();
        }
        match self.view.mode {
            Mode::Insert => self.paste(PasteKind::AtCursor, text),
            Mode::Normal => self.paste(PasteKind::Before { count: 1 }, text),
            Mode::Search | Mode::Read => Effects::none(),
        }
    }

    /// Insert literal text at the cursor — an IME composition commit (or any shell-supplied text).
    /// Insert mode only: composed text is editing input, not a command. Same edit as a typed key
    /// (no `select_pasted`), so multi-character composed strings land like normal typing.
    pub fn insert_text(&mut self, text: String) -> Effects {
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        if self.view.mode != Mode::Insert || text.is_empty() {
            return Effects::none();
        }
        self.edit::<InputText>(InputTextParams {
            buffer_id: self.view.buffer.buffer_id,
            text,
            select_pasted: false,
            replace_selection: false,
            at: None,
        })
    }

    /// Flip soft-wrap on/off. The wrap mode is core state (it rides every `viewport/subscribe`), but
    /// re-rendering the viewport at the new wrap is geometry, so the shell follows this with a
    /// `viewport/set_wrap`. The native shells write `Session.wrap` directly (they own the struct);
    /// the wasm web shell can't, so it calls this. Returns no effects — pure state.
    pub fn toggle_wrap(&mut self) -> Effects {
        self.wrap = match self.wrap {
            WrapMode::Soft => WrapMode::None,
            WrapMode::None => WrapMode::Soft,
        };
        Effects::none()
    }

    /// Land the cursor on a target reached by a same-buffer jump — go-to-line, search `n`/`N`, or
    /// the same-buffer branch of a navigation — and reveal it with a `Jump` scroll: short hops
    /// glide, far ones snap (the shell decides which). The one primitive for *how* an in-file jump
    /// scrolls into view, so every jump-style motion frames its target identically.
    pub fn jump_to_cursor(&mut self, cursor: CursorState) -> Effects {
        self.view.buffer.cursor = cursor;
        Effects::one(Effect::RevealCursor(RevealStyle::Jump))
    }

    /// [`Self::jump_to_cursor`] for a *stepping* motion (next/prev diagnostic or hunk), which can
    /// run out of places to go: reveal the cursor as a jump, but when the step found nowhere new
    /// (`moved == false`) toast `exhausted` instead of silently re-revealing the same spot.
    pub fn step_to_cursor(&mut self, cursor: CursorState, moved: bool, exhausted: &str) -> Effects {
        self.view.buffer.cursor = cursor;
        let mut fx = if moved {
            Effects::none()
        } else {
            // Grouped so repeatedly stepping with nowhere left to go coalesces to one toast.
            Effects::toast_grouped(exhausted, ToastKind::Info, "step-nav")
        };
        fx.push(Effect::RevealCursor(RevealStyle::Jump));
        fx
    }

    /// Land on a buffer an open answered with — the common tail of every `Switched`-shaped result.
    fn adopt_open(&mut self, open: ViewOpenResult) -> Effects {
        // A commit prepared just before this open has been waiting for its buffer id (the open is
        // what mints it). Anything else clears the wait: the user navigated away instead, so there
        // is no commit buffer to confirm.
        if let Some(pending) = self.pending_commit.as_mut() {
            if pending.buffer_id == 0 {
                pending.buffer_id = open.buffer_id;
                pending.view_id = open.view_id;
            }
        }
        self.adopt_navigation(open)
    }

    /// An open that failed, from whichever RPC was asked to do it.
    fn open_failed(&mut self, e: String) -> Effects {
        Effects::error_detail("Open failed", e)
    }

    /// Adopt the result of a navigation that moves the cursor and *may* land in the buffer we're
    /// already on (goto-definition, a picker / explorer open, a grep hit, nav-history back/forward).
    ///
    /// A hit in the SAME buffer is a move, not a switch: keep the window / viewport / diagnostics
    /// and just reposition the cursor, letting the shell reveal it with a `Jump` scroll — short
    /// hops glide, far ones snap. Resubscribing would replace the whole window (reading as an
    /// instant jump) and, for a nav-history step, reinstate the *saved* scroll that predates the
    /// jump, stranding the cursor off-screen. A hit in a DIFFERENT buffer is a real switch
    /// ([`Self::adopt_switch`]). One definition so every cursor-moving navigation scrolls its
    /// target into view the same way; genuine buffer switches (close, new-scratch, workspace change)
    /// always land on a different `buffer_id`, so routing them here is just a switch.
    pub fn adopt_navigation(&mut self, open: ViewOpenResult) -> Effects {
        if open.buffer_id != self.view.buffer.buffer_id {
            return self.adopt_switch(open);
        }
        // The same file through its *other* view — the picker's editor row chosen while the
        // reader is on screen — is the sibling, not a move within this one: the window has to
        // change even though the buffer, and the cursor in it, do not.
        if open.view_id != self.view.view_id {
            return self.adopt_sibling(open);
        }
        if self.pending_read_anchor.is_some() && self.view.read.is_some() {
            // `[x](./this-file.md#section)`: the target is the document already on
            // screen, so the anchor resolves against the live parse — no refetch fires.
            return self.consume_read_anchor();
        }
        self.pending_read_anchor = None;
        self.jump_to_cursor(open.cursor)
    }

    /// Rebind the session to a freshly opened buffer: reset all per-buffer state (modal,
    /// diagnostics, viewport binding, prompt — an externally-triggered switch can land mid-pick)
    /// and ask the shell to resubscribe. Input history is workspace-scoped, not per-buffer, so it
    /// lives on the session ([`Session::history`]) and this doesn't touch it.
    ///
    /// An open picker is deliberately *not* torn down here — rebinding the buffer doesn't own the
    /// picker's lifecycle. The pick→open path closes its own picker explicitly ([`Self::picker_accept`]),
    /// and a picker-initiated close of the active buffer wants the list kept open. A buffer-scoped
    /// picker (outline, diagnostics) that should dismiss on a buffer change is the picker's own call,
    /// not a side effect of the switch.
    pub fn adopt_switch(&mut self, open: ViewOpenResult) -> Effects {
        // A sneak session is keyed `(client, buffer)` *server-side*, so dropping this side of it
        // would leave the outgoing buffer's labels live: switch away mid-sneak and back, and they
        // render again with nothing here thinking it is sneaking. Cancel before the swap, while the
        // buffer they belong to is still the one we can name.
        let sneak_fx = self.cancel_sneak_on(self.view.buffer.buffer_id);
        // One assignment, where this was sixteen hand-written resets that nothing checked. Anything
        // that must *survive* a switch — the sticky presentation preferences, the mirrors of
        // server-side follows — lives on the session and is untouched here by construction.
        self.view = ViewState::from_open(open, &self.workspace_paths);
        // Not view state, but bound to whatever was on screen when it opened, so a switch dismisses
        // it: a modal prompt is the session's, and it has nowhere to return to.
        self.prompt = None;
        self.sync_read_anchor_on_switch();
        sneak_fx.and(Effects::one(Effect::Resubscribe))
    }

    /// Clear an active sneak session on `buffer_id`, both halves. No-op when not sneaking.
    ///
    /// Split out because three paths leave a session behind — a buffer switch, entering the reading
    /// view, and `Esc` — and only the last of them used to tell the server, which is what left
    /// labels rendering on a buffer nothing was sneaking in.
    fn cancel_sneak_on(&mut self, buffer_id: BufferId) -> Effects {
        if self.view.sneak.take().is_none() {
            return Effects::none();
        }
        self.request::<SneakCancel>(SneakCancelParams { buffer_id }, |_r| Event::Noop)
    }

    /// A freshly adopted view starts without a reading view: whether it is one is its window's
    /// to say ([`Self::sync_read_presentation`]). A pending cross-file anchor can only land in the
    /// reading view of a markdown file, so a switch to anything else drops it.
    fn sync_read_anchor_on_switch(&mut self) {
        self.view.read = None;
        if self.view.buffer.language.as_deref() != Some("markdown") {
            self.pending_read_anchor = None;
        }
    }

    /// Ask for the current view's sibling — its file's editor, or its reader. Named as this view
    /// plus the other kind: the server finds its buffer's view of that kind or makes one and
    /// answers with it; [`Self::adopt_sibling`] takes it up. The content anchor captured first
    /// keeps the same lines on screen when the sibling has no scroll of its own to come back to.
    fn open_sibling(&mut self, kind: aether_protocol::ui::ViewKind) -> Effects {
        Effects::one(Effect::SaveContentAnchor).and(self.request_str::<ViewOpen>(
            ViewOpenParams {
                view_id: Some(self.view.view_id),
                kind: Some(kind),
                ..Default::default()
            },
            Event::SiblingOpened,
        ))
    }

    /// Adopt the sibling view an [`Self::open_sibling`] answered with: the same buffer, another
    /// view of it. Not a switch — the cursor is the buffer's and shared by both views, and a mode
    /// an edit transition set stays set — and not a same-buffer move either, which would keep
    /// the window: the view is new, so the shell re-subscribes, at the view's own remembered
    /// scroll when it has one, else where the content anchor says. The reading view, if this was
    /// one, goes: the new window decides whether the sibling is.
    fn adopt_sibling(&mut self, open: ViewOpenResult) -> Effects {
        if open.buffer_id != self.view.buffer.buffer_id {
            return self.adopt_switch(open);
        }
        let sneak_fx = self.cancel_sneak_on(self.view.buffer.buffer_id);
        // The whole rebind, not the fields a sibling happens to differ in. Copying its id and
        // scroll off the open one by one left the kept flag behind: the status bar said the
        // editor was kept because the reader had been, the picker said it was not, and the first
        // `Space k` "released" a view that was never kept.
        let remembered_scroll = open.scroll.is_some();
        self.view.rebind(open, &self.workspace_paths);
        if remembered_scroll {
            self.forget_scroll_anchor();
        }
        self.view.read = None;
        if self.view.mode == Mode::Read {
            self.view.mode = Mode::Normal;
        }
        sneak_fx.and(Effects::one(Effect::Resubscribe))
    }

    /// An edit of our own changed the text under the reading view's parse, and its new cursor is
    /// adopted from the response a round trip before the re-parse lands in the pushed window.
    /// Deriving focus against the old parse meanwhile painted the bar on whatever block happened
    /// to sit at those bytes in the *previous* document — a flash on an unrelated block — so the
    /// parse is marked stale (`loading`) until the window re-parses it. Not when the window got
    /// here first: a parse already at the edit's revision is the new document.
    fn mark_read_stale(&mut self, revision: u64) {
        if let Some(read) = self.view.read.as_mut() {
            if read.revision != revision {
                read.loading = true;
            }
        }
    }

    /// The reading view is a consequence of the window, and this is where the client notices.
    ///
    /// The server presents a markdown file as the reader by sending it as one element the client
    /// lays out — unwrapped, whole, one wire row per line. Every window adoption comes through
    /// here: an element of that kind under the cursor puts the session in the reading view over
    /// the lines it carries, re-parsing whenever they change (an edit, an undo, another client's
    /// change — all of them arrive as a pushed window, so nothing is fetched); an ordinary editor
    /// takes the reading view down. Nothing else decides which view is showing: `Space u` and the
    /// edit transitions only *ask*, through the subscribe, and adopt whatever comes back.
    ///
    /// A partial load — the element has more lines than the window carries — is a window still
    /// on its way (the server loads such an element whole, and the grid asks for all of it), so
    /// the view stays loading rather than parsing half a document.
    fn sync_read_presentation(&mut self) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        let prose = self.view.window.as_ref().and_then(|w| {
            // **The reader is a whole view, not an element.** Rendered prose elsewhere — an
            // agent's reply inside a conversation — is an `Element::Prose` and never matches the
            // arm below; this guard is the second half of the same rule, and the test is the one
            // `View::kind` uses server-side: one element, and nothing else in the view. Without it
            // a composed view holding a single client-laid-out editor would be adopted as the
            // reader, which replaced a whole conversation with a reading view over one block.
            if w.root.editors().len() != 1 {
                return None;
            }
            w.root.editors().into_iter().find_map(|node| match node {
                Element::Editor {
                    element,
                    laid_out_by: aether_protocol::ui::LayoutOwner::Client,
                    rows,
                    first_row,
                    lines,
                    ..
                } if *element == self.view.focused_element => {
                    let whole = first_row.get() == 0 && lines.len() as u32 == *rows;
                    let text = lines
                        .iter()
                        .map(|l| {
                            l.visual_rows
                                .iter()
                                .flat_map(|r| r.segments.iter().map(|s| s.text.as_str()))
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some((whole, text))
                }
                _ => None,
            })
        });
        let Some((whole, text)) = prose else {
            // The editor. A pending cross-file anchor could only have landed in a reading view.
            self.pending_read_anchor = None;
            if self.view.read.take().is_some() && self.view.mode == Mode::Read {
                self.view.mode = Mode::Normal;
            }
            return Effects::none();
        };
        let mut fx = Effects::none();
        if self.view.read.is_none() {
            self.view.mode = Mode::Read;
            self.view.pending = Pending::None;
            self.view.count = None;
            fx = fx.and(self.cancel_sneak_on(buffer_id));
            self.view.read = Some(ReadView::loading(buffer_id));
        }
        if !whole {
            return fx;
        }
        let revision = self.view.buffer.revision;
        let read = self.view.read.as_mut().expect("installed above");
        if !read.loading && read.text == text {
            return fx; // the push was about something other than the text
        }
        // A followed cross-file anchor is pending: stage the parse instead of installing it —
        // the document paints once, already in place.
        if self.pending_read_anchor.is_some() {
            let mut staged = ReadView::loading(buffer_id);
            staged.adopt(revision, text);
            return fx.and(self.stage_read_place(staged));
        }
        read.adopt(revision, text);
        fx.and(self.read_fence_requests())
    }

    /// Ask the server to highlight every fenced code block of the freshly parsed document —
    /// tree-sitter lives server-side, so this is the reading view's route to editor-grade code
    /// colour. One request per fence; results adopt via [`Event::ReadHighlights`] and paint in as
    /// they land.
    fn read_fence_requests(&mut self) -> Effects {
        let fences = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let mut fences = crate::markdown::fenced_code_blocks(&read.blocks);
            // Sanity cap — no real document has hundreds of fences, but a pathological one
            // shouldn't turn into a request storm.
            fences.truncate(200);
            fences
        };
        let (buffer_id, revision) = {
            let read = self.view.read.as_ref().expect("checked above");
            (read.buffer_id, read.revision)
        };
        let mut fx = Effects::none();
        for (span, language, text) in fences {
            let block_start = span.start;
            fx = fx.and(self.request_str::<SyntaxHighlightSnippet>(
                SyntaxHighlightSnippetParams { language, text },
                move |result| Event::ReadHighlights {
                    buffer_id,
                    revision,
                    block_start,
                    result,
                },
            ));
        }
        fx
    }

    /// React to a change signal for `buffer_id` at `revision`: when the reading view shows that
    /// buffer at an older revision, re-fetch.
    /// Move the live cursor to another editor element of this view.
    ///
    /// A no-op without a viewport: focus is a property of a *presentation*, and there is nothing to
    /// step through before the first window arrives.
    fn focus_element(&mut self, target: FocusTarget) -> Effects {
        let Some(viewport_id) = self.view.viewport_id else {
            return Effects::none();
        };
        self.request_str::<ViewportFocusElement>(
            ViewportFocusElementParams {
                viewport_id,
                target,
            },
            Event::ElementFocused,
        )
    }

    /// Step the view — to its next change, or its next outline entry. A no-op without a viewport,
    /// as focus is: there is nothing to step through before the first window arrives.
    fn step_view(
        &mut self,
        forward: bool,
        count: u32,
        extend: bool,
        grain: NavigateGrain,
    ) -> Effects {
        let Some(viewport_id) = self.view.viewport_id else {
            return Effects::none();
        };
        self.request_str::<ViewportNavigateChange>(
            ViewportNavigateChangeParams {
                viewport_id,
                direction: if forward {
                    FocusStep::Next
                } else {
                    FocusStep::Previous
                },
                count: Some(count),
                grain,
                extend,
            },
            move |result| Event::ViewStepped { grain, result },
        )
    }

    /// Adopt a focus reply: which element holds the cursor, and the buffer it windows. Reports
    /// whether focus actually *moved*, which is what decides whether the view is re-framed.
    ///
    /// Crossing into another buffer changes everything the view says about what it is showing —
    /// path, label, read-only, revision — so the whole `BufferInfo` is rebuilt through the same path
    /// an open uses. `view_id` deliberately does *not* move: the view is still the patch, and it is
    /// what `view/close` and `viewport/subscribe` go on addressing.
    fn adopt_focus(&mut self, r: ViewportFocusElementResult) -> bool {
        let moved = r.element != self.view.focused_element;
        self.view.focused_element = r.element;
        self.view.buffer = buffer_info(r.buffer, &self.workspace_paths);
        // The four buffer-level facts move with focus, because they are facts about the buffer the
        // cursor is in. Left out, they kept describing the element focus had just left: the pushes
        // that would correct them are keyed to a buffer and only fire on a *change*, so a `Tab`
        // between two files' hunks showed the previous file's breadcrumb, diagnostic counts and
        // language-server glyph until something unrelated happened to move.
        self.adopt_buffer_status(r.buffer_status);
        moved
    }

    /// Install a buffer-level status snapshot. Shared by subscribe and focus because it is the same
    /// snapshot about the same thing — the buffer under the cursor — and the two drifting apart is
    /// what left focus updating only half of it.
    fn adopt_buffer_status(&mut self, status: aether_protocol::viewport::BufferStatusSnapshot) {
        self.view.diagnostics = status.diagnostics;
        self.view.lsp = status.lsp_status;
        self.view.symbol_path = status.symbol_path;
        self.view.externally_modified = status.externally_modified;
        self.view.externally_deleted = status.externally_deleted;
    }

    /// Focus the element a click landed in, if it isn't already focused.
    ///
    /// A click names an element, and setting the cursor without moving focus first would apply the
    /// clicked *line number* to whatever buffer is focused — in a patch, a different file.
    /// Land the cursor at `position` inside `element` of the view already on screen.
    ///
    /// The landing for everything that resolves to a place *within* a composed view — the picker's
    /// `ViewElement`, and a jumplist step whose entry was captured from a view still open. Focusing
    /// first is the load-bearing half: cursors are per `(client, buffer)`, and a cursor moved into
    /// an element the view is not focused on is not drawn at all, which is how a jump that resolved
    /// and travelled correctly still looked like nothing happening.
    fn seat_in_view_element(
        &mut self,
        open: Option<ViewOpenResult>,
        element: aether_protocol::viewport::FieldId,
        buffer_id: BufferId,
        position: LogicalPosition,
        anchor: Option<LogicalPosition>,
    ) -> Effects {
        // The view may have had to be reopened to land in — adopt it first, because an element is
        // an index into *that* view's tree and focusing it means nothing until it is on screen.
        // Absent whenever the view was already showing, which is the ordinary case.
        let adopt = match open {
            Some(open) => self.adopt_navigation(open),
            None => Effects::none(),
        };
        adopt.and(self.seat_in_focused_view(element, buffer_id, position, anchor))
    }

    fn seat_in_focused_view(
        &mut self,
        element: aether_protocol::viewport::FieldId,
        buffer_id: BufferId,
        position: LogicalPosition,
        anchor: Option<LogicalPosition>,
    ) -> Effects {
        tracing::debug!(
            element,
            buffer_id,
            line = position.line,
            col = position.col,
            focused_before = self.view.focused_element,
            "seat in view element"
        );
        let focus = self.focus_clicked_element(element);
        focus.and(self.request_str::<CursorSet>(
            CursorSetParams {
                buffer_id,
                position,
                anchor: anchor.unwrap_or(position),
                granularity: Granularity::Char,
            },
            Event::CursorJump,
        ))
    }

    pub fn focus_clicked_element(
        &mut self,
        element: aether_protocol::viewport::FieldId,
    ) -> Effects {
        if element == self.view.focused_element {
            return Effects::none();
        }
        let Some(viewport_id) = self.view.viewport_id else {
            return Effects::none();
        };
        self.request_str::<ViewportFocusElement>(
            ViewportFocusElementParams {
                viewport_id,
                target: FocusTarget::Element { element },
            },
            Event::ElementClicked,
        )
    }

    /// Adopt the revision and cursor an edit response reports — but only if it is about the buffer
    /// this view currently holds.
    ///
    /// Two ways it can be about another one. The view can switch while an edit is in flight, and a
    /// view can be several editors over several buffers, where the server resolves which element —
    /// hence which buffer — an edit lands in. Adopting regardless would file one buffer's revision
    /// against another, and every later push for that buffer would then look stale and be dropped:
    /// a silent failure where the window simply stops updating.
    fn adopt_edit(&mut self, buffer: BufferId, revision: u64, cursor: CursorState) {
        if buffer != self.view.buffer.buffer_id {
            return;
        }
        self.view.buffer.revision = revision;
        self.view.buffer.cursor = cursor;
    }

    /// Adopt the result of a `viewport/subscribe` the shell issued: install the viewport binding
    /// and the buffer-wide status that rides with it atomically (diagnostics, language-server
    /// health, external-change flags), plus the first window. Pure core state — the shell owns the
    /// pixel work it does afterward (seeding the scroll, revealing the cursor). One definition
    /// shared by every shell: the native shells pass the typed result; the wasm shell deserialises
    /// the same struct. Shells must never write these fields directly.
    ///
    /// The effects are the reading view's (`sync_read_presentation`): the window says which view
    /// this is, and a reader's parse asks for its fence highlights.
    pub fn adopt_subscribe(&mut self, res: ViewportSubscribeResult) -> Effects {
        self.view.viewport_id = Some(res.viewport_id);
        self.adopt_buffer_status(res.buffer_status);
        self.view.window = Some(res.window);
        // The server decides which element holds the cursor (from the scroll the subscribe named,
        // today), so the mirror starts from its answer — whichever element that is, including
        // element 0 of an ordinary view. Taking it only for a composed view left a subscribe that
        // landed in a patch's own text with the server on element N and this on element 0.
        //
        // The buffer rebinds only across a boundary. A composed view acts on the buffer its focused
        // element windows, not on the buffer it was opened as: a patch's elements window real files
        // while the view's own document is the generated patch text, and holding the latter while
        // looking at the former is two line spaces at once — the cursor is a position in a document
        // nothing on screen belongs to, so nothing draws it and a reveal is owed that no window can
        // ever pay. For an ordinary view the focused element windows the buffer already held, whose
        // `BufferInfo` came from the open and carries the restored scroll this subscribe was framed
        // from; describing it again would only lose that.
        let focus = res.focus;
        self.view.focused_element = focus.element;
        if focus.buffer.buffer_id != self.view.buffer.buffer_id {
            self.view.buffer = buffer_info(focus.buffer, &self.workspace_paths);
        }
        self.sync_read_presentation()
    }

    /// Adopt the window from a geometry RPC the shell issued (`view/window`, `view/set_wrap`,
    /// `view/resize`). Pure core state; the shell clamps its scroll and reveals the cursor around it.
    /// The effects are the reading view's, as for [`Self::adopt_subscribe`].
    pub fn adopt_window(&mut self, res: ViewportWindowResult) -> Effects {
        self.replace_window(res.window);
        self.sync_read_presentation()
    }

    /// Replace the window of the view on screen, keeping the caret in the shell's input if that
    /// is where it was.
    ///
    /// Elements are numbered by position, and a shell appends every run *above* its input, so the
    /// number that named the input before this window names the new run in it. The server moves
    /// its own focus the same way when it rebuilds the view; doing it here too is what keeps the
    /// two agreeing without a focus field on every push.
    fn replace_window(&mut self, window: aether_protocol::viewport::Window) {
        let on_input = self.shell_input_focused();
        self.view.window = Some(window);
        if on_input {
            if let Some(input) = self.shell_input() {
                self.view.focused_element = input;
            }
        }
    }

    /// Report the viewport's current scroll position so the core knows what's actually on screen
    /// (the shell owns the pixel scroll). `top_visual_row` is absolute (whole-buffer); the core maps
    /// it through the loaded window to a logical-line range that scopes sneak candidates. Cheap —
    /// safe to call every render/scroll.
    pub fn set_visible_lines(
        &mut self,
        top_visual_row: VisualRow,
        viewport_rows: u32,
        measured: &crate::grid::Measured,
    ) {
        self.view.visible_lines = self.view.window.as_ref().map(|w| {
            let (_, first, _) = crate::grid::line_at_row(w, top_visual_row, measured);
            let bottom = top_visual_row.saturating_add(viewport_rows.saturating_sub(1));
            let (_, last, _) = crate::grid::line_at_row(w, bottom, measured);
            (first, last.saturating_add(1))
        });
    }

    /// Close the buffer, then attach to the server-indicated next MRU buffer (or a fresh scratch).
    /// Closing the [tether](Session::tether) instead exits the client — no successor needed. In an
    /// *ephemeral* context, never replace it with a scratch — an empty ephemeral workspace is
    /// pointless — so we close without `open_next` and either attach to a remaining sibling buffer
    /// or leave the context entirely (see [`Self::leave_ephemeral_workspace`]). Drop a pending
    /// commit whose message buffer is closing.
    ///
    /// Without this the entry outlives its buffer, and `Space g c` would then "switch to the
    /// message already open" — at a buffer id that no longer exists.
    fn forget_commit_buffer(&mut self, buffer_id: BufferId) {
        if self
            .pending_commit
            .as_ref()
            .is_some_and(|p| p.buffer_id == buffer_id)
        {
            self.pending_commit = None;
        }
    }

    pub fn close_view(&mut self) -> Effects {
        // Closing the prepared message *is* the commit — the `$EDITOR` contract, where git reads
        // the file once the editor exits and aborts if the message came back empty. So this fires
        // the commit instead of the close; `Event::Committed` clears the pending entry and calls
        // back here to actually close, and a refusal (a `pre-commit` hook) leaves the buffer open
        // with the message intact. Nothing is saved first, deliberately: git reads the *file*, so
        // closing without saving abandons exactly as quitting an editor without writing does.
        if let Some(pending) = self
            .pending_commit
            .clone()
            .filter(|p| p.buffer_id == self.view.view_buffer)
        {
            return self.request_str::<GitCommit>(
                GitCommitParams {
                    repo_id: pending.repo_id,
                    amend: pending.amend,
                },
                Event::Committed,
            );
        }
        self.forget_commit_buffer(self.view.view_buffer);
        if self.tethered_view() {
            return self.request_str::<ViewClose>(
                ViewCloseParams {
                    view_id: self.view.view_id,
                    open_next: false,
                },
                |r| Event::TetherClosed(r.map(|_| ())),
            );
        }
        if aether_protocol::is_ephemeral_workspace_id(&self.workspace) {
            return self.request_str::<ViewClose>(
                ViewCloseParams {
                    view_id: self.view.view_id,
                    open_next: false,
                },
                |r| Event::EphemeralClosed(r.map(|closed| closed.next_view_id)),
            );
        }
        self.request_str::<ViewClose>(
            ViewCloseParams {
                view_id: self.view.view_id,
                open_next: true,
            },
            |r| {
                Event::Switched(r.and_then(|closed| {
                    closed
                        .opened
                        .ok_or_else(|| "view/close returned no successor".into())
                }))
            },
        )
    }

    /// Leave an ephemeral ("(workspace N)") context whose last buffer just closed: reset to the
    /// workspace chooser (shell-side — see `Effect::ToChooser`). The current session's buffer is
    /// already closed; the shell discards the session rather than leaving the stale buffer
    /// rendered behind the picker.
    ///
    /// A session *launched* onto the file (`ae /path`) never reaches this — its buffer is the
    /// [tether](Session::tether), and closing the tether exits the client before the ephemeral
    /// checks run. This is the navigated-into case (selected from the switcher, a second client
    /// that joined, or a released tether), where quitting would be surprising. The web client's
    /// chooser is mandatory anyway, so landing there is exactly right for it too.
    fn leave_ephemeral_workspace(&mut self) -> Effects {
        Effects::one(Effect::ToChooser)
    }

    /// Copy the active buffer's path to the system clipboard — `absolute` picks the canonical
    /// on-disk path (`Space Alt-p`), otherwise the workspace-relative path (`Space p`). Scratch
    /// buffers have no path, so it warns instead.
    fn copy_buffer_path(&mut self, absolute: bool) -> Effects {
        let Some(path) = self.view.buffer.path.as_deref() else {
            return Effects::toast("A scratch has no path", ToastKind::Warning);
        };
        let text = if absolute {
            path.to_string()
        } else {
            // Bare root-relative path — unlike the display label, no `root:` prefix in
            // multi-root workspaces. Falls back to the absolute path outside every root.
            match strip_longest_root(path, &self.workspace_paths) {
                Some((_, rel)) => rel,
                None => path.to_string(),
            }
        };
        // Grouped: copying again (absolute vs relative) updates one toast rather than stacking.
        let mut fx = Effects::toast_grouped(
            if absolute {
                "Copied absolute path"
            } else {
                "Copied relative path"
            },
            ToastKind::Success,
            "copy-path",
        );
        fx.push(Effect::WriteClipboard(text));
        fx
    }

    /// Copy the web client's URL for the current view (`Space Alt-z`): a file buffer becomes the
    /// root-relative `?workspace=&root=&file=` link with the cursor as its 1-based `#L:C`
    /// fragment (a shared-cursor link — the web boot jumps there); a scratch becomes a
    /// `?workspace=&view=` link. A file with no workspace to be relative to — one outside every
    /// root, or any file in an ephemeral (no-workspace) context — becomes an absolute `?path=`
    /// link, which the web boot opens exactly as `ae PATH` does. Only a *pathless* buffer in a
    /// temporary context has nothing to address. The shell prepends its own base URL and writes the
    /// clipboard ([`ShellAction::CopyWebUrl`]); the toast is emitted here, like every copy gesture.
    fn copy_web_url(&mut self) -> Effects {
        use crate::web_link::{web_link, WebLinkTarget};
        let at = self.view.buffer.cursor.position;
        let named = !self.workspace.is_empty()
            && !aether_protocol::is_ephemeral_workspace_id(&self.workspace);
        let path_query = match self.view.buffer.path.as_deref() {
            // In a named workspace, a file under one of its roots is addressed relative to it: the
            // link survives the workspace moving on disk, and reads as what it is.
            Some(path) => match strip_longest_root(path, &self.workspace_paths).filter(|_| named) {
                Some((root, rel)) => web_link(
                    Some(&self.workspace),
                    WebLinkTarget::File {
                        root,
                        path: &rel,
                        at: Some((at.line, at.col)),
                    },
                ),
                // Nothing to be relative to: address the file itself.
                None => web_link(
                    None,
                    WebLinkTarget::Path {
                        path,
                        at: Some((at.line, at.col)),
                    },
                ),
            },
            // A scratch is reachable only by its view id, which is scoped to the workspace it lives
            // in — and a temporary context is not something a link can name (its id is recycled).
            None if !named => {
                return Effects::toast_detail(
                    "No web URL",
                    "This scratch isn't in a workspace",
                    ToastKind::Warning,
                )
            }
            None => web_link(
                Some(&self.workspace),
                WebLinkTarget::View(self.view.view_id),
            ),
        };
        // Grouped with the path copies: the copy gestures update one toast rather than stacking.
        let mut fx = Effects::toast_grouped("Copied web URL", ToastKind::Success, "copy-path");
        fx.push(Effect::ShellAction(ShellAction::CopyWebUrl { path_query }));
        fx
    }

    /// Open a file by absolute path as a transient preview — result-style navigation (picker
    /// selections, goto-definition). Records the jump origin onto the nav history first.
    ///
    /// A path inside one of the workspace's roots opens as an ordinary root-relative buffer; a path
    /// outside every root — goto-definition into a dependency's source, say — opens as an *external*
    /// guest buffer via `absolute_path` (the same mechanism the `Space Alt-w` open-from-path overlay
    /// uses), rather than refusing with a toast. Either way it lands as a transient preview with the
    /// jump origin on the nav history, so `Alt-Left` returns.
    pub fn open_path_at(
        &mut self,
        path: String,
        jump_to: Option<LogicalPosition>,
        jump_to_anchor: Option<LogicalPosition>,
    ) -> Effects {
        self.open_path_as(path, jump_to, jump_to_anchor, None)
    }

    /// [`Self::open_path_at`] asking for a kind of view — the reader, for a followed `#anchor`,
    /// which only a rendered document can land.
    fn open_path_as(
        &mut self,
        path: String,
        jump_to: Option<LogicalPosition>,
        jump_to_anchor: Option<LogicalPosition>,
        kind: Option<aether_protocol::ui::ViewKind>,
    ) -> Effects {
        // Any fresh open invalidates a not-yet-landed cross-file anchor (`read_follow_link`
        // re-arms after this call for its own open).
        self.pending_read_anchor = None;
        let (path_index, relative_path, absolute_path) =
            match strip_longest_root(&path, &self.workspace_paths) {
                Some((idx, rel)) => (Some(idx), Some(rel), None),
                None => (None, None, Some(path)),
            };
        self.request_str::<ViewOpen>(
            ViewOpenParams {
                path_index,
                relative_path,
                absolute_path,
                jump_to,
                jump_to_anchor,
                transient: Some(true),
                record_nav_from: Some(self.view.buffer.buffer_id),
                // A jump-shaped open (a grep hit, a reference) is a working context: the server
                // lands a `jump_to` in the editor even when the target is markdown — unless the
                // reader of that file is what's on screen, where the jump stays on the page.
                // `kind` is for the one route with its own opinion, a followed `#anchor`.
                kind,
                ..Default::default()
            },
            Event::Switched,
        )
    }

    /// Record a committed value to its input-history list and, when that actually changed the list,
    /// tell the server so it persists and other windows see it. The local apply is not optimism —
    /// it's the same [`HistoryLists::record`] rule the server runs, so the two can't disagree; the
    /// round-trip is fire-and-forget.
    pub fn record_history(&mut self, kind: HistoryKind, entry: HistoryEntry) -> Effects {
        if !self.history.record(kind, entry.clone()) {
            return Effects::none();
        }
        self.request_str::<HistoryRecord>(HistoryRecordParams { kind, entry }, |_| Event::Noop)
    }

    /// Fetch the active workspace's recall lists. Runs on connect (from [`Self::startup`]) and
    /// again after every workspace switch — the lists are workspace-scoped, so a switch makes the
    /// ones we hold wrong, not merely stale.
    fn fetch_history(&mut self) -> Effects {
        self.history.reset();
        self.request_str::<HistoryState>(HistoryStateParams {}, Event::HistoryLoaded)
    }

    /// Step the focused input's history one entry (`Up` = older, `Down` = newer). Returns the
    /// entry to install — value *and* the configuration it ran under — or `None` when there's
    /// nothing to recall: an empty list, an end of the walk, or no walk in progress to come back
    /// from. `current` is what the field holds now, stashed so `Down` can restore it whole.
    fn history_step(
        &mut self,
        kind: HistoryKind,
        dir: VerticalDirection,
        current: HistoryEntry,
    ) -> Option<HistoryEntry> {
        match dir {
            VerticalDirection::Up => self.history.prev(kind, current),
            VerticalDirection::Down => self.history.next(kind),
        }
    }

    /// Keys while a modal prompt is open. Confirm: only `y`/`Y` accepts; everything else —
    /// **Enter included** — declines, honouring the capital `N` in the rendered `[y/N]`. Every
    /// confirm we raise is destructive (overwrite / discard / delete / remove), so the safe option
    /// is the default and Enter never silently destroys. Save-as routes to its own editor.
    /// Mark an LSP server (by its [`lsp_toast_group`](crate::session::lsp_toast_group) key) as
    /// awaiting a restart and build the in-place "Restarting" toast. The matching `lsp/status_changed`
    /// Ready/Crashed push resolves it (see the `LspStatusChanged` handler), replacing this toast via
    /// the shared per-instance group key.
    fn lsp_restarting_toast(&mut self, name: &str, language: &str, workspace_root: &str) -> Effect {
        let group = crate::session::lsp_toast_group(language, workspace_root);
        self.lsp_restart_pending.insert(group.clone());
        Effect::Toast {
            title: format!("Restarting {name}"),
            body: None,
            kind: ToastKind::Info,
            group: Some(group),
        }
    }

    pub fn on_prompt_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        let Some(prompt) = self.prompt.take() else {
            return Effects::none();
        };
        match prompt {
            Prompt::Confirm { kind: _, action } => {
                let accepts = !mods.ctrl
                    && !mods.alt
                    && matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'));
                if accepts {
                    self.run_confirm(action)
                } else {
                    // `decline_confirm` re-opens the save-as prompt (and refetches its ghost) for an
                    // overwrite decline; pass its effects through rather than dropping them.
                    self.decline_confirm(action)
                }
            }
            Prompt::LspInfo(mut info) => {
                // `Ctrl-r` restarts (matching the picker list's `Ctrl-r`); any other key closes.
                if code == KeyCode::Char('r') && mods.ctrl && !mods.alt {
                    let mut fx = self.request::<LspRestartServer>(
                        LspRestartServerParams {
                            language: info.language.clone(),
                        },
                        move |__r| {
                            let _ = __r;
                            Event::Noop
                        },
                    );
                    fx.push(self.lsp_restarting_toast(
                        &info.name,
                        &info.language,
                        &info.workspace_root,
                    ));
                    // Keep the dialog open so the user can watch the lifecycle — show `Restarting`
                    // at once, then the server's `lsp/status_changed` pushes refresh it through to
                    // Ready (see the `LspStatusChanged` handler). Esc / any other key still closes.
                    info.status = aether_protocol::lsp::LspStatus::Restarting;
                    info.progress.clear();
                    self.prompt = Some(Prompt::LspInfo(info));
                    return fx;
                }
                Effects::none()
            }
            Prompt::AppInfo(info) => {
                // `Ctrl-c` copies the whole snapshot as text — the paste-into-a-bug-report gesture,
                // and why the dialog beats `ae server status` in another terminal. The editor's own
                // Copy chord ([`Action::Copy`]), same as the hover popover's copy: this dialog has
                // no text input, so nothing claims the chord before the core sees it (the hazard in
                // docs — a *focused query input* — doesn't apply here).
                // Any other key closes (the prompt was already taken above).
                if code == KeyCode::Char('c') && mods.ctrl && !mods.alt {
                    let text = crate::app_info::to_plain_text(info.as_deref(), &self.conn);
                    let mut fx = Effects::toast("Copied app info", ToastKind::Success);
                    fx.push(Effect::WriteClipboard(text));
                    // Stay open: copying isn't dismissing, and the toast confirms it landed.
                    self.prompt = Some(Prompt::AppInfo(info));
                    return fx;
                }
                Effects::none()
            }
            Prompt::SaveAs(editor) => {
                // Text editing (insert / delete / caret) is owned by the shell's input, which syncs
                // the value via `save_as_set_input` / `save_as_set_root_filter`. The command keys
                // route through `on_save_as_key` — put the editor back so it can read/mutate it.
                self.prompt = Some(Prompt::SaveAs(editor));
                self.on_save_as_key(code, mods, text)
            }
            Prompt::OpenPath(editor) => {
                // Same shape as the `SaveAs` arm above: the editor owns the command keys, so put it
                // back before handing them over.
                self.prompt = Some(Prompt::OpenPath(editor));
                self.on_open_path_key(code, mods, text)
            }
        }
    }

    /// `Space j` — show the diagnostic(s) at the cursor in the hover box. Prefers
    /// diagnostics under the cursor column (zero-width points widened to one cell), falling
    /// back to all on the line. Reads the cached window render — no round-trip.
    pub fn show_diagnostic(&self) -> Effects {
        let cursor = self.view.buffer.cursor.position;
        let diags: Vec<(DiagnosticSeverity, String)> = self
            .view
            .window
            .as_ref()
            .and_then(|w| {
                let at = self.view.cursor_at();
                crate::grid::window_lines(w)
                    .into_iter()
                    .find(|(there, _)| *there == at)
            })
            .map(|(_, line)| {
                let under: Vec<_> = line
                    .diagnostics
                    .iter()
                    .filter(|d| cursor.col >= d.start && cursor.col < d.end.max(d.start + 1))
                    .map(|d| (d.severity, d.message.clone()))
                    .collect();
                if under.is_empty() {
                    line.diagnostics
                        .iter()
                        .map(|d| (d.severity, d.message.clone()))
                        .collect()
                } else {
                    under
                }
            })
            .unwrap_or_default();
        if diags.is_empty() {
            let mut fx = Effects::one(Effect::DismissHover);
            fx.push(Effect::Toast {
                title: "No diagnostics on this line".into(),
                body: None,
                kind: ToastKind::Info,
                group: None,
            });
            return fx;
        }
        Effects::one(Effect::ShowHover(HoverText::Blocks(
            diags
                .into_iter()
                .map(|(severity, msg)| HoverBlock {
                    text: format!("{}: {msg}", severity_label(severity)),
                    severity: Some(severity),
                })
                .collect(),
        )))
    }

    /// `Space m` — blame the cursor line and resolve the commit's details, one round-trip
    /// (`include_commit_info`). The multi-step operation this
    /// buffer's repo is stopped in, if any — read off the status the window already carries, which
    /// is the same fact the status bar is displaying.
    ///
    /// `None` covers both "nothing is stopped" and "we have no window yet to say so"; the caller
    /// treats them alike, because the server re-checks before doing anything either way.
    fn stopped_operation(&self) -> Option<GitRepoOperation> {
        self.view.window.as_ref()?.git_status.as_ref()?.operation
    }

    /// `git/abort_operation` for the buffer's repo — reached from `Space g d` once its confirm is
    /// accepted, or directly when there was no operation to confirm about.
    fn abort_operation(&mut self, buffer_id: BufferId) -> Effects {
        self.request_str::<GitAbortOperation>(
            GitAbortOperationParams {
                // Resolved server-side from the buffer we're on, like every other git verb.
                repo_id: None,
                buffer_id: Some(buffer_id),
            },
            Event::OperationAborted,
        )
    }

    pub fn show_commit_info(&mut self) -> Effects {
        self.request_str::<GitBlameLine>(
            GitBlameLineParams {
                buffer_id: self.view.buffer.buffer_id,
                line: self.view.buffer.cursor.position.line,
                include_commit_info: true,
            },
            |r| {
                Event::CommitLookup(r.map(|r| match r.blame {
                    Some(b) if b.is_uncommitted => {
                        CommitDetails::Note("This line isn't committed yet")
                    }
                    None => CommitDetails::Note("No commit details for this line"),
                    Some(_) => match r.commit_info {
                        Some(info) => CommitDetails::Info(Box::new(info)),
                        None => CommitDetails::Note("Commit not found"),
                    },
                }))
            },
        )
    }

    // ---- pickers ----------------------------------------------------------------------------

    /// Open a picker: subscribe a window and let `picker/update` pushes fill it. Every open is a
    /// fresh one ([`PickerReset::All`], uniform across kinds) — no query, chips or highlight
    /// carries over from the last time this picker was up. The kinds that want to land somewhere
    /// meaningful derive it from the *live* cursor instead ([`PickerKind::centers_on_cursor`]).
    /// `directory_path` seeds the Explorer's listing (its `Space e` = the buffer's directory).
    /// `seed_filters` replaces the server's persisted set (Explorer→Grep/Files switches,
    /// `Space Alt-f`); the echo through `PickerViewed` rebuilds the chip row.
    /// `from_selection` (Grep, `Space Alt-/`) tells the server to seed the query from the buffer's
    /// selection and run the search in this same call — the derived query/generation ride the
    /// `PickerViewed` echo, so there's no separate `picker/query` to send.
    /// `center_on_override` replaces the per-kind "where you are" default below — the
    /// capture→Jumplist swap uses it to keep the just-captured row highlighted.
    pub fn open_picker(
        &mut self,
        kind: PickerKind,
        directory_path: Option<String>,
        seed_filters: Option<PickerFilters>,
        from_selection: bool,
        center_on_override: Option<PickerItem>,
    ) -> Effects {
        let mut fresh = PickerState::new(kind);
        // Stamped at open so the workspace-symbols picker can distinguish "no matches" from
        // "nothing can answer here".
        fresh.workspace_has_projects = !self.workspace_projects.is_empty();
        self.picker = Some(fresh);
        // A fresh input owns the keyboard now; anything the last one was recalling is over.
        self.history.reset();
        let buffer_id = self.view.buffer.buffer_id;
        let has_center_override = center_on_override.is_some();
        // Views / Workspaces / Explorer / LspServers all open with the highlight on "where you
        // are" — the active buffer/workspace/file/language-server — matched by item key via the
        // `effective_center_on` echo (the display-only fields below are ignored by the match).
        // Views: the active *view* (key is `buffer_id`) — `view_id`, not the focused element's
        // buffer. The picker lists views, so in a composed one the row to land on is the patch
        // itself; centring on `view.buffer` highlighted whichever file the cursor happened to be
        // in, or nothing at all when that file had no row of its own.
        // Workspaces: the active workspace (key is `name`).
        // Explorer: the active buffer's filename, so the listing lands on the current file.
        // LspServers: the active buffer's own language server (key is `language` + `workspace_root`).
        let center_on = center_on_override.or(match kind {
            PickerKind::Views => Some(PickerItem::View {
                buffer_id: self.view.view_buffer,
                view_id: self.view.view_id,
                view_kind: None,
                display: String::new(),
                status: Default::default(),
                path_index: None,
                relative_path: None,
                match_indices: Vec::new(),
                transient: false,
            }),
            PickerKind::Workspaces => Some(PickerItem::Workspace {
                name: self.workspace.clone(),
                unsaved: 0,
                match_indices: Vec::new(),
            }),
            PickerKind::Explorer => self.view.buffer.path.as_deref().and_then(|path| {
                let name = std::path::Path::new(path)
                    .file_name()?
                    .to_str()?
                    .to_string();
                Some(PickerItem::DirEntry {
                    name,
                    is_dir: false,
                    match_indices: Vec::new(),
                    git_status: None,
                })
            }),
            PickerKind::LspServers => {
                self.view
                    .buffer
                    .lsp_server
                    .as_ref()
                    .map(|r| PickerItem::LspServer {
                        name: String::new(),
                        language: r.language.clone(),
                        workspace_root: r.workspace_root.clone(),
                        root_label: String::new(),
                        status: aether_protocol::lsp::LspStatus::Ready,
                        progress: Vec::new(),
                        match_indices: Vec::new(),
                    })
            }
            _ => None,
        });

        let request = self.request::<PickerView>(
            PickerViewParams {
                kind,
                reset: PickerReset::All,
                offset: 0,
                limit: FETCH_LIMIT,
                center_on,
                // A from-selection grep runs a brand-new search; there are no cached hits to land
                // the cursor on, so skip cursor-centering for it. An explicit `center_on`
                // override (the capture→Jumplist swap's just-captured row) also wins: the
                // server's cursor resolution would trump the client-passed item otherwise.
                center_on_cursor: (!from_selection
                    && !has_center_override
                    && kind.centers_on_cursor())
                .then_some(buffer_id),
                directory_path,
                explorer_roots: false,
                // The buffer-scoped kinds send the buffer they list *for*. GitBranches sends it
                // for a different reason: it's the repo-resolution hint, the same one
                // `git/prepare_commit` takes — without it a multi-repo workspace can't tell which
                // repo's branches to list. (The workspace changes picker needs no hint: it lists
                // every root, whatever repos they span.)
                buffer_id: (from_selection
                    || matches!(
                        kind,
                        PickerKind::Diagnostics
                            | PickerKind::References
                            | PickerKind::DocumentSymbols
                            | PickerKind::GitChangesFile
                            | PickerKind::GitBranches
                            // The log and stash pickers resolve their repo from the buffer; the
                            // file-locked log additionally takes its path from it.
                            | PickerKind::GitLog
                            | PickerKind::GitLogFile
                            | PickerKind::GitStash
                            // Likewise the baseline picker: the repo it re-baselines is the one
                            // the buffer you are looking at lives in.
                            | PickerKind::GitBaseline
                    ))
                .then_some(buffer_id),
                // The **listing** kinds also get the view, so they answer for everything it shows
                // rather than for the hunk the cursor is in. Only these two: the other kinds that
                // take a buffer want the focused one (a repo to resolve, a path, a selection to
                // slice), and handing them a view id would answer a different question.
                view_id: matches!(kind, PickerKind::Diagnostics | PickerKind::DocumentSymbols)
                    .then_some(self.view.view_id),
                from_selection,
                filters: seed_filters,
                // The binding tables live here in the client core, so a fresh Keybindings open
                // ships its rows for the server to match against.
                keybindings: (kind == PickerKind::Keybindings)
                    .then(crate::keymap::keybinding_entries),
            },
            move |__r| Event::PickerViewed {
                initial: true,
                result: __r.map_err(|e| e.message),
            },
        );
        // Every open starts the list at the top. A kind that wants to land somewhere else centres
        // via the `effective_center_on` echo, which arrives with the response and reveals *after*
        // this — the same order Views and the Explorer have always opened in.
        Effects::one(Effect::PickerScrollReset).and(request)
    }

    /// `Space Alt-f`: open Files pre-scoped to the active buffer's directory — a normal dir filter
    /// chip, visible/editable/removable, composable with globs. Falls back to an unscoped open for
    /// scratch buffers or files outside every root. (Grep's `Space Alt-/` is the unrelated
    /// [`Session::open_grep_from_selection`].)
    pub fn open_files_in_file_dir(&mut self) -> Effects {
        let seed = self
            .view
            .buffer
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).parent())
            .map(|p| p.display().to_string())
            .and_then(|dir| strip_longest_root(&dir, &self.workspace_paths))
            .map(|(path_index, relative_path)| PickerFilters {
                directories: vec![ScopedPath {
                    path_index,
                    relative_path,
                    is_file: false,
                }],
                ..PickerFilters::default()
            });
        self.open_picker(PickerKind::Files, None, seed, false, None)
    }

    /// `Space Alt-/`: open Grep with the query seeded from the buffer's selection — the grep
    /// equivalent of `Alt-/`. The server slices the selection, installs it as a literal query, and
    /// runs the search in the same `picker/view`; the derived query/generation ride back through
    /// the `PickerViewed` echo (so there's no follow-up `picker/query`). It's an ordinary open, so
    /// the chip row starts empty and the selection is searched workspace-wide. An empty selection
    /// just opens grep with no query.
    pub fn open_grep_from_selection(&mut self) -> Effects {
        self.open_picker(PickerKind::Grep, None, None, true, None)
    }

    /// `Ctrl-g` / `Ctrl-f` in the Explorer: switch to the Grep / Files picker scoped to the
    /// directory being browsed ("grep here"), the explorer's filters translated along. In
    /// Roots mode no dir scope is seeded — the target covers the whole workspace.
    fn switch_explorer_picker(&mut self, target: PickerKind) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.kind != PickerKind::Explorer {
            return Effects::none();
        }
        let dir_scope = p
            .directory
            .as_deref()
            .and_then(|abs| strip_longest_root(abs, &self.workspace_paths))
            .map(|(path_index, relative_path)| ScopedPath {
                path_index,
                relative_path,
                is_file: false,
            });
        let seeded = seeded_filters_for_switch(&p.wire_filters(), dir_scope, target);
        let hide = self.close_picker();
        hide.and(self.open_picker(target, None, Some(seeded), false, None))
    }

    /// `Space e` / `Space Alt-e`: Explorer at the buffer's directory, or at its workspace root.
    /// Scratch buffers fall through to the server default (last listing / first root).
    pub fn open_explorer(&mut self, at_root: bool) -> Effects {
        let dir = self.view.buffer.path.as_deref().and_then(|path| {
            if at_root {
                let (i, _) = strip_longest_root(path, &self.workspace_paths)?;
                self.workspace_paths.get(i as usize).cloned()
            } else {
                std::path::Path::new(path)
                    .parent()
                    .map(|p| p.display().to_string())
            }
        });
        self.open_picker(PickerKind::Explorer, dir, None, false, None)
    }

    /// Explorer navigation: list a different directory (or the workspace roots). Clears the
    /// query — entering a directory starts a fresh listing — but the filter chips ride along.
    /// `pre_select` lands the highlight on the named entry once the listing arrives.
    fn explorer_navigate(
        &mut self,
        directory_path: Option<String>,
        roots: bool,
        pre_select: Option<String>,
    ) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.generation += 1;
        p.query.clear();
        p.selected = 0;
        p.offset = 0;
        p.items.clear();
        p.refetch_in_flight = false; // fresh listing supersedes any in-flight scroll refetch
        let generation = p.generation;
        let filters = p.wire_filters();
        let center_on = pre_select.map(|name| PickerItem::DirEntry {
            name,
            is_dir: true,
            match_indices: Vec::new(),
            git_status: None,
        });

        let mut fx = Effects::one(Effect::PickerScrollReset);
        fx = fx.and(self.request::<PickerQuery>(
            PickerQueryParams {
                kind: PickerKind::Explorer,
                query: String::new(),
                generation,
                // The query RPC replaces the persisted filters too — carry the chips so a
                // racing arrival order can't wipe them under the view below.
                filters: filters.clone(),
            },
            move |__r| {
                let _ = __r;
                Event::Noop
            },
        ));
        fx = fx.and(self.request::<PickerView>(
            PickerViewParams {
                view_id: None,
                kind: PickerKind::Explorer,
                reset: PickerReset::Keep,
                offset: 0,
                limit: FETCH_LIMIT,
                center_on,
                center_on_cursor: None,
                directory_path,
                explorer_roots: roots,
                buffer_id: None,
                from_selection: false,
                filters: Some(filters),
                keybindings: None,
            },
            move |__r| Event::PickerViewed {
                initial: false,
                result: __r.map_err(|e| e.message),
            },
        ));
        fx
    }

    /// Move the picker highlight, refetching when it leaves the fetched window and revealing
    /// it otherwise (the shell scrolls the native list the minimum to keep it visible).
    /// Wheel scroll over the picker overlay: move the highlight by `delta` rows, like Alt-j/k.
    /// A no-op when no picker is open. Lets a shell route wheel events to the picker without
    /// reaching into its private navigation.
    pub fn picker_wheel(&mut self, delta: i64) -> Effects {
        if self.picker.is_none() {
            return Effects::none();
        }
        self.picker_move(delta)
    }

    fn picker_move(&mut self, delta: i64) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        // Two-level navigation for the collapsible kinds: with the selection on a group header,
        // Alt-j/k move *between groups* — a server-resolved step (`picker/set_group`; the neighbour
        // may sit past the fetched window) that leaves expansion alone, so a walk over headers
        // stays an overview. With the selection among a run's items, moves are local and
        // run-clamped — except at the run's edges, where they *spill* into the neighbouring group:
        // down off the last item enters the next group at its first item, up off the first enters
        // the previous at its last (still item level). A spill opens the group it walks into and
        // leaves the one it came from open, so a long Alt-j walk unfolds the list behind it. The
        // very ends still stop (the step answers `run: None`).
        if p.collapsible {
            // Single-flight: a gesture is mid-reshape — swallow repeats rather than route
            // them against transient state (a second step would skip a group's items).
            if p.group_gesture_in_flight {
                return Effects::none();
            }
            let direction = if delta < 0 {
                Direction::Backward
            } else {
                Direction::Forward
            };
            if !p.selection_at_item_level() {
                return self.picker_step_group(direction, GroupLanding::Header);
            }
            if let Some((first, last)) = p.focus_item_rows() {
                if delta > 0 && p.selected == last {
                    return self.picker_step_group(direction, GroupLanding::RunStart);
                }
                if delta < 0 && p.selected == first {
                    return self.picker_step_group(direction, GroupLanding::RunEnd);
                }
            }
        }
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(offset) = p.move_selection(delta) else {
            return Effects::one(Effect::RevealPickerSelection(Reveal::Minimal));
        };
        // Single-flight: only one window refetch is allowed in flight at a time. If one is already
        // running, coalesce this move — `selected` has already advanced locally, and the trailing
        // check when the reply lands (see `PickerViewed`) chases it with one fetch. This turns a
        // fast scroll from one request per move into ~one per round-trip (no pile-up, no
        // out-of-order replies). This move follows the selection, so the reply should chase it.
        if p.refetch_in_flight {
            return Effects::none();
        }
        self.picker_refetch(offset, true)
    }

    /// Re-subscribe the picker's window at a new offset. Marks the single in-flight refetch slot
    /// busy; the matching `PickerViewed` frees it. `chase_selection` records intent: keyboard nav
    /// passes `true` so the reply chases the highlight if coalesced moves ran it past the window;
    /// free pixel scroll (iced / web) passes `false` — the view moved, not the selection, so the
    /// window must stay where it was scrolled, not snap back.
    pub fn picker_refetch(&mut self, offset: u32, chase_selection: bool) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.refetch_in_flight = true;
        p.refetch_chases_selection = chase_selection;
        p.offset = offset;
        p.items.clear();
        let kind = p.kind;

        self.request::<PickerView>(
            PickerViewParams {
                view_id: None,
                kind,
                reset: PickerReset::Keep,
                offset,
                limit: FETCH_LIMIT,
                center_on: None,
                center_on_cursor: None,
                directory_path: None,
                explorer_roots: false,
                buffer_id: None,
                from_selection: false,
                filters: None,
                keybindings: None,
            },
            move |__r| Event::PickerViewed {
                initial: false,
                result: __r.map_err(|e| e.message),
            },
        )
    }

    /// A query edit: bump the generation (stale pushes get discarded), restart the window at
    /// the top, and tell the server.
    fn picker_query_changed(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.generation += 1;
        // Typing claims the generation: the server adopts `picker/query`'s number, so from here
        // the client's is authoritative — a view response landing later must not regress it (nor
        // clobber the typed query with its carried snapshot).
        p.generation_adopted = true;
        p.selected = 0;
        p.offset = 0;
        // Selection resets to row 0 — the (auto-expanded) top group's header: group level, matching
        // the server's expansion reset on `picker/query`. Any mid-flight group gesture is
        // superseded (its push may be generation-discarded, so it can't be relied on to release the
        // repeat guard).
        p.level = PickerLevel::Group;
        p.group_gesture_in_flight = false;
        // A new query starts a fresh window cycle — abandon any in-flight scroll refetch so its
        // late reply can't wedge the single-flight slot.
        p.refetch_in_flight = false;
        // We deliberately keep the *previous* query's window on screen until the fresh one arrives,
        // rather than clearing it now — clearing flashes an empty list on every keystroke (the new
        // window rides `picker/query`'s own `picker/update` push, a round-trip away). For the
        // synchronous kinds (files/buffers/symbols/diagnostics/explorer) the server reranks and
        // pushes the real window in one shot, so the stale rows are replaced atomically (same
        // generation, offset 0 — the server resets its window to match) with no blank in between.
        // (Streaming grep still clears to its own first push; that's the "Searching…" path.)
        // We also don't send a `picker/view` here — its point-in-time snapshot races the streaming
        // grep push and an empty one would blank the list. One request, one source of truth.
        //
        // A new query is in flight: mark the picker as searching now, before the first
        // `picker/update` push arrives, so the shell can show progress in the gap (otherwise a slow
        // grep reads as "no matches" until results stream). The server's pushes refine it from here.
        p.ticking = true;
        // A query change invalidates any pending pre-selection (the active-item centering) —
        // the user is steering somewhere new.
        p.pending_center = None;
        p.reveal_on_update = None;
        let (kind, query, generation) = (p.kind, p.query.clone(), p.generation);
        // An open glob/dir editor folds its in-progress value in for a live preview; otherwise
        // this is the committed chips. `None` (a dir listing mid-flight) can't happen here —
        // callers that might hold gate on `live_filters` before re-querying — but fall back to
        // the committed set defensively.
        let filters = p
            .live_filters(&workspace_paths)
            .unwrap_or_else(|| p.wire_filters());
        p.sent_filters = filters.clone();

        let mut fx = self.request::<PickerQuery>(
            PickerQueryParams {
                kind,
                query,
                generation,
                filters,
            },
            move |__r| {
                let _ = __r;
                Event::Noop
            },
        );
        fx.push(Effect::PickerScrollReset);
        fx
    }

    /// Replace the picker query wholesale and re-filter. A shell whose query field owns text editing
    /// (the web client's native `<input>`, with caret/selection/IME/paste) syncs the full value here
    /// instead of feeding character keys through [`on_picker_key`]. No-op if unchanged.
    pub fn picker_set_query(&mut self, query: String) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if p.query == query {
            return Effects::none();
        }
        p.query = query;
        // Typing abandons any history walk in progress (the recall path sets `query` directly, so
        // it doesn't come through here).
        self.history.reset();
        self.picker_query_changed()
    }

    /// Replace the search query wholesale and re-run the incremental search (the web client's native
    /// search `<input>` owns text editing and syncs the value here). No-op outside Search mode or if
    /// unchanged.
    pub fn search_set_query(&mut self, query: String) -> Effects {
        if self.view.mode != Mode::Search || self.view.search.query == query {
            return Effects::none();
        }
        self.view.search.query = query;
        // Typing abandons any history walk in progress — the stashed draft is stale now.
        self.history.reset();
        self.incremental_search()
    }

    /// Replace the save-as prompt's path-field text wholesale (each shell's input owns editing and
    /// syncs the value here). Re-derives the directory suggestion listing when the dir portion
    /// moved. No-op unless a save-as prompt is open.
    pub fn save_as_set_input(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::SaveAs(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        if ed.input.text == text {
            return Effects::none();
        }
        ed.input.set(text);
        if ed.path_edited(&workspace_paths) {
            self.refresh_save_as_listing()
        } else {
            Effects::none()
        }
    }

    /// Replace the multi-root save-as editor's root-filter text wholesale (native `<input>`
    /// parity). Resets the typeahead highlight to the best match and re-syncs the listing under the
    /// newly chosen root. No-op unless a save-as prompt is open.
    pub fn save_as_set_root_filter(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::SaveAs(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        if ed.root_filter.text == text {
            return Effects::none();
        }
        ed.root_filter.set(text);
        ed.root_selected = 0;
        if ed.sync_dir_listing(&workspace_paths) {
            self.refresh_save_as_listing()
        } else {
            Effects::none()
        }
    }

    /// Move focus between the save-as editor's root and path segments (the web client lets you
    /// click the unfocused segment). The path can't be entered under an invalid root — focus stays
    /// pinned to the red root. No-op outside a multi-root save-as prompt.
    pub fn save_as_set_field(&mut self, root: bool) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let labels = super::labels::root_labels(&workspace_paths);
        let Some(Prompt::SaveAs(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        if workspace_paths.len() <= 1 {
            return Effects::none();
        }
        ed.field = if root {
            ChipEditorField::Root
        } else if ed.root_invalid(&labels) {
            return Effects::none();
        } else {
            ChipEditorField::Path
        };
        Effects::none()
    }

    /// Replace the workspace-settings name-field text wholesale (the web client's native `<input>`
    /// owns editing and syncs the value here). The native shells edit it key-by-key through
    /// `on_workspace_settings_key`; this is the web parity entry point. No-op unless the overlay is
    /// open. Clears any in-dialog error, matching the key path.
    pub fn workspace_settings_set_name(&mut self, text: String) -> Effects {
        if let Some(s) = self.workspace_settings.as_mut() {
            if s.name.text != text {
                s.name.set(text);
                s.error = None;
            }
        }
        Effects::none()
    }

    /// Replace the workspace-settings add-root input text wholesale (native `<input>` parity, as
    /// above). No-op unless the overlay is open.
    pub fn workspace_settings_set_add(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        if s.add.input.text == text {
            return Effects::none();
        }
        s.add.input.set(text);
        s.error = None;
        if s.add.path_edited(&workspace_paths) {
            self.refresh_add_root_listing()
        } else {
            Effects::none()
        }
    }

    /// Fire `directory/list` for the add-root editor. See [`Self::refresh_path_editor_listing`],
    /// which every path-editing surface shares — and which reads this editor's `Absolute` base to
    /// send the unrestricted listing it needs.
    fn refresh_add_root_listing(&mut self) -> Effects {
        self.refresh_path_editor_listing(PathEditorOwner::AddRoot)
    }

    /// Replace the add-project row's path-segment text wholesale (native `<input>` parity, as
    /// above), refreshing its completion listing if the directory portion moved and re-syncing the
    /// language suggestion to the newly named directory.
    pub fn workspace_settings_set_add_project(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        if s.add_project.input.text == text {
            return Effects::none();
        }
        s.add_project.input.set(text);
        s.error = None;
        let mut fx = Effects::none();
        if s.add_project.path_edited(&workspace_paths) {
            fx = fx.and(self.refresh_add_project_listing());
        }
        fx.and(self.sync_add_project_inference())
    }

    /// Replace the add-project row's *root* segment filter (multi-root workspaces only).
    pub fn workspace_settings_set_add_project_root(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        if s.add_project.root_filter.text == text {
            return Effects::none();
        }
        s.add_project.root_filter.set(text);
        s.add_project.root_selected = 0;
        s.error = None;
        let mut fx = Effects::none();
        if s.add_project.sync_dir_listing(&workspace_paths) {
            fx = fx.and(self.refresh_add_project_listing());
        }
        // The chosen root is half the (root, path) pair the suggestion hangs off.
        fx.and(self.sync_add_project_inference())
    }

    /// Fire `directory/list` for the add-project editor. See
    /// [`Self::refresh_path_editor_listing`], which every path-editing surface shares.
    fn refresh_add_project_listing(&mut self) -> Effects {
        self.refresh_path_editor_listing(PathEditorOwner::AddProject)
    }

    /// Keep the add-project row's language suggestion in step with its (root, path) pair: when the
    /// pair moves, ask the server what language declaring that directory would pin
    /// (`workspace/infer_language` — the directory's own manifests, minus languages already
    /// declared for it). The result pre-fills an untouched language segment
    /// ([`Event::AddProjectLanguageInferred`]); one the user has typed into is left alone. Deduped
    /// on the pair, so calling this after any key that might have moved it is free.
    fn sync_add_project_inference(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        let target = s.add_project.save_target(&workspace_paths);
        if target == s.inference_key {
            return Effects::none();
        }
        s.inference_key = target.clone();
        let workspace = s.workspace_name.clone();
        let Some((path_index, relative_path)) = target else {
            // The path emptied: an inferred suggestion goes with it (a typed language stays).
            if s.language_inferred {
                s.add_project_language.clear();
                s.add_project_language_selected = 0;
                s.language_inferred = false;
            }
            return Effects::none();
        };
        let key = (path_index, relative_path.clone());
        self.request::<WorkspaceInferLanguage>(
            WorkspaceInferLanguageParams {
                workspace,
                path_index,
                relative_path,
            },
            move |r| Event::AddProjectLanguageInferred {
                key,
                language: r.ok().and_then(|v| v.language),
            },
        )
    }

    /// Replace the chip editor's path-field text wholesale (the web client's native `<input>` owns
    /// editing and syncs the value here). For a dir editor this re-derives the directory suggestion
    /// listing. No-op unless a chip editor is open.
    pub fn chip_editor_set_input(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        if ed.input.text == text {
            return Effects::none();
        }
        ed.input.set(text);
        // Typing abandons any history walk (the recall path writes `input` directly).
        self.history.reset();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        let refresh = ed.is_dir() && ed.path_edited(&workspace_paths);
        let mut fx = Effects::none();
        if refresh {
            fx = fx.and(self.refresh_chip_editor_listing());
        }
        // The in-progress value moved — re-run results to match (held while a refetch is in
        // flight; `live_filters` returns `None` until the listing lands).
        fx.and(self.sync_live_filters())
    }

    /// Replace the multi-root dir editor's root-filter text wholesale (native `<input>` parity).
    /// Resets the typeahead highlight to the best match and re-syncs the listing under the newly
    /// chosen root. No-op unless a chip editor is open.
    pub fn chip_editor_set_root_filter(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        if ed.root_filter.text == text {
            return Effects::none();
        }
        ed.root_filter.set(text);
        ed.root_selected = 0;
        let refresh = ed.sync_dir_listing(&workspace_paths);
        let mut fx = Effects::none();
        if refresh {
            fx = fx.and(self.refresh_chip_editor_listing());
        }
        // The chosen root drives the would-commit scope; re-run results to match.
        fx.and(self.sync_live_filters())
    }

    /// Move focus between the dir editor's root and path segments (the web client lets you click the
    /// unfocused segment). The path can't be entered under an invalid root — focus stays pinned to
    /// the red root, matching the keyboard gate. No-op outside a multi-root dir editor.
    pub fn chip_editor_set_field(&mut self, root: bool) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let labels = super::labels::root_labels(&workspace_paths);
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        if !ed.is_dir() || workspace_paths.len() <= 1 {
            return Effects::none();
        }
        ed.field = if root {
            ChipEditorField::Root
        } else if ed.root_invalid(&labels) {
            return Effects::none();
        } else {
            ChipEditorField::Path
        };
        Effects::none()
    }

    /// Keep an open LSP info dialog in step with the live LSP picker beneath it. LSP progress
    /// `report`s (Indexing 10% → 20% …) refresh the picker but deliberately *don't* broadcast
    /// `lsp/status_changed` (which fires only on begin/end busy transitions), so a dialog driven
    /// solely by `status_changed` would freeze its "Working" line at the opening snapshot. Re-reads
    /// the matching server's status + progress from the picker's current items.
    fn sync_lsp_dialog_from_picker(&mut self) {
        let Some(Prompt::LspInfo(info)) = self.prompt.as_mut() else {
            return;
        };
        let Some(p) = &self.picker else {
            return;
        };
        let matching = p.items.iter().find_map(|it| match it {
            PickerItem::LspServer {
                language,
                workspace_root,
                status,
                progress,
                ..
            } if *language == info.language && *workspace_root == info.workspace_root => {
                Some((status.clone(), progress.clone()))
            }
            _ => None,
        });
        if let Some((status, progress)) = matching {
            info.status = status;
            info.progress = progress;
        }
    }

    /// Push a filter (chip) change. For Grep/Files a filter change *is* a query change (same
    /// generation mechanics); for the Explorer the filters apply when the listing is built,
    /// so re-view the current directory with the replacement set. No-op for kinds that take
    /// no filters, and for the Explorer's Roots mode (nothing to filter there).
    fn apply_picker_filter_change(&mut self) -> Effects {
        let Some(kind) = self.picker.as_ref().map(|p| p.kind) else {
            return Effects::none();
        };
        match kind {
            PickerKind::Grep
            | PickerKind::Files
            | PickerKind::GitChanges
            | PickerKind::GitChangesFile
            | PickerKind::Jumplist
            | PickerKind::WorkspaceSymbols => self.picker_query_changed(),
            PickerKind::Explorer => {
                let filters = {
                    let Some(p) = &mut self.picker else {
                        return Effects::none();
                    };
                    if p.directory.is_none() {
                        return Effects::none(); // Roots mode
                    }
                    p.selected = 0;
                    p.offset = 0;
                    p.items.clear();
                    p.refetch_in_flight = false; // fresh listing supersedes any in-flight refetch
                    let f = p.wire_filters();
                    p.sent_filters = f.clone();
                    f
                };

                Effects::one(Effect::PickerScrollReset).and(self.request::<PickerView>(
                    PickerViewParams {
                        view_id: None,
                        kind: PickerKind::Explorer,
                        reset: PickerReset::Keep,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: None,
                        center_on_cursor: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        from_selection: false,
                        filters: Some(filters),
                        keybindings: None,
                    },
                    move |__r| Event::PickerViewed {
                        initial: false,
                        result: __r.map_err(|e| e.message),
                    },
                ))
            }
            _ => Effects::none(),
        }
    }

    /// Re-run the live query when an open glob/dir editor's in-progress value changes the effective
    /// filter set, so results update as you type. A no-op when the editor leaves the effective
    /// filters unchanged (focus moves, edits that don't move the would-commit value), when a dir
    /// listing is still loading (hold — `live_filters` returns `None`), or outside the streaming
    /// kinds. Also the path back to the committed set when the editor closes: with no editor open
    /// `live_filters` is the committed `wire_filters`, so a cancel that had a preview applied
    /// reverts here.
    fn sync_live_filters(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if !matches!(
            p.kind,
            PickerKind::Grep
                | PickerKind::Files
                | PickerKind::GitChanges
                | PickerKind::Jumplist
                | PickerKind::WorkspaceSymbols
        ) {
            return Effects::none();
        }
        let Some(eff) = p.live_filters(&workspace_paths) else {
            return Effects::none(); // indeterminate — hold the current results
        };
        if eff == p.sent_filters {
            return Effects::none(); // nothing the server isn't already running
        }
        self.picker_query_changed()
    }

    /// Toggle/cycle the filter a chord (or Enter on a selected chip) names, then push the
    /// change. A chord that doesn't apply to this picker kind is a clean no-op.
    fn toggle_picker_filter(&mut self, id: ChipId) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.filter_available(id) {
            return Effects::none();
        }
        // Explorer and Files both show hidden (and, for the Explorer, ignored) entries by default,
        // so their visibility chips *hide*; Grep's *include*. Files only offers the hidden chip.
        let hide = matches!(p.kind, PickerKind::Explorer | PickerKind::Files);
        if !chips::apply_chip_toggle(&mut p.chips, id, hide) {
            return Effects::none(); // valued chips (dir, glob) go through their editors
        }
        self.apply_picker_filter_change()
    }

    /// `Enter` on a selected chip: valued chips re-open their editor pre-filled; everything
    /// else toggles/cycles in place (a plain boolean's chip disappears).
    fn edit_selected_chip(&mut self, id: ChipId) -> Effects {
        match id {
            ChipId::Glob(i) => self.open_glob_prompt(Some(i)),
            ChipId::Dir(i) => self.open_dir_prompt(Some(i)),
            _ => self.toggle_picker_filter(id),
        }
    }

    /// Open the glob editor line. `edit: Some(i)` pre-fills glob `i`; `None` adds a new chip
    /// on commit.
    fn open_glob_prompt(&mut self, edit: Option<usize>) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.filter_available(ChipId::Glob(0)) {
            return Effects::none();
        }
        // The editor owns the keys now; a lingering chip selection would go stale once the
        // commit reshapes the row.
        p.chip_selected = None;
        let prefill = edit
            .and_then(|i| p.glob_value(i))
            .map(str::to_string)
            .unwrap_or_default();
        // Baseline for the live-preview dedup: what the server is showing right now (the
        // committed chips). A fresh/empty editor leaves the effective set equal to this, so it
        // takes a real edit before results move.
        p.sent_filters = p.wire_filters();
        p.chip_editor = Some(ChipEditor::glob(prefill, edit));
        Effects::none()
    }

    /// Open the directory-scope editor line. `edit: Some(i)` re-opens scope `i` pre-filled
    /// (path focused); `None` adds a new chip on commit (multi-root workspaces focus the root
    /// segment first). Kicks off a `directory/list` so the path field's ghost suggestions
    /// are ready when focus lands there.
    fn open_dir_prompt(&mut self, edit: Option<usize>) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.filter_available(ChipId::Dir(0)) {
            return Effects::none();
        }
        p.chip_selected = None;
        let current = edit.and_then(|i| p.dir_value(i).cloned());
        let multi_root = workspace_paths.len() > 1;
        let root_index = current.as_ref().map(|d| d.path_index).unwrap_or(0);
        let field = if multi_root && current.is_none() {
            ChipEditorField::Root
        } else {
            ChipEditorField::Path
        };
        // Grep / GitChanges / Jumplist / workspace symbols may scope to a single file; the
        // Files picker stays directory-only (narrowing a file list to one file is degenerate).
        let allow_files = matches!(
            p.kind,
            PickerKind::Grep
                | PickerKind::GitChanges
                | PickerKind::Jumplist
                | PickerKind::WorkspaceSymbols
        );
        let mut ed = ChipEditor::dir(
            current.map(|d| d.relative_path).unwrap_or_default(),
            field,
            root_index,
            edit,
            allow_files,
        );
        ed.sync_dir_listing(&workspace_paths);
        // Baseline for the live-preview dedup — the currently displayed (committed) set.
        p.sent_filters = p.wire_filters();
        p.chip_editor = Some(ed);
        self.refresh_chip_editor_listing()
    }

    /// Fire `directory/list` for the dir-chip editor's current (root, dir-portion) pair. The
    /// requested path rides on the result event so a stale response (the editor moved on)
    /// can be discarded. No-op for glob editors and invalid roots.
    fn refresh_chip_editor_listing(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(path) = self
            .picker
            .as_ref()
            .and_then(|p| p.chip_editor.as_ref())
            .and_then(|ed| ed.dir_listing_path(&workspace_paths))
        else {
            return Effects::none();
        };
        let abs = path.clone();

        self.request::<DirectoryList>(
            DirectoryListParams {
                path,
                unrestricted: false,
            },
            move |__r| Event::PickerChipListing {
                abs,
                result: __r.map_err(|e| e.message),
            },
        )
    }

    /// Commit the chip editor line. A dir editor only commits a *valid* scope — a root that
    /// matches some label and a path that exists (or is empty); otherwise the editor stays
    /// open with the invalid segment rendered red.
    fn commit_chip_editor(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if let Some(ed) = p.chip_editor.as_ref() {
            if ed.is_dir() {
                let root_ok = workspace_paths.len() < 2 || {
                    let labels = super::labels::root_labels(&workspace_paths);
                    !ed.root_invalid(&labels)
                };
                if !root_ok || !ed.path_valid() {
                    return Effects::none();
                }
            }
        }
        let Some(ed) = p.chip_editor.take() else {
            return Effects::none();
        };
        // The committed field text goes to the input history, whether or not it changed the chip
        // row: re-committing the same scope is still a use, and recording the *typed* text (not the
        // parsed scope) is what lets recall replay it verbatim.
        let recorded = if ed.is_dir() {
            (HistoryKind::Path, ed.input.text.trim().to_string())
        } else {
            // A glob that normalizes away (empty, bare `*`) is a chip *removal*, not a value —
            // `record_history` drops the empty string, so nothing lands.
            (
                HistoryKind::Glob,
                chips::normalize_glob(&ed.input.text).unwrap_or_default(),
            )
        };
        // These two lists carry no configuration: for them the value *is* the configuration.
        let recorded = (recorded.0, HistoryEntry::bare(recorded.1));
        let changed = match ed.kind {
            chips::ChipEditorKind::Glob { edit } => {
                let normalized = chips::normalize_glob(&ed.input.text);
                chips::commit_glob_edit(&mut p.chips, normalized, edit)
            }
            chips::ChipEditorKind::Dir { edit } => {
                // The would-commit scope — `None` for an empty path in a single-root workspace
                // ("the whole root" means "no narrowing"). The validity gate above guarantees
                // `preview_scope` sees a valid root/path, so this is exactly what the live
                // preview was already showing.
                let value = ed.preview_scope(&workspace_paths);
                chips::commit_dir_edit(&mut p.chips, value, edit)
            }
        };
        self.history.reset();
        let fx = self.record_history(recorded.0, recorded.1);
        if !changed {
            return fx;
        }
        fx.and(self.apply_picker_filter_change())
    }

    /// Alt-l: descend into the highlighted explorer directory (Enter does too, via accept).
    fn explorer_enter_selected(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if let Some(PickerItem::DirEntry {
            name, is_dir: true, ..
        }) = p.selected_item()
        {
            let dir = match p.explorer_listing_dir() {
                Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                None => return Effects::none(),
            };
            return self.explorer_navigate(Some(dir), false, None);
        }
        // In the roots view (multi-root), descend into the selected root — mirrors Enter.
        if let Some(PickerItem::Root { path_index, .. }) = p.selected_item() {
            let dir = self.workspace_paths.get(*path_index as usize).cloned();
            return self.explorer_navigate(dir, false, None);
        }
        Effects::none()
    }

    /// Alt-Backspace: progressively unwind — one query word per press, then (explorer) one
    /// directory segment per press — landing the highlight on the directory just left — into roots
    /// mode in multi-root workspaces, and only then the rightmost filter chip. The breadcrumb sits
    /// closest to the cursor and unwinds first; chips have their own toggle bindings. Deliberately
    /// *not* bound to Alt-h: clearing input is Alt-Backspace's job alone — Alt-h only ever goes
    /// structurally shallower (ascend to the group header, explorer ascend) and never touches the
    /// query.
    ///
    /// The query rung is word-grained rather than a wipe, which makes this key mean the same thing
    /// here as in a buffer and in the path editors next door: remove the last unit of input, and
    /// once there is none left, unwind what encloses it. A single-word query — most of them —
    /// still clears in one press; a multi-word one drops one matcher atom at a time, and repeated
    /// presses still walk the whole ladder.
    fn picker_back(&mut self) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.query.is_empty() {
            p.query = chips::pop_word(&p.query);
            return self.picker_query_changed();
        }
        // Explorer: unwind the breadcrumb one directory segment per press before touching chips.
        if let Some(fx) = self.explorer_ascend() {
            return fx;
        }
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if let Some(chip) = p.chip_row(&workspace_paths).last().map(|c| c.id) {
            chips::remove_chip(&mut p.chips, chip);
            p.chip_selected = None;
            return self.apply_picker_filter_change();
        }
        Effects::none()
    }

    /// Explorer Alt-h (and [`Self::picker_back`]'s middle stage): step out of the listed
    /// directory — one segment per press, landing the highlight on the directory just left —
    /// then, at a root with siblings, into Roots mode. The structural mirror of Alt-l's
    /// descend. `None` when there's nothing to ascend (non-Explorer kinds, single-root top,
    /// already in Roots mode).
    fn explorer_ascend(&mut self) -> Option<Effects> {
        let workspace_root_count = self.workspace_paths.len();
        let p = self.picker.as_ref()?;
        if p.kind != PickerKind::Explorer {
            return None;
        }
        match p.directory_parent.clone() {
            Some(parent) => {
                // Pre-select the directory we're leaving in the parent's listing.
                let leaving = p.directory.as_deref().and_then(|d| {
                    std::path::Path::new(d)
                        .file_name()
                        .and_then(|os| os.to_str())
                        .map(str::to_string)
                });
                Some(self.explorer_navigate(Some(parent), false, leaving))
            }
            // At a root with siblings: step out into Roots mode (the root name is the last
            // breadcrumb segment).
            None if p.directory.is_some() && workspace_root_count > 1 => {
                Some(self.explorer_navigate(None, true, None))
            }
            // Single-root top, or already in Roots mode: nothing left to ascend.
            _ => None,
        }
    }

    /// Enter / row click: act on the highlighted item. Directories and roots navigate within
    /// the open explorer; everything else closes the panel and runs `picker/select`.
    /// The [`WindowTarget`] that duplicates the current view (`Space z`): a real workspace lands
    /// the sibling on its MRU buffer (`WindowOpen::Workspace`); an ephemeral file context passes the
    /// buffer's path (the ephemeral id isn't CLI-addressable); a pathless ephemeral scratch can't be
    /// reproduced, so the sibling opens the chooser.
    fn current_view_target(&self) -> WindowTarget {
        let workspace = (!aether_protocol::is_ephemeral_workspace_id(&self.workspace))
            .then(|| self.workspace.clone());
        let open = match (&workspace, self.view.buffer.path.as_deref()) {
            (Some(_), _) | (None, None) => WindowOpen::Workspace,
            (None, Some(path)) => WindowOpen::Path {
                path: path.to_string(),
                at: None,
            },
        };
        WindowTarget {
            workspace,
            // The same context, not just the same workspace: duplicating a window on a worktree and
            // landing on the main checkout would be a surprising `Space z`.
            worktrees: self.window_worktrees(),
            open,
        }
    }

    /// This session's bindings in [`WindowTarget`] form.
    fn window_worktrees(&self) -> Vec<(String, String)> {
        self.workspace_worktrees
            .iter()
            .map(|w| (w.repo_id.clone(), w.worktree.clone()))
            .collect()
    }

    /// The spawn descriptor for opening the highlighted picker item in a *new* window (`Ctrl-Enter`),
    /// or `None` when the row isn't a new-window target. The native counterpart of the web client's
    /// `pickerItemUrl`: it supports the same set — files, grep hits, file-backed and scratch buffers,
    /// explorer files, and workspaces — and declines directories, roots, LSP servers, keybindings,
    /// and the diagnostic/reference/symbol jump targets (all of which the web client also omits).
    fn picker_item_target(&self) -> Option<WindowTarget> {
        let p = self.picker.as_ref()?;
        // The synthetic "+ Create …" row has nothing to open in another window.
        if p.selected_is_create() {
            return None;
        }
        // Files/grep/buffers live in the *current* workspace; a Workspace row names its own. A path
        // is only CLI-addressable when the workspace is real — an ephemeral id can't seed a fresh
        // `ae`, so we open by path alone there (mirrors `current_view_target`).
        let here = (!aether_protocol::is_ephemeral_workspace_id(&self.workspace))
            .then(|| self.workspace.clone());
        let abs = |path_index: u32, relative: &str| -> Option<String> {
            let root = self.workspace_paths.get(path_index as usize)?;
            Some(format!("{}/{}", root.trim_end_matches('/'), relative))
        };
        // Rows that open something *here* carry this context, so a new window on a file you are
        // reading in a worktree opens in that worktree rather than on the main checkout.
        let mine = self.window_worktrees();
        match p.selected_item()? {
            PickerItem::File {
                path_index,
                relative_path,
                ..
            } => Some(WindowTarget {
                workspace: here,
                worktrees: mine,
                open: WindowOpen::Path {
                    path: abs(*path_index, relative_path)?,
                    at: None,
                },
            }),
            PickerItem::GrepHit {
                path_index,
                relative_path,
                line,
                col,
                ..
            } => Some(WindowTarget {
                workspace: here,
                worktrees: mine,
                open: WindowOpen::Path {
                    path: abs(*path_index, relative_path)?,
                    at: Some((*line, *col)),
                },
            }),
            // A file-backed buffer opens by path, like a Files row.
            PickerItem::View {
                path_index: Some(pi),
                relative_path: Some(rel),
                ..
            } => Some(WindowTarget {
                workspace: here,
                worktrees: mine,
                open: WindowOpen::Path {
                    path: abs(*pi, rel)?,
                    at: None,
                },
            }),
            // A scratch (no path) re-opens by view id against the shared daemon — but only when the
            // workspace is CLI-addressable (the new `ae` must activate it before the open).
            PickerItem::View { view_id, .. } => here.map(|ws| WindowTarget {
                workspace: Some(ws),
                worktrees: mine,
                open: WindowOpen::View(*view_id),
            }),
            // An explorer *file* (a directory navigates within the picker instead). The listing dir
            // is absolute, so join the leaf name for the absolute path.
            PickerItem::DirEntry {
                name,
                is_dir: false,
                ..
            } => {
                let dir = p.explorer_listing_dir()?;
                Some(WindowTarget {
                    workspace: here,
                    worktrees: mine,
                    open: WindowOpen::Path {
                        path: format!("{}/{name}", dir.trim_end_matches('/')),
                        at: None,
                    },
                })
            }
            // Open a *different* workspace in a new window — lands on its MRU buffer.
            // A *different* workspace opens in whichever context it was last used in — ours says
            // nothing about it, and its repos may not even be the same ones.
            PickerItem::Workspace { name, .. } => Some(WindowTarget {
                workspace: Some(name.clone()),
                worktrees: Vec::new(),
                open: WindowOpen::Workspace,
            }),
            // A branch row: open the tree that holds it, in a new window. This is the verb the whole
            // context keying exists for — two windows, two trees of one repo, at once.
            //
            // A branch **no tree holds** has nothing to open in another window: git permits one
            // checkout per branch, so a second window on it would need a tree that does not exist.
            // Refused with the way forward rather than silently falling through to an ordinary
            // accept, which would check the branch out *here* — something else entirely from what
            // was asked for.
            PickerItem::GitBranch {
                repo_id, checkout, ..
            } => {
                let checkout = checkout.as_ref()?;
                let mut worktrees: Vec<(String, String)> = self
                    .workspace_worktrees
                    .iter()
                    .filter(|w| &w.repo_id != repo_id)
                    .map(|w| (w.repo_id.clone(), w.worktree.clone()))
                    .collect();
                // An empty admin name is the main checkout, which is the *absence* of a binding —
                // the same thing `workspace/bind_worktree` reads it as.
                if !checkout.worktree.is_empty() {
                    worktrees.push((repo_id.clone(), checkout.worktree.clone()));
                }
                Some(WindowTarget {
                    workspace: here,
                    worktrees,
                    open: WindowOpen::Workspace,
                })
            }
            _ => None,
        }
    }

    /// A Ctrl-click on a picker row — the mouse sibling of `Ctrl-Enter`: move the selection onto the
    /// clicked row, then open it in a new window (or fall through to a normal open when the row isn't
    /// a new-window target). Shell-invoked (the iced GUI reads the modifier at click time, since a
    /// `mouse_area` press carries none); the TUI never calls it.
    pub fn picker_click_new_window(&mut self, abs: u32) -> Effects {
        if let Some(p) = &mut self.picker {
            p.selected = abs;
            // A header row has no new-window target (the client doesn't hold the group's first item
            // while collapsed) — treat the Ctrl-click like a plain click: disclosure, not a jump.
            if let Some(PickerItem::Group {
                header, expanded, ..
            }) = p.selected_item()
            {
                let (header, expanded) = (header.clone(), *expanded);
                p.level = PickerLevel::Group;
                return if expanded {
                    self.picker_collapse_group(header)
                } else {
                    self.picker_expand_group(header, GroupLanding::Header)
                };
            }
        }
        if let Some(target) = self.picker_item_target() {
            return self.close_picker().and(Effects::one(Effect::ShellAction(
                ShellAction::NewWindow(target),
            )));
        }
        self.picker_accept()
    }

    fn picker_accept(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        // The synthetic "+ Create …" row: confirming it creates the named file/dir (Explorer) or
        // a fresh workspace (Workspaces).
        if p.selected_is_create() {
            return match p.kind {
                PickerKind::Workspaces => self.workspace_create_from_query(),
                PickerKind::GitBranches => self.branch_create_from_query(),
                _ => self.explorer_create_from_query(),
            };
        }
        let Some(item) = p.selected_item().cloned() else {
            return Effects::none();
        };
        match &item {
            PickerItem::DirEntry {
                name, is_dir: true, ..
            } => {
                let dir = match p.explorer_listing_dir() {
                    Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                    None => return Effects::none(),
                };
                return self.explorer_navigate(Some(dir), false, None);
            }
            PickerItem::Root { path_index, .. } => {
                let dir = self.workspace_paths.get(*path_index as usize).cloned();
                return self.explorer_navigate(dir, false, None);
            }
            PickerItem::GitStash { repo_id, oid, .. } => {
                // A stash *is* a commit, so previewing it is the log picker's Enter verbatim: its
                // first-parent diff is exactly what `git stash show -p` prints.
                let params = aether_protocol::git::GitShowParams {
                    repo_id: Some(repo_id.clone()),
                    buffer_id: None,
                    target: aether_protocol::git::ShowTarget::Commit { rev: oid.clone() },
                    focus_path: None, // a stash is about no file in particular
                };
                let hide = self.close_picker();
                return hide
                    .and(self.request_str::<aether_protocol::git::GitShow>(params, Event::Shown));
            }
            PickerItem::GitBaseline {
                repo_id, choice, ..
            } => {
                // `choice: None` is the "back to the index" row, and `None` is exactly what the
                // RPC takes to clear a baseline — so the row needs no special case here.
                let params = aether_protocol::git::GitSetBaselineParams {
                    repo_id: repo_id.clone(),
                    source: choice.clone(),
                };
                let hide = self.close_picker();
                return hide.and(
                    self.request::<aether_protocol::git::GitSetBaseline>(params, |r| {
                        Event::BaselineSet(r.map(|r| r.baseline).map_err(|e| e.message))
                    }),
                );
            }
            PickerItem::GitCommit {
                repo_id,
                hash,
                path,
                ..
            } => {
                // The row *is* the revision, so this needs no resolution: `git/show` materialises
                // the commit as a read-only virtual buffer and the result adopts exactly like a
                // `view/open` (same shape), so the picker closes onto the diff.
                //
                // From a *file's* history the row also names that file, and the cursor lands on its
                // changes — you asked about one path, not about everything the commit touched.
                let params = aether_protocol::git::GitShowParams {
                    repo_id: Some(repo_id.clone()),
                    buffer_id: None,
                    target: aether_protocol::git::ShowTarget::Commit { rev: hash.clone() },
                    focus_path: path.clone(),
                };
                let hide = self.close_picker();
                return hide
                    .and(self.request_str::<aether_protocol::git::GitShow>(params, Event::Shown));
            }
            PickerItem::GitBranch {
                repo_id,
                name,
                is_head,
                checkout,
                detached_at,
                ..
            } => {
                // One intent — *get me to this branch* — and git's state picks the mechanism. A
                // branch a tree already holds cannot be checked out again, so going there means
                // moving this window to that tree; a branch no tree holds is reached by moving
                // HEAD here. Either way what the user sees is "I am now looking at this branch, in
                // this window", which is what makes the two mechanisms one gesture rather than an
                // overload.
                if let Some(checkout) = checkout {
                    if checkout.is_current {
                        // Already here. A detached row has no branch to name, so it says where you
                        // are instead of what you are on.
                        return Effects::toast(
                            if detached_at.is_some() {
                                format!("Already in {name}")
                            } else {
                                format!("Already on {name}")
                            },
                            ToastKind::Info,
                        );
                    }
                    // An empty admin name is the main checkout, which is exactly what
                    // `workspace/bind_worktree` reads as "unbind" — so selecting the branch main
                    // holds sends this repo back to it, with no case of its own.
                    let (repo_id, worktree) = (repo_id.clone(), checkout.worktree.clone());
                    return self.bind_worktree(repo_id, worktree);
                }
                // Already here: say so rather than spawning a git that would do nothing.
                if *is_head {
                    return Effects::toast(format!("Already on {name}"), ToastKind::Info);
                }
                let (repo_id, name) = (repo_id.clone(), name.clone());
                // Close first: a checkout is a terminal action, and the list it was showing is
                // about to be stale (the HEAD marker moves).
                let hide = self.close_picker();
                return hide.and(self.git_checkout(repo_id, name, false));
            }
            PickerItem::LspServer {
                name,
                language,
                workspace_root,
                root_label,
                status,
                progress,
                ..
            } => {
                // Not a jump target: Enter drills into the detail dialog (restart lives
                // there and on Ctrl-r in the list). The picker stays open *underneath* — the
                // dialog is a prompt, which takes key precedence — so closing it (Esc / any
                // non-Ctrl-r key) returns to the LSP picker with this server still selected,
                // mirroring the explorer's delete-confirm drawn over its listing.
                let info = LspServerStatus {
                    name: name.clone(),
                    language: language.clone(),
                    workspace_root: workspace_root.clone(),
                    status: status.clone(),
                    progress: progress.clone(),
                };
                let _ = root_label;
                self.prompt = Some(Prompt::LspInfo(Box::new(info)));
                return Effects::none();
            }
            PickerItem::Keybinding { .. } => {
                // Informational — a shortcut row isn't a jump target and Enter doesn't fire the
                // binding, so it does nothing: the picker stays open (no close, no `picker/select`).
                // This keeps it clear that the list is a reference, not a command palette; Esc
                // dismisses it like any other picker.
                return Effects::none();
            }
            // A group's header row IS a jump target: Enter resolves server-side to the group's
            // *first item* — so type-query-then-Enter takes the top group's top hit without a
            // mandatory descend. (Click is the disclosure gesture instead — see
            // `Event::PickerClicked`.) Falls through to the ordinary `picker/select` below.
            PickerItem::Group { .. } => {}
            _ => {}
        }
        let kind = p.kind;
        // Hint observation before the picker closes: accepting a workspace row demonstrates the
        // chooser's open hint (its follow, when displayed).
        let observed =
            if kind == PickerKind::Workspaces && matches!(item, PickerItem::Workspace { .. }) {
                self.observe_picker_cmd(PickerCmd::OpenWorkspace)
            } else {
                Effects::none()
            };
        // Resolve the pick *before* closing. `picker/hide` releases the picker's state server-side,
        // and requests go out in enqueue order, so a `picker/select` behind the close would have no
        // candidate set left to resolve its item against — an `invalid params` error instead of a
        // jump. Closing second also reads right: the row is resolved, then the list goes away.
        let select = self.request::<PickerSelect>(PickerSelectParams { kind, item }, move |__r| {
            Event::PickerSelected {
                result: __r.map_err(|e| e.message),
            }
        });

        observed.and(select).and(self.close_picker())
    }

    /// Drop the panel and unsubscribe (the server keeps walker/matcher state for resume).
    /// Select the rightmost filter chip (the browser tag-input gesture: Left / Backspace at the start
    /// of the query steps into the chip row). The web client's native query `<input>` owns the caret,
    /// so the shell detects "at query start" itself and calls this, rather than relying on the core's
    /// cursor-based entry in [`Self::on_picker_key`]. No-op when there are no chips. Pure selection
    /// state — no effects. Once a chip is selected, the chip-nav keys route through `on_picker_key`.
    pub fn picker_select_last_chip(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        if let Some(p) = &mut self.picker {
            let n = p.chip_row(&workspace_paths).len();
            if n > 0 {
                p.chip_selected = Some(n - 1);
            }
        }
        Effects::none()
    }

    /// Is the open picker the *mandatory* chooser — the Workspaces picker over a placeholder
    /// session (a no-args start, or after `ToChooser`)? There's nothing behind it to fall back to,
    /// so no dismissal gesture may close it: Esc exits the process instead (see
    /// [`Self::on_picker_key`]) and a shell's click-away leaves it up.
    pub fn picker_is_mandatory(&self) -> bool {
        self.is_placeholder()
            && self
                .picker
                .as_ref()
                .is_some_and(|p| p.kind == PickerKind::Workspaces)
    }

    pub fn close_picker(&mut self) -> Effects {
        let Some(p) = self.picker.take() else {
            return Effects::none();
        };
        self.history.reset();
        // Closing is what commits the query to the input history — grep searches per keystroke, so
        // recording on change would store `h`, `ha`, `han`… Only the query the user actually
        // settled on lands, and only if it was long enough to have run a search at all. Covers
        // accept and dismiss alike: both funnel through here. The whole chip row rides along, so
        // recalling the query later reproduces the search it was — scope, match options and all.
        let mut fx = match p.kind.history_kind() {
            Some(kind) if p.query.chars().count() >= MIN_GREP_QUERY_LEN => {
                let entry = HistoryEntry {
                    value: p.query.clone(),
                    filters: p.wire_filters(),
                };
                self.record_history(kind, entry)
            }
            _ => Effects::none(),
        };
        fx = fx.and(
            self.request::<PickerHide>(PickerHideParams { kind: p.kind }, move |__r| {
                let _ = __r;
                Event::Noop
            }),
        );
        fx
    }

    /// Keys while a picker is open: list navigation + query editing.
    pub fn on_picker_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        // The chip editor line (glob/dir, revealed below the input) owns the keys while open.
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.chip_editor.is_some())
        {
            return self.on_chip_editor_key(code, mods, text);
        }
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let no_chord = !mods.ctrl && !mods.alt;
        // A selected chip captures the editing keys (Enter edits, Backspace/Delete removes,
        // Left/Right walk the row, Esc deselects, typing deselects back into the query).
        // Anything else falls through to the normal picker vocabulary below.
        if let Some(sel) = p.chip_selected {
            let row = p.chip_row(&workspace_paths);
            if row.is_empty() {
                p.chip_selected = None;
            } else {
                let sel = sel.min(row.len() - 1);
                match code {
                    KeyCode::Left if no_chord => {
                        p.chip_selected = Some(sel.saturating_sub(1));
                        return Effects::none();
                    }
                    KeyCode::Right if no_chord => {
                        if sel + 1 >= row.len() {
                            p.chip_selected = None;
                        } else {
                            p.chip_selected = Some(sel + 1);
                        }
                        return Effects::none();
                    }
                    KeyCode::Esc => {
                        p.chip_selected = None;
                        return Effects::none();
                    }
                    KeyCode::Backspace | KeyCode::Delete if no_chord => {
                        chips::remove_chip(&mut p.chips, row[sel].id);
                        let remaining = row.len() - 1;
                        p.chip_selected = (remaining > 0).then(|| sel.min(remaining - 1));
                        return self.apply_picker_filter_change();
                    }
                    KeyCode::Enter if no_chord => {
                        return self.edit_selected_chip(row[sel].id);
                    }
                    KeyCode::Char(_) if no_chord => {
                        // Typing returns to the query — fall through so the char lands.
                        p.chip_selected = None;
                    }
                    _ => {}
                }
            }
        }
        let mandatory = self.picker_is_mandatory();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        match code {
            // The mandatory chooser ([`Self::picker_is_mandatory`]): there is nothing behind the
            // picker to fall back to, so Esc exits instead of dismissing. The picker deliberately
            // stays open — a shell that can't exit (the web: a browser tab has no process to quit)
            // maps `Effect::Exit` to a no-op and the chooser simply remains up. Deliberately not a
            // `PickerCmd::Dismiss` observation: nothing closes here.
            KeyCode::Esc if mandatory => {
                return Effects::one(Effect::Exit);
            }
            // Hint observation before the picker closes (the picker-dismiss hint displays in
            // this context — Esc while it's up is its follow).
            KeyCode::Esc => {
                let observed = self.observe_picker_cmd(PickerCmd::Dismiss);
                return observed.and(self.close_picker());
            }
            // Ctrl-Enter opens the highlighted item in a *new* window (GUI-only; a no-op in the TUI),
            // mirroring the web client's Ctrl/Cmd-Enter "open in a new tab". Rows that aren't a
            // new-window target (directories, LSP servers, keybindings, …) fall through to an
            // ordinary accept — the same fall-through the web shell does when the row has no URL.
            KeyCode::Enter if mods.ctrl => {
                // A branch no tree holds cannot be opened in a second window: git permits one
                // checkout per branch. Refused *with the way forward* rather than falling through
                // to an ordinary accept, which would check the branch out here — something else
                // entirely from what was asked for, and silently.
                if p.kind == PickerKind::GitBranches {
                    if let Some(PickerItem::GitBranch { name, checkout, .. }) = p.selected_item() {
                        if checkout.is_none() {
                            return Effects::toast(
                                format!("{name} has no worktree yet"),
                                ToastKind::Warning,
                            );
                        }
                    }
                }
                if let Some(target) = self.picker_item_target() {
                    return self.close_picker().and(Effects::one(Effect::ShellAction(
                        ShellAction::NewWindow(target),
                    )));
                }
                return self.picker_accept();
            }
            KeyCode::Enter => return self.picker_accept(),
            // Ctrl-d: trash the highlighted entry (Files + Explorer) or delete the highlighted
            // workspace (Workspaces), behind a confirm. (Not plain `Delete` — that's a forward-delete
            // in the query input, owned by the shell; deleting is too destructive to ride a bare
            // editing key.)
            KeyCode::Char('d')
                if mods.ctrl
                    && !mods.alt
                    && matches!(
                        p.kind,
                        PickerKind::Files | PickerKind::Explorer | PickerKind::Workspaces
                    ) =>
            {
                return self.picker_stage_delete();
            }
            // Ctrl-d in the view picker closes the highlighted row in place (no open) — a live
            // buffer or a dormant (session-restored) one alike, the server resolves which. It shares
            // the `Ctrl-d` key with the delete-file gesture above but not the kind (Views vs
            // Files/Explorer/Workspaces), so the two guards stay disjoint; closing a buffer just
            // drops it from the list, it doesn't delete anything on disk. The picker stays open (see
            // `picker_close_view`). NOT `Ctrl-x` (tempting for the `Space x` mnemonic): every GUI
            // shell's focused query input claims Ctrl-x as its native Cut and swallows it before the
            // core ever sees it — the iced forward gate in `app.rs` only forwards keys the input left
            // uncaptured, and the web `routeOverlayKey` clip filter drops Ctrl-c/v/x/a outright. Only
            // the TUI (which forwards every Ctrl chord) would see it. Ctrl-d dodges all three.
            // GitBranches: Ctrl-d deletes the highlighted branch behind a confirm — the same
            // gesture Explorer/Files/Workspaces and Views use for "remove the highlighted thing".
            KeyCode::Char('d') if mods.ctrl && p.kind == PickerKind::GitBranches => {
                let Some(PickerItem::GitBranch {
                    repo_id,
                    name,
                    checkout,
                    detached_at,
                    ..
                }) = p.selected_item()
                else {
                    return Effects::none();
                };
                // One key, taking the **outermost** thing off: a row with a worktree loses the
                // worktree, a row without one loses the branch. The exact inverse of `Ctrl-o`
                // building the tree onto the branch, and pressing it twice walks the row back down
                // — with the row visibly changing in between, which is what keeps it honest.
                let Some(checkout) = checkout else {
                    if mods.alt {
                        // Force is the escalation from a `NotMerged` refusal the user has read, and
                        // that path runs through the confirm below. Nothing to escalate from yet.
                        return Effects::none();
                    }
                    let name = name.clone();
                    self.prompt = Some(Prompt::Confirm {
                        kind: ConfirmKind::DeleteBranch { name: name.clone() },
                        action: ConfirmAction::DeleteBranch { name, force: false },
                    });
                    return Effects::none();
                };
                // Two things this row can't be asked to do, checked in the order the user would
                // hit them. Standing in it comes first: it is true of the main checkout *and* of a
                // worktree, and "you are here" is the more useful sentence either way.
                if checkout.is_current {
                    return Effects::error_detail(
                        if detached_at.is_some() {
                            format!("You're in {name}")
                        } else {
                            format!("You're on {name}")
                        },
                        "Switch away first",
                    );
                }
                // The main checkout is the repository: there is no tree to remove, and git refuses
                // to delete a branch it holds, so neither half of `Ctrl-d` applies.
                if checkout.is_main {
                    return Effects::error_detail(
                        format!("{name} is in the main checkout"),
                        "There's no worktree to remove, and git won't delete a branch it holds",
                    );
                }
                // No confirm dialog, unlike the branch half: here the *refusal* is the
                // confirmation. A first press either removes a clean tree or comes back itemising
                // what a forced removal would destroy, and `Ctrl-Alt-d` is the escalation from
                // having read that. A modal "are you sure?" carrying no facts would only train
                // people to confirm. The escalation rides Alt rather than Shift — the plain/Alt
                // sibling convention the stash picker's `Ctrl-p`/`Ctrl-Alt-p` pair already uses,
                // and the one that survives a terminal, where Ctrl-Shift-D arrives
                // indistinguishable from Ctrl-d.
                let (repo_id, worktree) = (repo_id.clone(), checkout.worktree.clone());
                let force = mods.alt;
                return self.git_worktree_remove(repo_id, worktree, force);
            }
            // `Ctrl-o` — create a worktree for the highlighted branch and **stay put**.
            //
            // Its own key rather than a side effect of Enter, because creating one is a full
            // checkout: a registered, cancellable operation (`GitOperationKind::WorktreeAdd`) that
            // may seed `node_modules` from `.worktreeinclude` and can warn about submodules.
            // Bundled into a navigation key, cancelling it mid-flight leaves "where am I?" with no
            // good answer; separated, the answer is "where you were, with no worktree".
            //
            // `o` because it already means *make a new one* here — `Ctrl-o` opens a line below in
            // Normal mode and a block below in read mode. The hint says "Create worktree" outright
            // rather than leaning on the letter, since `o` reads as "open" to everyone else.
            KeyCode::Char('o') if mods.ctrl && !mods.alt && p.kind == PickerKind::GitBranches => {
                // Everything the create needs, read off the row before the picker borrow is
                // released — `observe_picker_cmd` and `git_worktree_add` both want `&mut self`.
                let create_from_query = p.selected_is_create().then(|| p.query.trim().to_string());
                let row = match p.selected_item() {
                    Some(PickerItem::GitBranch {
                        repo_id,
                        name,
                        checkout,
                        detached_at,
                        ..
                    }) => Some((
                        repo_id.clone(),
                        name.clone(),
                        checkout.clone(),
                        detached_at.is_some(),
                    )),
                    _ => None,
                };
                let observed = self.observe_picker_cmd(PickerCmd::CreateWorktree);

                // The `+ Create` row: the branch doesn't exist either, so make both.
                if let Some(branch) = create_from_query {
                    let Some(repo_id) = self.branch_picker_repo_id() else {
                        return observed;
                    };
                    if branch.is_empty() {
                        return observed.and(Effects::error("Type a branch name to create"));
                    }
                    return observed.and(self.git_worktree_add(repo_id, branch, true));
                }
                let Some((repo_id, name, checkout, detached)) = row else {
                    return observed;
                };
                if let Some(checkout) = checkout {
                    // Already has one. Naming *which* matters: the tree's admin name drifts from
                    // the branch, so "main already has a worktree" would be the wrong sentence for
                    // a row whose tree is called something else entirely.
                    return observed.and(Effects::error(if checkout.is_main {
                        format!("{name} is in the main checkout")
                    } else {
                        format!("{name} is already in worktree {}", checkout.worktree)
                    }));
                }
                // A detached row always carries a checkout, so this is unreachable through it —
                // but a row with no branch has no branch to make a tree for, and saying so beats
                // sending the server a commit id where it expects a ref.
                if detached {
                    return observed;
                }
                return observed.and(self.git_worktree_add(repo_id, name, false));
            }
            // Stash chords, in the Ctrl family the other pickers' row actions use. `Ctrl-p` pops
            // (the gesture you stashed *for*) and `Ctrl-Alt-p` applies — the plain/Alt sibling
            // convention, so the pair needs one letter rather than two unrelated ones.
            KeyCode::Char('p')
                if mods.ctrl && p.kind == PickerKind::GitStash && !p.items.is_empty() =>
            {
                let pop = !mods.alt;
                let Some(PickerItem::GitStash { repo_id, oid, .. }) = p.selected_item() else {
                    return Effects::none();
                };
                let params = GitStashApplyParams {
                    repo_id: Some(repo_id.clone()),
                    buffer_id: None,
                    oid: oid.clone(),
                    pop,
                };
                let hide = self.close_picker();
                return hide.and(self.request_str::<GitStashApply>(params, |result| {
                    Event::StashDone {
                        staged: false,
                        result,
                    }
                }));
            }
            // `Ctrl-d` deletes the highlighted thing, as in every other picker — behind a confirm,
            // because a dropped stash is not something the editor can give back.
            KeyCode::Char('d') if mods.ctrl && !mods.alt && p.kind == PickerKind::GitStash => {
                let Some(PickerItem::GitStash {
                    repo_id,
                    oid,
                    message,
                    ..
                }) = p.selected_item()
                else {
                    return Effects::none();
                };
                let (repo_id, oid, message) = (repo_id.clone(), oid.clone(), message.clone());
                self.prompt = Some(Prompt::Confirm {
                    kind: ConfirmKind::DropStash { message },
                    action: ConfirmAction::DropStash { repo_id, oid },
                });
                return Effects::none();
            }
            KeyCode::Char('d') if mods.ctrl && !mods.alt && p.kind == PickerKind::Views => {
                let fx = self.observe_picker_cmd(PickerCmd::CloseView);
                return fx.and(self.picker_close_view());
            }
            // Ctrl-j: capture the picker's filtered results into the jumplist and jump to the
            // highlighted row — `]`/`[` then step the captured set. Position-shaped kinds only
            // (`captures_to_jumplist`). Safe on the clipboard front (unlike Ctrl-c/v/x/a, GUI query
            // inputs don't claim it) and distinct from Enter in the TUI (crossterm raw mode maps
            // the 0x0A byte to Ctrl-j, not Enter).
            KeyCode::Char('j') if mods.ctrl && !mods.alt && p.kind.captures_to_jumplist() => {
                return self.jumplist_capture();
            }
            // Up/Down recall this picker's query history — grep only, the one kind whose query is a
            // question you re-ask rather than a live filter over a candidate set. They're free here
            // precisely because the *list* moves on Alt-k/j, and they reach the core in every shell
            // (no text input claims a bare arrow-up).
            KeyCode::Up | KeyCode::Down if no_chord && p.kind.history_kind().is_some() => {
                let dir = if code == KeyCode::Up {
                    VerticalDirection::Up
                } else {
                    VerticalDirection::Down
                };
                let kind = p.kind.history_kind().unwrap();
                let current = HistoryEntry {
                    value: p.query.clone(),
                    filters: p.wire_filters(),
                };
                let Some(entry) = self.history_step(kind, dir, current) else {
                    return Effects::none(); // nothing to recall; leave the picker alone
                };
                // Not via `picker_set_query` — that's the shell's typing sync, which abandons the
                // walk we're in the middle of. Install the query *and* the chip row the entry
                // carries (a recall reproduces the search, not just its words), then re-run:
                // `picker_query_changed` sends the query and the freshly adopted filters together,
                // so it's one round-trip. Stepping back off the walk restores the chips you had.
                if let Some(p) = &mut self.picker {
                    p.query = entry.value;
                    p.adopt_filters(&entry.filters);
                }
                return self.picker_query_changed();
            }
            // Alt-k/j move the highlight (Up/Down deliberately don't, matching the others).
            KeyCode::Char('k') if mods.alt && !mods.ctrl => {
                let fx = self.observe_picker_cmd(PickerCmd::MoveSelection);
                return fx.and(self.picker_move(-1));
            }
            KeyCode::Char('j') if mods.alt && !mods.ctrl => {
                let fx = self.observe_picker_cmd(PickerCmd::MoveSelection);
                return fx.and(self.picker_move(1));
            }
            // `Ctrl-g` / `Ctrl-f` in the Explorer: switch to Grep / Files scoped to the
            // browsed directory ("grep here").
            KeyCode::Char('g') if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer => {
                return self.switch_explorer_picker(PickerKind::Grep);
            }
            KeyCode::Char('f') if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer => {
                return self.switch_explorer_picker(PickerKind::Files);
            }
            // Alt-l is one rule in every picker: go one level deeper into the highlighted row.
            // A container descends (Explorer directory, collapsed group); a leaf has no inside,
            // so the level below it is the thing itself and Alt-l opens it — the same act as
            // Enter. That makes `h`/`j`/`k`/`l` a complete picker vocabulary without leaving the
            // home row, and it's the same "accept the highlight" gesture the completing path
            // fields give Alt-l. Enter stays distinct on one row type only: on a group header it
            // resolves server-side to the group's *first item*, where Alt-l descends onto the
            // header's run instead.
            //
            // Alt-l/h used to also mean "jump to the next/previous group" in the header-grouped
            // kinds that don't collapse (References, Keybindings) and "next top-level unit" in
            // DocumentSymbols. That third meaning is gone — those rows open now.
            KeyCode::Char('l') if mods.alt && !mods.ctrl => return self.picker_descend(),
            // Alt-h mirrors it: one level shallower — out of the listed directory, or out of a
            // group onto its header. There's no "un-open", so on a leaf it does nothing. Never
            // the query: the unwind ladder (query, then chips) is Alt-Backspace's alone.
            KeyCode::Char('h') if mods.alt && !mods.ctrl && p.kind == PickerKind::Explorer => {
                return self.explorer_ascend().unwrap_or_else(Effects::none);
            }
            // Alt-h closes the group: from inside a run it collapses that run and puts the
            // highlight on its header; on an expanded header it collapses in place. A collapsed
            // header is as shallow as it goes — a no-op, NOT a query wipe: the unwind (clear
            // query → pop chip) is Alt-Backspace's alone.
            KeyCode::Char('h') if mods.alt && !mods.ctrl && p.collapsible => {
                // The run the highlight is in, from the window's spans rather than the row itself:
                // deep inside a long run its header has scrolled off above, and the leading span
                // is exactly the repeat that covers that case. A collapsed group answers
                // `expanded: false` here and the press does nothing.
                let Some(span) = p.governing_span(p.selected) else {
                    return Effects::none();
                };
                if span.expanded != Some(true) {
                    return Effects::none();
                }
                let header = span.header.clone();
                return self.picker_collapse_group(header);
            }
            // Alt-a toggles every group at once — expand-all, or collapse-all when nothing is
            // collapsed. `a` for "all", unshifted: the Alt-letter family is the one that survives
            // every terminal. A no-op in the flat pickers.
            KeyCode::Char('a') if mods.alt && !mods.ctrl && p.collapsible => {
                return self.picker_toggle_all_groups();
            }
            // Alt-Backspace unwind: clear the query first, then (explorer) step to the parent
            // (one segment per press) into roots mode (multi-root only), then pop chips.
            // Alt-h used to share this and no longer does — it duplicated Alt-Backspace on the
            // query and now only ever ascends (the arms above).
            KeyCode::Backspace if mods.alt && !mods.ctrl => return self.picker_back(),
            // Filter-chip chords. Booleans toggle in place; valued filters open the editor line.
            // Gated per kind inside the helpers.
            KeyCode::Char('c') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Case);
            }
            KeyCode::Char('w') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Word);
            }
            KeyCode::Char('e') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Regex);
            }
            KeyCode::Char('i') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Ignored);
            }
            KeyCode::Char('.') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Hidden);
            }
            KeyCode::Char('m') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Changed);
            }
            KeyCode::Char('u') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(ChipId::Untracked);
            }
            KeyCode::Char('g') if mods.alt && !mods.ctrl => {
                return self.open_glob_prompt(None);
            }
            KeyCode::Char('p') if mods.alt && !mods.ctrl => {
                // Only the kinds that actually have path scopes count as a demonstration —
                // Alt-p is a no-op elsewhere and must not mark the hint used.
                let fx = if matches!(p.kind, PickerKind::Files | PickerKind::Grep) {
                    self.observe_picker_cmd(PickerCmd::AddPathScope)
                } else {
                    Effects::none()
                };
                return fx.and(self.open_dir_prompt(None));
            }
            KeyCode::PageUp => {
                return self.picker_move(-(VISIBLE_ROWS as i64 - 1));
            }
            KeyCode::PageDown => {
                return self.picker_move(VISIBLE_ROWS as i64 - 1);
            }
            // LspServers: Ctrl-r restarts the highlighted server in place.
            KeyCode::Char('r') if mods.ctrl && !mods.alt && p.kind == PickerKind::LspServers => {
                if let Some(PickerItem::LspServer {
                    name,
                    language,
                    workspace_root,
                    ..
                }) = p.selected_item()
                {
                    let (name, language, workspace_root) =
                        (name.clone(), language.clone(), workspace_root.clone());

                    let mut fx = self.request::<LspRestartServer>(
                        LspRestartServerParams {
                            language: language.clone(),
                        },
                        move |__r| {
                            let _ = __r;
                            Event::Noop
                        },
                    );
                    fx.push(self.lsp_restarting_toast(&name, &language, &workspace_root));
                    return fx;
                }
                return Effects::none();
            }
            // `Left` / `Backspace` step into the chip row (rightmost first) — the browser
            // tag-input gesture. In-query caret moves and deletes are owned by each shell's input
            // (which only forwards these from the query start), so reaching the core *is* the
            // boundary: there's nothing to the left but the chips.
            KeyCode::Left | KeyCode::Backspace if no_chord => {
                return self.picker_select_last_chip();
            }
            _ => {}
        }
        // A printable char reaches the core only to land a typed-to-deselect from the chip row (the
        // chip-selected arm above cleared `chip_selected` and fell through); normal query typing is
        // owned by each shell's input and synced via `picker_set_query`. Append it to the query.
        if no_chord {
            if let Some(typed) = text {
                let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
                if !typed.is_empty() {
                    p.query.push_str(&typed);
                    return self.picker_query_changed();
                }
            }
        }
        Effects::none()
    }

    /// Keys while the chip editor line is open. The dir editor reads as one `dir: root: path`
    /// field: Tab / Alt-l accept the focused segment's ghost (root — adopting it and moving
    /// into the path; path — absorbing the next directory segment), `:` on a completed root
    /// value moves into the path, Alt-j/k cycle the focused segment's matches, Alt-Backspace
    /// pops a path segment (then, at an empty path, clears the root selection), and plain
    /// Backspace at an empty path steps back into the root. Enter commits, Esc cancels.
    fn on_chip_editor_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let labels = super::labels::root_labels(&workspace_paths);
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        let is_dir = ed.is_dir();
        let multi_root_dir = is_dir && workspace_paths.len() > 1;
        let in_root = multi_root_dir && ed.field == ChipEditorField::Root;
        let no_chord = !mods.ctrl && !mods.alt;
        // Whether the path field's suggestion listing went stale and needs a directory/list.
        let mut refresh = false;
        match code {
            KeyCode::Enter if no_chord => return self.commit_chip_editor(),
            // Cancel: drop the editor and fall through — `sync_live_filters` reverts the results
            // to the committed chips if a preview was applied.
            KeyCode::Esc => {
                p.chip_editor = None;
            }
            // Tab / Shift-Tab traverse the editor's segments, as they do in every other field
            // (the workspace-settings dialog, the save-as prompt). Traversal only — accepting a
            // suggestion is Alt-l.
            KeyCode::Tab if no_chord && is_dir && in_root => {
                refresh = ed.advance_to_path(&workspace_paths);
            }
            KeyCode::BackTab if multi_root_dir && !in_root => {
                ed.field = ChipEditorField::Root;
            }
            // Alt-l accepts the focused segment's suggestion. Root — adopt the ghost completion and
            // continue right into the path; path — absorb the ghost directory segment (repeated
            // presses walk down the tree).
            KeyCode::Char('l') if mods.alt && !mods.ctrl && is_dir => {
                if in_root {
                    refresh = ed.commit_root_field(&labels, &workspace_paths);
                } else {
                    refresh = ed.accept_path_suggestion(&workspace_paths);
                }
            }
            KeyCode::Char('h') if mods.alt && !mods.ctrl && multi_root_dir => {
                ed.field = ChipEditorField::Root;
            }
            // `:` on a completed root value confirms it and moves into the path — it's the
            // separator you'd type next. On an incomplete value it's swallowed (`:` can
            // never extend a root-label prefix match).
            KeyCode::Char(':') if !mods.ctrl && !mods.alt && in_root => {
                if ed.root_complete(&labels) {
                    refresh = ed.commit_root_field(&labels, &workspace_paths);
                }
            }
            // Alt-Backspace: in the dir editor's path it deletes the rightmost segment,
            // fish-style; at an empty path it clears the root selection (the next rung of
            // the progressive unwind). In the root and glob fields it clears the field.
            KeyCode::Backspace if mods.alt && !mods.ctrl => {
                if is_dir && ed.field == ChipEditorField::Path {
                    if ed.input.text.is_empty() {
                        if multi_root_dir {
                            ed.field = ChipEditorField::Root;
                            ed.root_filter.clear();
                            ed.root_selected = 0;
                        }
                    } else {
                        refresh = ed.pop_path_segment(&workspace_paths);
                    }
                } else if in_root {
                    ed.root_filter.clear();
                    ed.root_selected = 0;
                } else {
                    ed.input.clear();
                }
            }
            // Backspace at an empty path steps back into the root field — the same leftward
            // gesture the chip row uses from the query.
            KeyCode::Backspace
                if no_chord
                    && multi_root_dir
                    && ed.field == ChipEditorField::Path
                    && ed.input.text.is_empty() =>
            {
                ed.field = ChipEditorField::Root;
            }
            // Up/Down recall this field's prior values: globs and paths keep separate lists — a
            // `*.rs` is never a path — and the root typeahead has none (it's a fixed candidate set,
            // cycled with Alt-j/k). The recalled path text is whatever was committed, so it replays
            // through the same listing refresh a typed value would.
            KeyCode::Up | KeyCode::Down if no_chord && !in_root => {
                let dir = if code == KeyCode::Up {
                    VerticalDirection::Up
                } else {
                    VerticalDirection::Down
                };
                let kind = if is_dir {
                    HistoryKind::Path
                } else {
                    HistoryKind::Glob
                };
                let current = HistoryEntry::bare(ed.input.text.clone());
                let Some(entry) = self.history_step(kind, dir, current) else {
                    return Effects::none();
                };
                let Some(ed) = self.picker.as_mut().and_then(|p| p.chip_editor.as_mut()) else {
                    return Effects::none();
                };
                ed.input.set(entry.value);
                let refresh = ed.is_dir() && ed.path_edited(&workspace_paths);
                let mut fx = Effects::none();
                if refresh {
                    fx = fx.and(self.refresh_chip_editor_listing());
                }
                return fx.and(self.sync_live_filters());
            }
            // Cycle the focused segment's matches: root typeahead candidates (wrapping), or
            // the path field's directory suggestions (clamped). Glob: no-op — its recall lives on
            // Up/Down below, like every other overlay input.
            KeyCode::Char(c @ ('j' | 'k')) if mods.alt && !mods.ctrl => {
                let down = c == 'j';
                if in_root {
                    let n = chips::root_candidates(&labels, &ed.root_filter.text).len();
                    if n > 0 {
                        let sel = ed.root_selected.min(n - 1);
                        ed.root_selected = if down {
                            (sel + 1) % n
                        } else {
                            (sel + n - 1) % n
                        };
                        // The chosen root moved — the path now resolves under it.
                        refresh = ed.sync_dir_listing(&workspace_paths);
                    }
                } else if is_dir {
                    ed.cycle_path_suggestion(down);
                }
            }
            // In-field text entry (chars, plain Backspace, Left/Right caret) is owned by each
            // shell's input, which syncs the value via `chip_editor_set_input` /
            // `chip_editor_set_root_filter` (those carry the listing-refresh side effects). The
            // core handles only the command keys above; anything else here is a no-op.
            _ => {
                let _ = text;
            }
        }
        let mut fx = Effects::none();
        if refresh {
            fx = fx.and(self.refresh_chip_editor_listing());
        }
        // A command key may have moved the would-commit value (suggestion accept, segment pop,
        // root cycle) or closed the editor (Esc) — re-run results to match. No-op when nothing
        // changed (focus moves) or while a refetch is mid-flight.
        fx.and(self.sync_live_filters())
    }

    /// Command keys while the save-as prompt is open. Mirrors [`Self::on_chip_editor_key`] — the
    /// editor reads as one `root: path` field: Tab / Alt-l accept the focused segment's ghost,
    /// `:` on a completed root moves into the path, Alt-j/k cycle the focused segment's matches,
    /// Alt-Backspace pops a path segment (then, at an empty path, the root selection), plain
    /// Backspace at an empty path steps back into the root. Enter saves (or, in the root field,
    /// confirms the root and moves on); Esc cancels.
    fn on_save_as_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::SaveAs(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        match path_editor_key(ed, &workspace_paths, code, mods, text) {
            PathEditorKey::Commit => self.commit_save_as(),
            PathEditorKey::Cancel => {
                self.prompt = None;
                Effects::none()
            }
            PathEditorKey::Handled { refresh: true } => self.refresh_save_as_listing(),
            // The prompt *is* the editor — there's no enclosing form, so a Tab off either end has
            // nowhere to go and simply stops.
            PathEditorKey::Handled { refresh: false }
            | PathEditorKey::NextField
            | PathEditorKey::PrevField
            | PathEditorKey::Ignored => Effects::none(),
        }
    }

    /// Keys while the open-from-path prompt is up — the absolute-path twin of
    /// [`Self::on_save_as_key`].
    fn on_open_path_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::OpenPath(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        match path_editor_key(ed, &workspace_paths, code, mods, text) {
            // The empty guard lives *here*, not in `commit_open_path` — that one closes the prompt
            // on its first line, so letting an empty Enter reach it would dismiss the overlay
            // instead of doing nothing. (`open_path_from_os` calls it too, and wants exactly that
            // closing behaviour.)
            PathEditorKey::Commit => match ed.absolute_target() {
                Some(path) => self.commit_open_path(path),
                None => Effects::none(),
            },
            PathEditorKey::Cancel => {
                self.prompt = None;
                Effects::none()
            }
            PathEditorKey::Handled { refresh: true } => self.refresh_open_path_listing(),
            // The prompt *is* the editor — one segment, no enclosing form, so a Tab off either end
            // has nowhere to go and simply stops.
            PathEditorKey::Handled { refresh: false }
            | PathEditorKey::NextField
            | PathEditorKey::PrevField
            | PathEditorKey::Ignored => Effects::none(),
        }
    }

    /// Fire `directory/list` for the open-from-path editor. See
    /// [`Self::refresh_path_editor_listing`], which reads this editor's `Absolute` base to send the
    /// unrestricted listing — the one that works with no workspace active.
    fn refresh_open_path_listing(&mut self) -> Effects {
        self.refresh_path_editor_listing(PathEditorOwner::OpenPath)
    }

    /// Fire `directory/list` for the save-as editor's current (root, dir-portion) pair. The
    /// requested path rides on the result event so a stale response (the editor moved on) can be
    /// discarded. No-op for an invalid root or a closed prompt.
    fn refresh_save_as_listing(&mut self) -> Effects {
        self.refresh_path_editor_listing(PathEditorOwner::SaveAs)
    }

    /// Commit the save-as prompt: save the literal typed path under the chosen root. A leading `/`
    /// re-resolves against the workspace roots; an empty path keeps the prompt open. Closes the
    /// prompt on submit — the overwrite confirm (if any) re-opens it via [`Self::decline_confirm`].
    fn commit_save_as(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::SaveAs(ed)) = self.prompt.as_ref() else {
            return Effects::none();
        };
        let raw = ed.input.text.trim().to_string();
        let relative_target = ed.save_target(&workspace_paths);
        if raw.is_empty() {
            return Effects::none(); // nothing typed — keep the prompt open
        }
        let target = if raw.starts_with('/') {
            match strip_longest_root(&raw, &workspace_paths) {
                Some(target) => target,
                None => {
                    self.prompt = None;
                    return Effects::error_detail(
                        "Outside the workspace",
                        format!("{raw} isn't under any of this workspace's roots"),
                    );
                }
            }
        } else {
            match relative_target {
                Some(target) => target,
                None => return Effects::none(),
            }
        };
        self.prompt = None;
        self.save(Some(target), false, AfterSave::Nothing)
    }

    /// Sync the open-from-path field's value from the shell's input (the shell owns text entry).
    pub fn open_path_set_input(&mut self, text: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(Prompt::OpenPath(ed)) = self.prompt.as_mut() else {
            return Effects::none();
        };
        if ed.input.text == text {
            return Effects::none();
        }
        ed.input.set(text);
        if ed.path_edited(&workspace_paths) {
            self.refresh_open_path_listing()
        } else {
            Effects::none()
        }
    }

    /// Open a file the *operating system* handed us: macOS "Open With" / a Dock drop delivering
    /// `application:openURLs:` to an already-running client, and whatever Linux's desktop
    /// integration eventually sends. `path` is absolute — the OS resolved it.
    ///
    /// Deliberately the same path as the `Space Alt-w` overlay's commit, so a file arriving from the
    /// desktop behaves exactly like one the user typed: a real (non-transient) buffer, with its
    /// workspace context resolved server-side (the workspace that owns the path, or a temporary
    /// context for a file outside every one).
    ///
    /// Closes whatever overlay was up first. That is nearly always the **boot chooser**: the
    /// document and the connection race, and the shell can only park the document while it is still
    /// dialing — win that race and the chooser is already open by the time the file arrives, leaving
    /// a workspace picker sitting over the document the user asked for. Naming a file answers the
    /// question the chooser is asking, so it dismisses rather than layers.
    ///
    /// Not [`Self::open_path_at`]: that opens a *transient preview* for result-style navigation
    /// (picker rows, goto-definition), which would evaporate the moment the buffer was hidden. A
    /// file someone deliberately opened from their file manager is not a preview.
    pub fn open_path_from_os(&mut self, path: String) -> Effects {
        let fx = self.close_picker();
        fx.and(self.commit_open_path(path))
    }

    /// Submit the open-from-path overlay: open `path` (absolute, or a leading `~/`) via
    /// `workspace/open_path`. The server resolves the workspace context — internal if it's under
    /// the active workspace's roots, an external buffer if not, a fresh ephemeral context if no
    /// workspace is active. The result lands like a workspace switch (adopt the workspace + buffer); the
    /// path field is already non-empty (checked by the caller).
    fn commit_open_path(&mut self, path: String) -> Effects {
        self.prompt = None;
        self.request_str::<WorkspaceOpenPath>(
            WorkspaceOpenPathParams {
                path,
                transient: None,
                // The overlay stays existing-files-only (a typo'd path should error readably,
                // not silently mint a buffer); the CLI boot is the create route.
                create_if_missing: false,
                // Typed by hand, with no position to carry: open where the file was left.
                jump_to: None,
            },
            |r| {
                Event::WorkspaceActivated(r.and_then(|a| {
                    a.opened
                        .map(|open| (a.workspace, open))
                        .ok_or_else(|| "open_path returned no view".into())
                }))
            },
        )
    }

    /// Expand `header`'s group in the open collapsible picker and move into it (`Alt-l`, a click
    /// on a collapsed header). Idempotent — on an already-open group it just re-focuses it, which
    /// is what keeps `Alt-l` making progress on a header the focus has drifted off.
    fn picker_expand_group(&mut self, header: GroupHeader, landing: GroupLanding) -> Effects {
        self.picker_set_group(PickerGroupAction::Expand { header }, landing)
    }

    /// Collapse `header`'s group and put the selection back on its header (`Alt-h`, a click on an
    /// expanded header).
    fn picker_collapse_group(&mut self, header: GroupHeader) -> Effects {
        self.picker_set_group(PickerGroupAction::Collapse { header }, GroupLanding::Header)
    }

    /// Focus the group adjacent to the focused one, server-resolved so it works when the neighbour
    /// sits past the fetched window. Serves both group-level `Alt-j`/`Alt-k` (`landing: Header`,
    /// expansion untouched) and the item-level spill over a run edge (`RunStart`/`RunEnd` — walk
    /// into the neighbouring group's items, which has to open it and leaves the run we came from
    /// open). A step off the ends answers `run: None` — a stop, and [`Event::GroupSet`] leaves the
    /// selection alone.
    fn picker_step_group(&mut self, direction: Direction, landing: GroupLanding) -> Effects {
        let expand = landing != GroupLanding::Header;
        self.picker_set_group(PickerGroupAction::Step { direction, expand }, landing)
    }

    /// One `picker/set_group` gesture. The response's run geometry lands as [`Event::GroupSet`]
    /// and seats the selection per `landing`; the server's reshaped window arrives through the
    /// normal push path (offset-guarded, order-independent).
    fn picker_set_group(&mut self, action: PickerGroupAction, landing: GroupLanding) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.group_gesture_in_flight = true;
        let kind = p.kind;
        self.request::<PickerSetGroup>(PickerSetGroupParams { kind, action }, move |__r| {
            Event::GroupSet(__r.map(|r| r.run).map_err(|e| e.message), landing)
        })
    }

    /// `Alt-a`: expand every group, or collapse every group when none is collapsed (the server
    /// picks the direction — only it sees the whole run list). The selection stays where it is:
    /// the landing re-seats it at the same offset into the focused run, which the reshaped row
    /// space has usually moved.
    fn picker_toggle_all_groups(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if !p.collapsible {
            return Effects::none();
        }
        // How far into the focused run the selection sits, so the reply can put it back there.
        // `None` on a header: it stays on the header, wherever the reshape moved it to.
        let offset = p
            .selection_at_item_level()
            .then(|| p.focus_item_rows().map(|(first, _)| p.selected - first))
            .flatten();
        self.picker_set_group(PickerGroupAction::ToggleAll, GroupLanding::Keep { offset })
    }

    /// Reveal the selection after a row move — `Reveal::Minimal` for the local two-level
    /// local row moves, `Reveal::Run` for a group step onto an open run (frame the whole run) —
    /// chasing it with a window refetch when it sits outside the fetched window. Fires the reveal
    /// both immediately and re-armed for the next push (`reveal_on_update`), so a reshaping window
    /// that hasn't landed yet still gets revealed against fresh geometry when it does.
    fn picker_reveal_selection(&mut self, reveal: Reveal) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.reveal_on_update = Some(reveal);
        let in_window = p.selected >= p.offset && p.selected < p.offset + p.items.len() as u32;
        let fx = Effects::one(Effect::RevealPickerSelection(reveal));
        if !in_window && !p.refetch_in_flight {
            let offset = p.selected.saturating_sub(FETCH_LIMIT / 2);
            return fx.and(self.picker_refetch(offset, true));
        }
        fx
    }

    /// `Alt-l`: one level deeper into the highlighted row. Descends where the row has an inside
    /// — an Explorer directory (or root), a collapsible kind's group — and otherwise opens it
    /// through [`Self::picker_accept`], since a leaf's only "deeper" is the thing itself.
    ///
    /// The one row Alt-l won't take is the synthetic "+ Create …": creating a file or a
    /// workspace is a deliberate act, and Alt-l is a navigation chord sitting next to Alt-j/k,
    /// where an overshoot would make something on disk. Enter keeps that gesture to itself.
    fn picker_descend(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.selected_is_create() {
            return Effects::none();
        }
        if p.kind == PickerKind::Explorer {
            // A directory / root descends; a file falls through and opens.
            if matches!(
                p.selected_item(),
                Some(PickerItem::DirEntry { is_dir: true, .. }) | Some(PickerItem::Root { .. })
            ) {
                return self.explorer_enter_selected();
            }
        } else if p.collapsible && !p.selection_at_item_level() {
            // Open the highlighted group and move onto its first item. Always the round trip, even
            // when the group already shows its items: the reply carries the run's geometry in the
            // reshaped row space, which is what seats the selection — and expanding is idempotent,
            // so the same press also re-focuses a group the focus has drifted off (after a re-rank
            // clamped the selection onto another header), which keeps each press making progress.
            if let Some(PickerItem::Group { header, .. }) = p.selected_item() {
                let header = header.clone();
                return self.picker_expand_group(header, GroupLanding::RunStart);
            }
        }
        self.picker_accept()
    }

    /// Apply a server notification to the session. Stale pushes (other viewports/buffers,
    /// older picker generations) are discarded per the protocol.
    fn on_server_push(&mut self, n: Notification) -> Effects {
        match n.method.as_str() {
            ViewportLinesChanged::NAME => {
                let Ok(p) = serde_json::from_value::<ViewportLinesChangedParams>(n.params) else {
                    return Effects::none();
                };
                if Some(p.viewport_id) != self.view.viewport_id {
                    return Effects::none();
                }
                // The notification carries the freshly rendered window for the loaded range
                // — apply it directly, keep the revision fresh (edits that only arrive this
                // way, e.g. another client's), and keep the cursor in view under the new
                // geometry (the shell clamps + reveals).
                //
                // `p.buffer` is the focused element's buffer as the server saw it — the one buffer
                // this client tracks a revision for. It can still differ from ours: a push rendered
                // before a focus move landed names the element focus just left, and filing its
                // revision against the new buffer would make the next push for it look stale.
                if p.buffer == self.view.buffer.buffer_id {
                    self.view.buffer.revision = p.revision;
                }
                // Server-side cursor moves with no request in flight (e.g. the clamp a watcher
                // reload applies when the file shrank under the cursor) ride the push; adopt
                // before the shells reveal against the new window.
                if let Some(cursor) = p.cursor {
                    self.view.buffer.cursor = cursor;
                }
                self.replace_window(p.window);
                // A reader's lines are all in the window, so this is also its re-parse.
                let read_fx = self.sync_read_presentation();
                Effects::one(Effect::WindowAdopted).and(read_fx)
            }
            GitBlameChanged::NAME => {
                // The blame-follow push (`git/set_blame_follow`): the settled cursor line's
                // blame. Stale pushes for a previous buffer are discarded; a `None` blame
                // clears the label (no repo / untracked / past EOF).
                let Ok(p) = serde_json::from_value::<GitBlameChangedParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id == self.view.buffer.buffer_id {
                    self.view.blame = p.blame.map(|b| (p.line, b));
                }
                Effects::none()
            }
            aether_protocol::agent::AgentTurnChanged::NAME => {
                use aether_protocol::agent::AgentTurnChangedParams;
                let Ok(p) = serde_json::from_value::<AgentTurnChangedParams>(n.params) else {
                    return Effects::none();
                };
                let finished = p.turn.clone().filter(|t| !t.running);
                match p.turn.filter(|t| t.running) {
                    Some(turn) => {
                        self.agent_turns.insert(p.view_id, turn);
                    }
                    None => {
                        self.agent_turns.remove(&p.view_id);
                    }
                }
                // Say how it went only when you are looking somewhere else, and only when there is
                // something to say: a turn that simply ended is not news, and the conversation on
                // screen shows its own state. Grouped by view so one conversation replaces its own
                // notice rather than stacking a column of them.
                match finished
                    .filter(|_| p.view_id != self.view.view_id)
                    .and_then(|t| t.stop_reason)
                    .filter(|r| r.is_notable())
                {
                    Some(reason) => Effects::toast_grouped_detail(
                        "Agent".to_string(),
                        reason.label(),
                        ToastKind::Warning,
                        format!("agent-{}", p.view_id.get()),
                    ),
                    None => Effects::none(),
                }
            }
            aether_protocol::shell::ShellRunChanged::NAME => {
                use aether_protocol::shell::ShellRunChangedParams;
                let Ok(p) = serde_json::from_value::<ShellRunChangedParams>(n.params) else {
                    return Effects::none();
                };
                let finished = p.run.clone().filter(|r| !r.is_running());
                match p.run.filter(|r| r.is_running()) {
                    Some(run) => {
                        self.shell_runs.insert(p.view_id, run);
                    }
                    None => {
                        self.shell_runs.remove(&p.view_id);
                    }
                }
                // Say how it went only when you are looking somewhere else: a shell on screen has
                // the outcome in the run's own header, and a toast repeating it is noise. Grouped
                // by view so a shell you keep re-running replaces its own notice rather than
                // stacking a column of them.
                match finished.filter(|_| p.view_id != self.view.view_id) {
                    Some(run) => Effects::toast_grouped_detail(
                        run.command.clone(),
                        run.status.label(),
                        match run.status {
                            aether_protocol::shell::RunStatus::Exited { code: 0 } => {
                                ToastKind::Success
                            }
                            _ => ToastKind::Warning,
                        },
                        format!("shell-{}", p.view_id.get()),
                    ),
                    None => Effects::none(),
                }
            }
            GitOperationChanged::NAME => {
                // A long-running git operation started, advanced, or finished. Repo-scoped and
                // pushed to every client, so this is adopted regardless of which buffer is
                // showing — the operation isn't a property of the buffer that started it.
                let Ok(p) = serde_json::from_value::<GitOperationChangedParams>(n.params) else {
                    return Effects::none();
                };
                self.git_operation = p.operation.map(|op| (p.repo_id, op));
                Effects::none()
            }
            BufferChanged::NAME => {
                // The revision-only change signal for edits outside the pushed window. Rendering
                // ignores it (the window on screen is untouched by an out-of-window edit), and a
                // reader has no outside — its element is loaded whole, so its edits arrive as
                // windows.
                let Ok(p) = serde_json::from_value::<BufferChangedParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id == self.view.buffer.buffer_id {
                    self.view.buffer.revision = p.revision;
                }
                Effects::none()
            }
            aether_protocol::view::ViewState::NAME => {
                // Transience is the view's, so it arrives addressed by view id: a file's reader
                // and its editor are kept and dropped independently, and only the view this
                // client presents has an answer we can apply.
                if let Ok(p) = serde_json::from_value::<ViewStateParams>(n.params) {
                    if p.view_id == self.view.view_id {
                        self.view.view_transient = p.transient;
                    }
                }
                Effects::none()
            }
            BufferState::NAME => {
                let Ok(p) = serde_json::from_value::<BufferStateParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id != self.view.buffer.buffer_id {
                    return Effects::none();
                }
                self.view.buffer.saved_revision = p.saved_revision;
                // A save-as renames the shared buffer; follow it — adopt the new path and re-derive
                // the label. Only on an actual change, so in-place save/reload pushes are no-ops
                // (and a legacy server omitting `path` never clobbers our label).
                if let Some(new_path) = p.path {
                    if self.view.buffer.path.as_deref() != Some(new_path.as_str()) {
                        let label =
                            super::session::label_for_path(&new_path, &self.workspace_paths);
                        self.view.relabel_focused(label);
                        self.view.buffer.path = Some(new_path);
                    }
                }
                let was_external = self.view.externally_modified || self.view.externally_deleted;
                self.view.externally_modified = p.externally_modified;
                self.view.externally_deleted = p.externally_deleted;
                // Grouped per buffer: a deleted-then-modified (or repeated) disk event updates the
                // one external-change toast rather than stacking.
                let group = format!("external-change:{}", self.view.buffer.buffer_id);
                if !was_external && p.externally_deleted {
                    Effects::toast_grouped_detail(
                        "File removed on disk",
                        "Save to recreate it, or close the view",
                        ToastKind::Warning,
                        group,
                    )
                } else if !was_external && p.externally_modified {
                    Effects::toast_grouped_detail(
                        "File changed on disk",
                        "Save to overwrite it, or reload",
                        ToastKind::Warning,
                        group,
                    )
                } else {
                    Effects::none()
                }
            }
            LspDiagnosticsChanged::NAME => {
                if let Ok(p) = serde_json::from_value::<LspDiagnosticsChangedParams>(n.params) {
                    if p.buffer_id == self.view.buffer.buffer_id {
                        self.view.diagnostics = p.counts;
                    }
                }
                Effects::none()
            }
            LspSymbolPathChanged::NAME => {
                if let Ok(p) = serde_json::from_value::<LspSymbolPathChangedParams>(n.params) {
                    // Buffer-guarded like the diagnostics push: a path for the buffer we just
                    // switched away from would otherwise label the new one.
                    if p.buffer_id == self.view.buffer.buffer_id {
                        self.view.symbol_path = p.path;
                    }
                }
                Effects::none()
            }
            // Another client in this context captured or cleared the jumplist out from under our
            // open Jumplist picker. Re-view rather than patching rows in: a capture can flip the
            // list between grouped and flat (and in or out of path-scopeable), and both gates ride
            // the view response, not the push. The selection goes back to the top because this is a
            // *different* list — holding row 7 across the swap would point at nothing meaningful.
            JumplistChanged::NAME => match &mut self.picker {
                Some(p) if p.kind == PickerKind::Jumplist => {
                    p.selected = 0;
                    p.level = PickerLevel::Group;
                    self.picker_refetch(0, false)
                }
                _ => Effects::none(),
            },
            PickerUpdate::NAME => {
                if let Ok(u) = serde_json::from_value::<PickerUpdateParams>(n.params) {
                    let mut reveal = None;
                    if let Some(p) = &mut self.picker {
                        // A server-resolved highlight (DocumentSymbols' cursor-enclosing symbol on
                        // the async fill) rides the push as `center_on`; `apply_update` adopts it —
                        // together with the window the server re-framed around it — behind its
                        // staleness guards, exactly like the view response's `effective_center_on`.
                        // A rejected push is genuinely stale: shells deliver server messages in
                        // wire order, so a push can't outrun the view response that establishes the
                        // slot's generation.
                        if p.apply_update(u) && p.pending_center.is_none() {
                            reveal = p.reveal_on_update.take();
                        }
                    }
                    // The LSP picker refresh carries live progress (`report`s don't fire
                    // `lsp/status_changed`); fold it into an open LSP dialog so its "Working" line
                    // tracks the percentage instead of freezing at the opening snapshot.
                    self.sync_lsp_dialog_from_picker();
                    if let Some(reveal) = reveal {
                        return Effects::one(Effect::RevealPickerSelection(reveal));
                    }
                }
                Effects::none()
            }
            SearchStateChanged::NAME => {
                // Matches recomputed (buffer edit) or the cursor crossed a match boundary.
                if let Ok(s) = serde_json::from_value::<SearchSummary>(n.params) {
                    if s.buffer_id == self.view.buffer.buffer_id
                        && (self.view.search.active || self.view.mode == Mode::Search)
                    {
                        self.view.search.summary = Some(s);
                    }
                }
                Effects::none()
            }
            LspStatusChanged::NAME => {
                let Ok(s) = serde_json::from_value::<LspServerStatus>(n.params) else {
                    return Effects::none();
                };
                let matches_current = self.view.buffer.lsp_server.as_ref().is_some_and(|r| {
                    r.language == s.language && r.workspace_root == s.workspace_root
                });
                // Live-update an open LSP info dialog for the same server, so a restart's
                // Restarting → Ready transition shows in place without reopening it.
                let matches_dialog = matches!(
                    self.prompt.as_ref(),
                    Some(Prompt::LspInfo(info))
                        if info.language == s.language
                            && info.workspace_root == s.workspace_root
                );
                if matches_dialog {
                    self.prompt = Some(Prompt::LspInfo(Box::new(s.clone())));
                }
                // Resolve a pending restart (issued via `Ctrl-r`): the server reaching a terminal
                // state ends the lifecycle, so replace its "Restarting" toast in place. Gated on the
                // pending set so an ordinary busy→idle `status_changed` blip doesn't toast.
                let group = crate::session::lsp_toast_group(&s.language, &s.workspace_root);
                let restart_toast = if self.lsp_restart_pending.contains(&group) {
                    use aether_protocol::lsp::LspStatus;
                    match &s.status {
                        LspStatus::Ready => {
                            self.lsp_restart_pending.remove(&group);
                            Some(Effect::Toast {
                                title: format!("{} restarted", s.name),
                                body: None,
                                kind: ToastKind::Success,
                                group: Some(group.clone()),
                            })
                        }
                        LspStatus::Crashed { .. } | LspStatus::Stopped => {
                            self.lsp_restart_pending.remove(&group);
                            Some(Effect::Toast {
                                title: format!("{} failed to restart", s.name),
                                body: None,
                                kind: ToastKind::Error,
                                group: Some(group.clone()),
                            })
                        }
                        // Starting / Initializing / Restarting: still in flight — keep waiting.
                        _ => None,
                    }
                } else {
                    None
                };
                if matches_current {
                    self.view.lsp = Some(s);
                }
                restart_toast.map_or_else(Effects::none, Effects::one)
            }
            ViewClosed::NAME => {
                // Another client (or a path/workspace deletion) closed a view; if it's ours,
                // switch to the server-indicated next view (or a fresh scratch).
                let Ok(p) = serde_json::from_value::<ViewClosedParams>(n.params) else {
                    return Effects::none();
                };
                // The buffer went with the view when the view was its last. The tether closed out
                // from under us: this client's job is over, however the close happened (the
                // future `ae --web file` waiter rides this). And however it went, the message
                // buffer is gone — so is the commit it was for.
                if let Some(buffer_id) = p.buffer_id {
                    if self.tether == Some(buffer_id) {
                        return Effects::one(Effect::Exit);
                    }
                    self.forget_commit_buffer(buffer_id);
                }
                if p.view_id != self.view.view_id {
                    return Effects::none();
                }
                let moved = std::mem::take(&mut self.workspace_moved_under_us);
                let fx = if moved {
                    // The file is the same file; the tree under it changed. The roots are already
                    // adopted and the branch indicator says which tree — a warning here would be
                    // describing a problem that isn't one.
                    Effects::none()
                } else {
                    Effects::toast("View closed by another client", ToastKind::Warning)
                };

                // In an ephemeral context, don't fall back to a fresh scratch when nothing remains
                // — leave the context, same as closing it ourselves (see `close_view`). This is
                // the multi-client case: another client closed the shared external file we were
                // both viewing, and there's no other buffer in this throwaway context to land on.
                if aether_protocol::is_ephemeral_workspace_id(&self.workspace)
                    && p.next_view_id.is_none()
                {
                    return fx.and(self.leave_ephemeral_workspace());
                }

                // Prefer the path when the server named one: a worktree rebind hands over the
                // same file on the new tree, and a path is the only stable way to say that — the
                // id it could offer is a dormant placeholder the initiating client's own landing
                // buffer may already have materialised under a different id.
                let (view_id, path_index, relative_path) = match p.next_path {
                    Some(loc) => (None, Some(loc.path_index), Some(loc.relative_path)),
                    None => (p.next_view_id, None, None),
                };
                fx.and(self.request::<ViewOpen>(
                    ViewOpenParams {
                        view_id,
                        path_index,
                        relative_path,
                        // Nothing left to land on: a placeholder, not a scratch to keep.
                        transient: (view_id.is_none() && path_index.is_none()).then_some(true),
                        ..Default::default()
                    },
                    move |__r| Event::Switched(__r.map_err(|e| e.message)),
                ))
            }
            aether_protocol::workspace::WorkspaceChanged::NAME => {
                // Another client changed the shape of the workspace we are standing in — today,
                // by rebinding a worktree, which moves every root at once. Adopt it before the
                // `view/closed` that follows makes us open something under the new roots: paths
                // are rendered against this list, so a stale one mislabels everything it resolves.
                let Ok(info) = serde_json::from_value::<WorkspaceInfo>(n.params) else {
                    return Effects::none();
                };
                self.sync_workspace_info(info);
                // The `view/closed` pushes that follow a shape change are not "another client
                // closed your file" — the workspace moved and took its buffers with it. Consumed by
                // the next close so it words itself correctly. One-shot and best-effort: a shape
                // change that closes nothing leaves it set, which at worst silences one later
                // close's toast while still switching correctly.
                self.workspace_moved_under_us = true;
                Effects::none()
            }
            WorkspaceRenamed::NAME => {
                // Another client renamed our active workspace. The server already re-keyed our
                // server-side state; adopt the new name locally so the display and the reconnect
                // baseline (reconnect is by name) follow.
                let Ok(p) = serde_json::from_value::<WorkspaceRenamedParams>(n.params) else {
                    return Effects::none();
                };
                if self.workspace != p.old_name {
                    return Effects::none();
                }
                self.workspace = p.new_name.clone();
                // Keep an open settings overlay's committed name in step too, or its next commit
                // would target the stale name.
                if let Some(s) = self.workspace_settings.as_mut() {
                    if s.workspace_name == p.old_name {
                        s.workspace_name = p.new_name.clone();
                        s.name.set(p.new_name.clone());
                    }
                }
                Effects::toast(
                    format!("Workspace renamed to {}", p.new_name),
                    ToastKind::Info,
                )
            }
            SettingsChanged::NAME => {
                // Another client changed the global app settings. Apply them live (the same reflow
                // path the boot fetch uses); an open app-settings overlay re-renders from the new
                // `Session` state. A quiet toast explains the otherwise-spontaneous reflow.
                let Ok(settings) = serde_json::from_value::<AppSettings>(n.params) else {
                    return Effects::none();
                };
                let mut fx = self.apply_app_settings(settings);
                fx.push(Effect::Toast {
                    title: "Settings updated".to_string(),
                    body: None,
                    kind: ToastKind::Info,
                    group: None,
                });
                fx
            }
            _ => Effects::none(),
        }
    }

    // ---- search ----------------------------------------------------------------------------

    /// `/` or `?`: open the search prompt. Snapshots cursor/query/options for Esc-restore (the
    /// shell anchors its scroll via the effect) and clears the server-side search so stale
    /// highlights disappear immediately.
    ///
    /// The prompt opens at its defaults — empty query *and* default match options — the same way
    /// every picker opens at [`PickerReset::All`]. Options used to be sticky across `/` presses,
    /// but a case or regex toggle left over from an earlier search silently changes what the next
    /// one matches; `Up` recalls a past query together with the options it ran under
    /// when you do want the old configuration back. The snapshot is
    /// taken *before* the reset, so Esc still restores a committed search exactly as it was.
    pub fn enter_search(&mut self, extend_to_cursor: bool) -> Effects {
        self.view.search.snapshot = Some(SearchSnapshot {
            cursor: self.view.buffer.cursor,
            query: std::mem::take(&mut self.view.search.query),
            active: self.view.search.active,
            options: self.view.search.options,
        });
        self.view.search.options = MatchOptions::default();
        self.view.search.active = false;
        self.view.search.summary = None;
        self.history.reset();
        self.view.search.chip_selected = None;
        self.view.search.extend_to_cursor = extend_to_cursor;
        self.view.mode = Mode::Search;

        let mut fx = Effects::one(Effect::SaveScrollAnchor);
        fx = fx.and(self.request::<SearchClear>(
            SearchClearParams {
                buffer_id: self.view.buffer.buffer_id,
            },
            move |__r| {
                let _ = __r;
                Event::Noop
            },
        ));
        fx
    }

    /// One incremental step: hand the server the latest query; it jumps the cursor to the
    /// first match at-or-after the prompt's entry point. An emptied query clears instead.
    fn incremental_search(&mut self) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        if self.view.search.query.is_empty() {
            self.view.search.summary = None;

            let fx = self.request::<SearchClear>(SearchClearParams { buffer_id }, move |__r| {
                let _ = __r;
                Event::Noop
            });
            let revert = self.revert_to_snapshot_cursor();
            return fx.and(revert);
        }

        self.request::<SearchSet>(
            SearchSetParams {
                buffer_id,
                query: self.view.search.query.clone(),
                anchor: self
                    .view
                    .search
                    .snapshot
                    .as_ref()
                    .map(|s| min_pos(s.cursor.position, s.cursor.anchor)),
                extend: self.view.search.extend_to_cursor,
                from_selection: false,
                options: self.view.search.options,
            },
            move |__r| Event::SearchApplied(__r.map_err(|e| e.message)),
        )
    }

    /// Move the cursor back to where the prompt opened (no-op outside incremental search or
    /// when it hasn't moved).
    fn revert_to_snapshot_cursor(&mut self) -> Effects {
        let Some(snap) = self.view.search.snapshot.as_ref() else {
            return Effects::none();
        };
        if self.view.buffer.cursor.position == snap.cursor.position
            && self.view.buffer.cursor.anchor == snap.cursor.anchor
        {
            return Effects::none();
        }

        self.request::<CursorSet>(
            CursorSetParams {
                buffer_id: self.view.buffer.buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
            move |__r| Event::CursorMsg(__r.map_err(|e| e.message)),
        )
    }

    // ---- pointer (mouse) -----------------------------------------------------------------
    //
    // Geometry (screen cell → buffer position) is the shell's job — only the shell knows its
    // viewport/scroll. The core owns the selection semantics: the drag anchor, the click-streak
    // granularity, and the `cursor/set` round-trip. Shared by every shell so click/drag behaves
    // identically across terminal, native, and web.

    /// A pointer press at an already-resolved buffer position. `granularity` carries the click
    /// streak — `Char`/`Word`/`Line` for single/double/triple — and the server expands the
    /// selection to that unit. `extend` (shift-click) keeps the current anchor instead of
    /// collapsing the selection to the press. Records the drag anchor so a follow-up
    /// [`pointer_drag`](Self::pointer_drag) extends from here.
    pub fn pointer_press(
        &mut self,
        element: aether_protocol::viewport::FieldId,
        pos: LogicalPosition,
        granularity: Granularity,
        extend: bool,
    ) -> Effects {
        // A click names an element, and everything that follows from the press — the cursor it
        // sets, the drag it anchors, the buffer both act on — belongs to *that* element. Focusing it
        // is therefore part of the press rather than something each shell remembers to do first: the
        // terminal did and the GUI didn't, so in the GUI a click outside the focused element set a
        // cursor the server then bounded back into the element that still had focus, and clicking
        // only ever worked in one of them. A no-op when the element is already focused.
        //
        // Emitted before the cursor's own request, and the shells send requests in emission order,
        // so the server has moved focus by the time the `element/set` runs.
        let focus = self.focus_clicked_element(element);
        let anchor = if extend {
            self.view.buffer.cursor.anchor
        } else {
            pos
        };
        self.view.drag = Some((element, anchor, granularity));
        // A pointer selection is a Normal-mode concept. Double/triple-click (Word/Line) and
        // shift-click create a selection immediately, and a selection can't coexist with the
        // insert-mode bar caret: the selection's endpoint is an inclusive char, the caret is the
        // gap before it, so the two render in different places. Drop to Normal so the block cursor
        // sits on the endpoint. A plain single click stays in Insert — it only repositions the
        // caret (a point cursor, no selection).
        if self.view.mode == Mode::Insert && (extend || granularity != Granularity::Char) {
            self.view.mode = Mode::Normal;
        }
        focus.and(self.request_str::<CursorSet>(
            CursorSetParams {
                buffer_id: self.element_buffer(element),
                position: pos,
                anchor,
                granularity,
            },
            Event::CursorMsg,
        ))
    }

    /// The buffer an element of this view windows, falling back to the buffer the view is bound to
    /// when the element isn't in the tree (a rebuild between the click and its handling).
    ///
    /// A click names an element, and the position it resolved to is a line of **that element's**
    /// buffer. Sending it to whichever buffer the view happened to be bound to is a line number
    /// applied to a different file: it lands the cursor somewhere arbitrary in the buffer being left,
    /// while the one just clicked keeps the position focus had seated. Focus reconciles a beat later
    /// — the reply is a round trip behind — so the press cannot wait for it and must name the buffer
    /// itself. The requests are ordered, so the server has already moved focus by the time this one
    /// runs.
    fn element_buffer(&self, element: aether_protocol::viewport::FieldId) -> BufferId {
        self.view
            .window
            .as_ref()
            .and_then(|w| {
                w.root.editors().iter().find_map(|node| match node {
                    aether_protocol::viewport::Element::Editor {
                        element: id,
                        buffer,
                        ..
                    } if *id == element => Some(*buffer),
                    _ => None,
                })
            })
            .unwrap_or(self.view.buffer.buffer_id)
    }

    /// Pointer drag to a new position while the button is held: extend the selection from the
    /// recorded anchor, preserving the press's granularity. A no-op when no press is active (the
    /// drag began outside the text, or the press was suppressed).
    pub fn pointer_drag(&mut self, pos: LogicalPosition) -> Effects {
        let Some((element, anchor, granularity)) = self.view.drag else {
            return Effects::none();
        };
        // Dragging is a selection gesture: once it covers more than the press anchor it's a real
        // selection, so leave Insert for the same reason as `pointer_press`. (Word/Line drags
        // already switched at press time; this catches the Char-granularity drag.)
        if self.view.mode == Mode::Insert && pos != anchor {
            self.view.mode = Mode::Normal;
        }
        self.request_str::<CursorSet>(
            CursorSetParams {
                // The element the *press* landed in: a drag belongs to the selection it started,
                // and the focus reply for that press may still be in flight.
                buffer_id: self.element_buffer(element),
                position: pos,
                anchor,
                granularity,
            },
            Event::CursorMsg,
        )
    }

    /// Pointer release — ends the drag. The selection stays as last set.
    pub fn pointer_release(&mut self) {
        self.view.drag = None;
    }

    /// Esc in the prompt: restore the pre-prompt search (query + server state), cursor, and
    /// (via the effect) the shell's scroll anchor.
    pub fn abort_search(&mut self) -> Effects {
        self.view.mode = self.search_return_mode();
        self.view.search.extend_to_cursor = false;
        self.history.reset();
        self.view.search.chip_selected = None;
        let Some(snap) = self.view.search.snapshot.take() else {
            return Effects::none();
        };
        let buffer_id = self.view.buffer.buffer_id;
        let mut fx = if snap.active && !snap.query.is_empty() {
            self.request::<SearchSet>(
                SearchSetParams {
                    buffer_id,
                    query: snap.query.clone(),
                    anchor: None,
                    extend: false,
                    from_selection: false,
                    options: snap.options,
                },
                move |__r| Event::SearchRestored(__r.map_err(|e| e.message)),
            )
        } else {
            self.view.search.summary = None;

            self.request::<SearchClear>(SearchClearParams { buffer_id }, move |__r| {
                let _ = __r;
                Event::Noop
            })
        };
        self.view.search.query = snap.query;
        self.view.search.active = snap.active;
        self.view.search.options = snap.options;

        fx = fx.and(self.request::<CursorSet>(
            CursorSetParams {
                buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
            move |__r| Event::CursorMsg(__r.map_err(|e| e.message)),
        ));
        fx.push(Effect::RestoreScrollAnchor);
        fx
    }

    /// Enter in the prompt: keep the query as the committed search. Commit is also what makes the
    /// query recallable — the incremental preview types a new query on every keystroke, so
    /// recording anything earlier would fill the history with prefixes.
    pub fn commit_search(&mut self) -> Effects {
        self.view.search.snapshot = None;
        let mut fx = Effects::none();
        if self.view.search.query.is_empty() {
            self.view.search.active = false;
            self.view.search.summary = None;
        } else {
            self.view.search.active = true;
            let entry = HistoryEntry::with_options(
                self.view.search.query.clone(),
                self.view.search.options,
            );
            fx = self.record_history(HistoryKind::Search, entry);
        }
        self.history.reset();
        self.view.search.extend_to_cursor = false;
        self.view.search.chip_selected = None;
        self.view.mode = self.search_return_mode();
        fx
    }

    /// The mode leaving the search prompt returns to: Read when the buffer is displayed as a
    /// reading view (search entered from Read returns to Read), Normal otherwise.
    fn search_return_mode(&self) -> Mode {
        if self.view.read.is_some() {
            Mode::Read
        } else {
            Mode::Normal
        }
    }

    /// `n`/`Alt-n`: step match-to-match; with no active search, revive the most recent
    /// history entry first. Steps run sequentially in one future.
    pub fn search_cycle(&mut self, direction: Direction, count: u32, extend: bool) -> Effects {
        let revive = if self.view.search.active {
            None
        } else {
            // Revive the newest entry *with its match options* — the revived query rides the nav
            // RPC below alongside `self.view.search.options`, and a regex revived as a literal would
            // quietly match nothing.
            match self.history.list(HistoryKind::Search).last().cloned() {
                Some(entry) => {
                    self.view.search.query = entry.value.clone();
                    self.view.search.options = entry.filters.match_options();
                    self.view.search.active = true;
                    Some(entry.value)
                }
                None => return Effects::none(),
            }
        };
        // Revive + count ride the nav RPC itself: the server re-sets the query first (skipping the
        // step when it has no matches), then steps `count` times.
        self.request_str::<SearchStep>(
            SearchStepParams {
                buffer_id: self.view.buffer.buffer_id,
                direction,
                extend,
                count,
                set_query: revive,
                options: self.view.search.options,
            },
            Event::SearchNav,
        )
    }

    /// `Alt-/`: search for the selected text, literally — the server derives and escapes the query
    /// from its own selection state.
    pub fn search_from_selection(&mut self) -> Effects {
        self.request_str::<SearchSet>(
            SearchSetParams {
                buffer_id: self.view.buffer.buffer_id,
                query: String::new(),
                anchor: None,
                extend: false,
                from_selection: true,
                // "Find this text" runs at the defaults, like a prompt open ([`Self::enter_search`]):
                // literal (the server matches the raw selection text), smartcase, no whole-word.
                // Inheriting the previous search's options would be worse here than in the prompt —
                // there's no visible chip row to show what got carried over.
                options: MatchOptions::default(),
            },
            |r| {
                Event::SearchFromSel(
                    r.map(|r| r.query.map(|q| (q, SearchSetResult { query: None, ..r }))),
                )
            },
        )
    }

    /// `Esc` in Normal — drop the active search (clear highlights).
    pub fn drop_search(&mut self) -> Effects {
        if !(self.view.search.active || self.view.search.summary.is_some()) {
            return Effects::none();
        }
        self.view.search.active = false;
        self.view.search.summary = None;

        self.request::<SearchClear>(
            SearchClearParams {
                buffer_id: self.view.buffer.buffer_id,
            },
            move |__r| {
                let _ = __r;
                Event::Noop
            },
        )
    }

    /// `]`/`[` (full) and `Alt-]`/`Alt-[` (`CurrentFile` scope): step through the jumplist —
    /// resolve cursor-relative (stopping, not wrapping, at the ends), open transient at the entry,
    /// record nav, all one server-side composite. The direction and scope ride into the event so a
    /// boundary result can toast the right message.
    pub fn jumplist_step(
        &mut self,
        direction: Direction,
        count: u32,
        scope: JumplistStepScope,
    ) -> Effects {
        self.request_str::<JumplistStep>(
            JumplistStepParams {
                buffer_id: self.view.buffer.buffer_id,
                direction,
                count,
                scope,
                open: true,
            },
            move |r| Event::JumplistStepped(r, direction, scope),
        )
    }

    /// `Space Alt-j`: discard the context's captured list. Sends the current buffer so the
    /// response can carry a cursor with the `k/N` stamp already gone.
    fn clear_jumplist(&mut self) -> Effects {
        self.request_str::<JumplistClear>(
            JumplistClearParams {
                buffer_id: Some(self.view.buffer.buffer_id),
            },
            Event::JumplistCleared,
        )
    }

    /// Picker `Ctrl-j`: snapshot the picker's filtered results into the jumplist and jump to the
    /// highlighted row — capture + select in one composite. The picker closes like an accept;
    /// `]`/`[` then step the captured set. No-op while an async resolve is still filling the list
    /// (the snapshot would be partial).
    fn jumplist_capture(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if !p.kind.captures_to_jumplist() || p.ticking {
            return Effects::none();
        }
        let Some(item) = p.selected_item().cloned() else {
            return Effects::none();
        };
        let kind = p.kind;
        let observed = self.observe_picker_cmd(PickerCmd::CaptureJumplist);
        // The source picker stays open until the capture lands: on Ok(Some) the handler swaps
        // it for the Jumplist picker (same row highlighted) and toasts the count; on Ok(None)
        // nothing was captured and the source picker survives untouched.
        observed.and(
            self.request::<JumplistCapture>(JumplistCaptureParams { kind, item }, move |__r| {
                Event::JumplistCaptured(__r.map_err(|e| e.message), kind)
            }),
        )
    }

    // ---- Explorer/Files create + delete --------------------------------------------------

    /// Stage a delete confirm for the highlighted picker entry: trash a Files/Explorer file or
    /// directory (`path/delete`), or forget a workspace (`workspace/delete`) from the switcher. The
    /// absolute path comes from the picker's listed directory (Explorer) or the entry's workspace
    /// root (Files). The picker stays open under the confirm; the refreshed listing arrives via a
    /// `view/closed` / `picker/update` push.
    pub fn picker_stage_delete(&mut self) -> Effects {
        // A highlighted workspace: the server refuses to delete the active one (the rug-pull guard),
        // so don't even stage a doomed confirm — say why and bail.
        if let Some(p) = &self.picker {
            if p.kind == PickerKind::Workspaces {
                let Some(PickerItem::Workspace { name, .. }) = p.selected_item() else {
                    return Effects::none();
                };
                if name == &self.workspace {
                    return Effects::error_detail(
                        "Can't delete the active workspace",
                        "Switch away first",
                    );
                }
                let name = name.clone();
                self.prompt = Some(Prompt::Confirm {
                    kind: ConfirmKind::DeleteWorkspace { name: name.clone() },
                    action: ConfirmAction::DeleteWorkspace { name },
                });
                return Effects::none();
            }
        }

        let staged = {
            let Some(p) = &self.picker else {
                return Effects::none();
            };
            let Some(item) = p.selected_item() else {
                return Effects::none();
            };
            match item {
                PickerItem::DirEntry { name, is_dir, .. } => p.explorer_listing_dir().map(|dir| {
                    let noun = if *is_dir { "directory" } else { "file" };
                    (
                        format!("{}/{name}", dir.trim_end_matches('/')),
                        noun,
                        name.clone(),
                    )
                }),
                PickerItem::File {
                    path_index,
                    relative_path,
                    ..
                } => self.workspace_paths.get(*path_index as usize).map(|root| {
                    (
                        format!("{}/{relative_path}", root.trim_end_matches('/')),
                        "file",
                        relative_path.clone(),
                    )
                }),
                _ => None,
            }
        };
        let Some((path, noun, name)) = staged else {
            return Effects::none();
        };
        self.prompt = Some(Prompt::Confirm {
            kind: ConfirmKind::Delete { noun, name },
            action: ConfirmAction::DeletePath { path, noun },
        });
        Effects::none()
    }

    /// `Ctrl-d` in the view picker: close the highlighted view without opening it. A view whose
    /// text is unsaved goes through a discard confirm first (mirroring the editor's own close);
    /// clean and externally-changed ones (no in-buffer edits to lose) close straight away. The
    /// picker stays open and re-lists from the server's `picker/update` push.
    pub fn picker_close_view(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.kind != PickerKind::Views {
            return Effects::none();
        }
        let Some(PickerItem::View {
            buffer_id,
            view_id,
            status,
            display,
            ..
        }) = p.selected_item()
        else {
            return Effects::none();
        };
        let (buffer_id, view_id) = (*buffer_id, *view_id);
        if matches!(status, BufferDirtyState::Unsaved) {
            self.prompt = Some(Prompt::Confirm {
                kind: ConfirmKind::DiscardOnClose {
                    label: display.clone(),
                },
                action: ConfirmAction::ClosePickerView { buffer_id, view_id },
            });
            return Effects::none();
        }
        self.close_picker_view(buffer_id, view_id)
    }

    /// Fire `view/close` for a buffer chosen in the picker. `open_next` is set only when the
    /// closed buffer is the editor's active one — then the server attaches the viewport to the next
    /// MRU buffer (or a fresh scratch) and we adopt it; closing a background buffer leaves the editor
    /// untouched. Either way the picker stays open and re-lists from the server's refresh push (the
    /// switch doesn't tear it down — see [`Self::adopt_switch`]). Closing the
    /// [tether](Session::tether) — active or backgrounded — exits the client instead, like every
    /// other close path.
    fn close_picker_view(&mut self, buffer_id: BufferId, view_id: ViewId) -> Effects {
        // The row names its view — the one closing addresses — and its buffer, which is what the
        // tether is.
        if self.tether == Some(buffer_id) {
            return self.request_str::<ViewClose>(
                ViewCloseParams {
                    view_id,
                    open_next: false,
                },
                |r| Event::TetherClosed(r.map(|_| ())),
            );
        }
        let closing_active = view_id == self.view.view_id;
        self.request_str::<ViewClose>(
            ViewCloseParams {
                view_id,
                open_next: closing_active,
            },
            move |r| {
                if closing_active {
                    Event::Switched(r.and_then(|closed| {
                        closed
                            .opened
                            .ok_or_else(|| "view/close returned no successor".into())
                    }))
                } else {
                    // Background buffer: nothing to adopt — the picker refresh rides a separate push.
                    let _ = r;
                    Event::Noop
                }
            },
        )
    }

    /// Create whatever the Explorer query names in the listed directory — a directory when it ends
    /// with `/`, otherwise a file (which opens). Reached by selecting the synthetic "+ Create …"
    /// row (see [`PickerState::pending_create`]). Multi-segment names create the intermediate
    /// directories server-side. No-op outside the Explorer.
    pub fn explorer_create_from_query(&mut self) -> Effects {
        let (dir, query) = {
            let Some(p) = &self.picker else {
                return Effects::none();
            };
            if p.kind != PickerKind::Explorer {
                return Effects::none();
            }
            let Some(dir) = p.directory.clone() else {
                return Effects::none();
            };
            (dir, p.query.clone())
        };
        let q = query.trim();
        let (base, is_dir) = match q.strip_suffix('/') {
            Some(stripped) => (stripped, true),
            None => (q, false),
        };
        if base.is_empty() {
            return Effects::error("Type a name to create");
        }
        if base
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Effects::error("Invalid name");
        }
        let abs = format!("{}/{base}", dir.trim_end_matches('/'));
        if is_dir {
            return self.request_str::<DirectoryCreate>(
                DirectoryCreateParams { path: abs },
                Event::DirCreated,
            );
        }
        // File: address it under a workspace root, then open with create-on-save. Creating a
        // file is a terminal pick — you land in the new buffer — so drop the explorer first
        // (`Event::Switched`'s adopt deliberately leaves pickers open, which is right for the
        // view picker's close-and-relist but would strand the explorer over the new file).
        // Creating a *directory* instead steps into it and keeps exploring, and the
        // outside-roots refusal above keeps the explorer up so the name can be fixed.
        let Some((path_index, relative_path)) = strip_longest_root(&abs, &self.workspace_paths)
        else {
            return Effects::error_detail(
                "Outside the workspace",
                "That path isn't under any of this workspace's roots",
            );
        };
        let from = self.view.buffer.buffer_id;
        let hide = self.close_picker();
        hide.and(self.request_str::<ViewOpen>(
            ViewOpenParams {
                path_index: Some(path_index),
                relative_path: Some(relative_path),
                create_if_missing: true,
                record_nav_from: Some(from),
                ..Default::default()
            },
            Event::Switched,
        ))
    }

    /// The Workspaces picker's synthetic "+ Create workspace …" row: create a fresh workspace named by
    /// the (trimmed) query, then activate it. Mirrors [`explorer_create_from_query`].
    pub fn workspace_create_from_query(&mut self) -> Effects {
        let name = {
            let Some(p) = &self.picker else {
                return Effects::none();
            };
            if p.kind != PickerKind::Workspaces {
                return Effects::none();
            }
            p.query.trim().to_string()
        };
        if name.is_empty() {
            return Effects::error("Type a name to create");
        }
        if name.contains('/') || name.contains('\\') {
            return Effects::error_detail(
                "Invalid workspace name",
                "It can't contain path separators",
            );
        }
        // Hint observation before the picker closes (the chooser's create hint lives in this
        // context — a successful create is its follow).
        let observed = self.observe_picker_cmd(PickerCmd::CreateWorkspace);
        // Drop the picker first — the create both activates the workspace and (when it has no roots)
        // opens the settings overlay, so the picker shouldn't linger underneath.
        let hide = self.close_picker();
        observed.and(hide).and(self.request_str::<WorkspaceCreate>(
            WorkspaceCreateParams { name },
            Event::WorkspaceCreated,
        ))
    }

    /// Adopt a `WorkspaceInfo` returned by an add/remove-root RPC: update the session's roots and,
    /// when the settings overlay is open and for the same workspace, its roots list too.
    fn sync_workspace_info(&mut self, info: WorkspaceInfo) {
        if self.workspace == info.name {
            self.workspace_paths = info.paths.clone();
            self.workspace_projects = info.projects.clone();
        }
        if let Some(s) = self.workspace_settings.as_mut() {
            if s.workspace_name == info.name {
                s.roots = info.paths;
                s.projects = info.projects;
            }
        }
    }

    /// Open the workspace-settings overlay (`Space .`), seeded from the active workspace's name and
    /// roots. Focus lands on the always-present add-root input row at the bottom, since most opens
    /// (especially the post-create flow) are to add a root; the name field is above the roots and
    /// reached with Alt-k. Migrated from the TUI's `open_workspace_settings`.
    ///
    /// Emits the add-root row's first `directory/list`, for the `~/` it opens seeded with — so the
    /// completions are on screen before the first keystroke rather than after you have guessed a
    /// prefix. (Its return type changed from `()` for exactly this; the overlay is otherwise still
    /// free to open.)
    pub fn open_workspace_settings(&mut self) -> Effects {
        let roots = self.workspace_paths.clone();
        let projects = self.workspace_projects.clone();
        let workspace_name = self.workspace.clone();
        self.workspace_settings = Some(WorkspaceSettings {
            workspace_name: workspace_name.clone(),
            name: TextField::new(workspace_name),
            roots,
            projects,
            selected: 0, // the workspace-name field
            // Seeded `~/`, and directories-only: a root is a directory, so a file suggestion here
            // could only ever lead to the server's rejection.
            add: Box::new(PathEditor::absolute(HOME_PREFIX.to_string(), false)),
            // Multi-root workspaces open on the root segment (there's a choice to make); a
            // single-root one skips straight to the path, where its only root is implied.
            add_project_language: crate::chips::Input::default(),
            add_project_language_selected: 0,
            on_add_project_language: false,
            language_inferred: false,
            inference_key: None,
            add_project: Box::new(PathEditor::new(
                String::new(),
                if self.workspace_paths.len() > 1 {
                    ChipEditorField::Root
                } else {
                    ChipEditorField::Path
                },
                0,
                // Directories only: a project *is* its directory, so a file suggestion here could
                // only ever lead to the server's rejection.
                false,
            )),
            error: None,
        });
        let workspace_paths = self.workspace_paths.clone();
        if let Some(s) = self.workspace_settings.as_mut() {
            s.add.sync_dir_listing(&workspace_paths);
        }
        self.refresh_add_root_listing()
    }

    /// Keys while the workspace-settings overlay is open. Migrated from the TUI's
    /// `handle_workspace_settings_key`, made sans-IO: rename / add-root / remove-root each emit an
    /// `Effect::Request`, whose result event ([`Event::WorkspaceRenamed`] / `WorkspaceRootAdded` /
    /// `WorkspaceRootRemoved`) updates the overlay. The TUI's "commit-rename-then-advance-only-on-
    /// success" gate is simplified: Enter / blur emits the rename request and navigation is free;
    /// the result event reconciles the name (or sets the error).
    ///
    /// Selection model: index 0 is the name field, `1..=roots.len` the root rows, and
    /// `roots.len + 1` the add-root input row. Alt-j/k move between fields; Left/Right move the
    /// caret inside a text field. Delete / Ctrl-d on a root row opens the shared confirm prompt
    /// (`request_remove_root`); Enter on the input row commits the add.
    pub fn on_workspace_settings_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Effects {
        // Ctrl-d is accepted alongside Delete to remove the selected root or project.
        let is_delete_chord =
            code == KeyCode::Delete || (code == KeyCode::Char('d') && mods.ctrl && !mods.alt);

        let Some(row) = self.workspace_settings.as_ref().map(|s| s.row()) else {
            return Effects::none();
        };
        let on_name = row == SettingsRow::Name;
        let no_chord = !mods.ctrl && !mods.alt;

        if code == KeyCode::Esc {
            // Closing blurs the name field — commit any pending rename, then close. Unlike the TUI,
            // the close isn't gated on the rename succeeding: the request fires and the overlay
            // closes; a rejected rename surfaces as a toast rather than holding the overlay open.
            let rename = if on_name {
                self.commit_rename_if_changed()
            } else {
                Effects::none()
            };
            self.workspace_settings = None;
            return rename;
        }

        // The add-project row is a full path editor. Give it the key first: it owns the chords that
        // act *within* a field (Alt-j/k cycle candidates, Alt-l accepts, Alt-h/Backspace step back),
        // and hands `Tab`/`Shift-Tab` back once it runs out of segments so traversal continues
        // through the dialog. Anything it ignores falls through to the dialog keys below.
        if row == SettingsRow::AddProject {
            // The language segment sits after the editor's own two, so it takes the keys first when
            // it has focus.
            if self
                .workspace_settings
                .as_ref()
                .is_some_and(|s| s.on_add_project_language)
            {
                if let Some(fx) = self.on_add_project_language_key(code, mods) {
                    return fx;
                }
            }
            let workspace_paths = self.workspace_paths.clone();
            let outcome = self.workspace_settings.as_mut().map(|s| {
                path_editor_key(
                    &mut s.add_project,
                    &workspace_paths,
                    code,
                    mods,
                    text.clone(),
                )
            });
            match outcome {
                Some(PathEditorKey::Commit) => return self.commit_add_project(),
                // Esc closes the whole dialog, as it does from any row — the editor is a field
                // here, not a prompt of its own.
                Some(PathEditorKey::Cancel) => {
                    self.workspace_settings = None;
                    return Effects::none();
                }
                // Editor chords can rewrite the path (Alt-l accept, Alt-Backspace pop) or re-aim
                // the root (Alt-j/k in the root segment) — re-sync the language suggestion either
                // way; it dedupes on the (root, path) pair, so an unmoved pair costs nothing.
                Some(PathEditorKey::Handled { refresh: true }) => {
                    let fx = self.refresh_add_project_listing();
                    return fx.and(self.sync_add_project_inference());
                }
                Some(PathEditorKey::Handled { refresh: false }) => {
                    return self.sync_add_project_inference()
                }
                // Tab off the editor's last segment enters the language field rather than leaving
                // the row; only Tab off *that* moves on. Backward still steps to the row above.
                Some(PathEditorKey::NextField) => {
                    if let Some(s) = self.workspace_settings.as_mut() {
                        s.on_add_project_language = true;
                    }
                    return Effects::none();
                }
                Some(PathEditorKey::PrevField) => return self.settings_step_field(false),
                Some(PathEditorKey::Ignored) | None => {}
            }
        }

        // The add-root row is a path editor too, and gets the key on the same terms — but a
        // single-segment one (its path is absolute, so there is no root to choose), which is why
        // `NextField`/`PrevField` simply traverse the dialog instead of stepping into a language
        // field the way add-project's do.
        if row == SettingsRow::AddRoot {
            let workspace_paths = self.workspace_paths.clone();
            let outcome = self
                .workspace_settings
                .as_mut()
                .map(|s| path_editor_key(&mut s.add, &workspace_paths, code, mods, text.clone()));
            match outcome {
                Some(PathEditorKey::Commit) => return self.commit_add_root(),
                Some(PathEditorKey::Cancel) => {
                    self.workspace_settings = None;
                    return Effects::none();
                }
                Some(PathEditorKey::Handled { refresh: true }) => {
                    return self.refresh_add_root_listing()
                }
                Some(PathEditorKey::Handled { refresh: false }) => return Effects::none(),
                Some(PathEditorKey::NextField) => return self.settings_step_field(true),
                Some(PathEditorKey::PrevField) => return self.settings_step_field(false),
                Some(PathEditorKey::Ignored) | None => {}
            }
        }

        // Tab / Shift-Tab traverse the dialog's fields — the form convention, and the reason the
        // editor above no longer claims Tab for completion.
        if code == KeyCode::Tab || code == KeyCode::BackTab {
            return self.settings_step_field(code == KeyCode::Tab);
        }

        // Up / Down traverse too, as a non-chord alternative to Tab. Deliberately *not* Alt-j/k:
        // those act inside the focused field (cycling the path editor's candidates), and a key that
        // sometimes traverses and sometimes doesn't — depending on which field you happen to be on —
        // is exactly the ambiguity Tab was introduced to remove. No field here uses the arrows
        // (neither name nor path input has history recall), so they're free.
        if no_chord && matches!(code, KeyCode::Up | KeyCode::Down) {
            let rename = if on_name && code == KeyCode::Down {
                self.commit_rename_if_changed()
            } else {
                Effects::none()
            };
            if let Some(s) = self.workspace_settings.as_mut() {
                s.selected = if code == KeyCode::Down {
                    (s.selected + 1).min(s.row_count() - 1)
                } else {
                    s.selected.saturating_sub(1)
                };
            }
            return rename;
        }

        if is_delete_chord {
            match row {
                SettingsRow::Root(i) => return self.request_remove_root(i),
                SettingsRow::Project(i) => return self.request_remove_project(i),
                _ => {}
            }
        }

        // Both input rows commit through their own editor above; only the name field reaches here.
        if code == KeyCode::Enter {
            match row {
                SettingsRow::Name => return self.commit_rename_if_changed(),
                _ => return Effects::none(),
            }
        }

        // Alt-Backspace deletes the last unit of the focused field, at the field's own grain. Only
        // the name field's grain (a word) lives here now — both path rows pop a `/` segment through
        // the editor above, which also refreshes their listing to follow the pop.
        if code == KeyCode::Backspace && mods.alt && !mods.ctrl {
            if let Some(s) = self.workspace_settings.as_mut() {
                if row == SettingsRow::Name {
                    let shortened = chips::pop_word(&s.name.text);
                    s.name.set(shortened);
                }
            }
            return Effects::none();
        }

        // Text editing for the focused field (name / add-root / add-project) is owned by each
        // shell's input, which syncs the value via `workspace_settings_set_name` / `_set_add` /
        // `_set_add_project`. The core handles only the command keys above; any other key here is a
        // no-op.
        let _ = text;
        Effects::none()
    }

    /// Keys while the add-project row's language segment has focus. `None` means "not mine" — the
    /// key falls through to the path editor and then the dialog.
    ///
    /// A typeahead over the supported languages, mirroring the root segment's: `Alt-j/k` cycle the
    /// matches, `Alt-l` adopts the highlighted one, `Shift-Tab`/`Alt-h` step back into the path, and
    /// `Tab` leaves the row. Text entry is shell-owned, synced via
    /// [`Self::workspace_settings_set_add_project_language`].
    fn on_add_project_language_key(&mut self, code: KeyCode, mods: Mods) -> Option<Effects> {
        let alt = mods.alt && !mods.ctrl;
        let no_chord = !mods.ctrl && !mods.alt;
        match code {
            // Leaving forwards settles on the highlighted candidate, so a partly typed `pyth`
            // commits as `python` rather than as text the server would reject.
            KeyCode::Tab if no_chord => {
                self.adopt_highlighted_language();
                if let Some(s) = self.workspace_settings.as_mut() {
                    s.on_add_project_language = false;
                }
                Some(self.settings_step_field(true))
            }
            KeyCode::BackTab | KeyCode::Char('h') if code == KeyCode::BackTab || alt => {
                if let Some(s) = self.workspace_settings.as_mut() {
                    s.on_add_project_language = false;
                }
                Some(Effects::none())
            }
            KeyCode::Char('l') if alt => {
                self.adopt_highlighted_language();
                Some(Effects::none())
            }
            KeyCode::Char(c @ ('j' | 'k')) if alt => {
                if let Some(s) = self.workspace_settings.as_mut() {
                    let n = s.language_candidates().len();
                    if n > 0 {
                        let sel = s.add_project_language_selected.min(n - 1);
                        s.add_project_language_selected = if c == 'j' {
                            (sel + 1) % n
                        } else {
                            (sel + n - 1) % n
                        };
                    }
                }
                Some(Effects::none())
            }
            // Enter commits the whole row from here too.
            KeyCode::Enter if no_chord => {
                self.adopt_highlighted_language();
                Some(self.commit_add_project())
            }
            _ => None,
        }
    }

    /// Replace the typed language filter with the candidate it resolves to. A no-op when nothing
    /// matches, so an invalid entry stays visible (and red) rather than being silently rewritten.
    fn adopt_highlighted_language(&mut self) {
        let Some(s) = self.workspace_settings.as_mut() else {
            return;
        };
        // An empty field means "infer" — leaving it must not invent the first candidate.
        if s.add_project_language.text.is_empty() {
            return;
        }
        if let Some(full) = s.highlighted_language() {
            s.add_project_language = crate::chips::Input::new(full.to_string());
            s.add_project_language_selected = 0;
        }
    }

    /// Replace the add-project row's language filter wholesale (native `<input>` parity). An edit
    /// arriving here is the user typing (a core-driven autofill syncs the *same* text back through
    /// the shells, which the no-change guard swallows) — so the field stops being "inferred" and
    /// later inference results leave it alone.
    pub fn workspace_settings_set_add_project_language(&mut self, text: String) -> Effects {
        if let Some(s) = self.workspace_settings.as_mut() {
            if s.add_project_language.text != text {
                s.add_project_language.set(text);
                s.add_project_language_selected = 0;
                s.language_inferred = false;
                s.error = None;
            }
        }
        Effects::none()
    }

    /// Move the workspace-settings focus one field forward or back, **wrapping** at either end — a
    /// form that traps you at the last field is worse than one you can cycle, and Tab is expected to
    /// cycle.
    ///
    /// Landing on the add-project row enters its composite editor at the end you arrived from: the
    /// root segment going forwards, the path segment coming back. That's how Tab behaves into any
    /// multi-part widget, and it makes reverse traversal actually retrace the forward path.
    fn settings_step_field(&mut self, forward: bool) -> Effects {
        let Some(s) = self.workspace_settings.as_ref() else {
            return Effects::none();
        };
        // Leaving the name field commits any pending rename, exactly as blurring it does.
        let rename = if s.row() == SettingsRow::Name && forward {
            self.commit_rename_if_changed()
        } else {
            Effects::none()
        };
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_mut() else {
            return rename;
        };
        let multi_root = s.add_project.multi_root(&workspace_paths);
        let count = s.row_count();
        s.selected = if forward {
            (s.selected + 1) % count
        } else {
            (s.selected + count - 1) % count
        };
        if s.row() == SettingsRow::AddProject && multi_root {
            s.add_project.field = if forward {
                ChipEditorField::Root
            } else {
                ChipEditorField::Path
            };
        }
        rename
    }

    /// Commit a pending workspace rename if the name field differs from the committed name. Emits a
    /// `workspace/rename` request; [`Event::WorkspaceRenamed`] reconciles the result. A no-op edit
    /// (empty or unchanged) just normalizes the field back to the committed name. Migrated from the
    /// TUI's `commit_rename_if_changed`, minus its success-gating return value (navigation is free
    /// now — the result event updates the name when it lands).
    fn commit_rename_if_changed(&mut self) -> Effects {
        let Some((old_name, new_name)) = self
            .workspace_settings
            .as_ref()
            .map(|s| (s.workspace_name.clone(), s.name.text.trim().to_string()))
        else {
            return Effects::none();
        };
        if new_name.is_empty() || new_name == old_name {
            if let Some(s) = self.workspace_settings.as_mut() {
                s.name.set(old_name);
            }
            return Effects::none();
        }
        self.request_str::<WorkspaceRename>(
            WorkspaceRenameParams {
                workspace: old_name,
                new_name,
            },
            Event::WorkspaceRenamed,
        )
    }

    /// Commit the add-root input row: emit a `workspace/add_root` request for the trimmed path.
    /// [`Event::WorkspaceRootAdded`] reconciles the result. Migrated from the TUI's `commit_add_root`.
    fn commit_add_root(&mut self) -> Effects {
        // `absolute_target` rather than `save_target`: this editor has no root to be relative to,
        // and the pair is exclusive so reaching for the wrong one yields `None` instead of a
        // plausible-looking `(0, "/home/me/code")`. The `~` travels verbatim — `workspace/add_root`
        // expands it server-side, where there is a `$HOME` to expand against.
        let Some((workspace, Some(path))) = self
            .workspace_settings
            .as_ref()
            .map(|s| (s.workspace_name.clone(), s.add.absolute_target()))
        else {
            return Effects::none();
        };
        if let Some(s) = self.workspace_settings.as_mut() {
            s.error = None;
        }
        self.request_str::<WorkspaceAddRoot>(
            WorkspaceAddRootParams { workspace, path },
            Event::WorkspaceRootAdded,
        )
    }

    /// Commit the add-project row: emit `workspace/add_project` for the editor's (root, path) pair
    /// and the chosen language.
    ///
    /// The language is optional — left blank the server infers it from the directory's build
    /// manifests, which covers the common case. Typed, it must be one of ours: an unmatched filter
    /// refuses the commit rather than sending text the server would reject, so the field can only
    /// ever produce a language that starts something.
    fn commit_add_project(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(s) = self.workspace_settings.as_ref() else {
            return Effects::none();
        };
        let workspace = s.workspace_name.clone();
        // `save_target` yields the literal typed path under the chosen root — no snapping to the
        // highlighted suggestion (that's what Tab is for), which matters here too: you may be
        // naming a directory the completion listing hasn't caught up with.
        let Some((path_index, relative_path)) = s.add_project.save_target(&workspace_paths) else {
            return Effects::none();
        };
        if relative_path.trim().is_empty() {
            return Effects::none();
        }
        if s.language_invalid() {
            let typed = s.add_project_language.text.clone();
            if let Some(s) = self.workspace_settings.as_mut() {
                s.error = Some(format!("{typed} is not a language Aether has a server for"));
            }
            return Effects::none();
        }
        let language = s.chosen_language();
        if let Some(s) = self.workspace_settings.as_mut() {
            s.error = None;
        }
        self.request_str::<WorkspaceAddProject>(
            WorkspaceAddProjectParams {
                workspace,
                path_index,
                relative_path,
                language,
            },
            Event::WorkspaceProjectAdded,
        )
    }

    /// Open the shared confirm prompt for removing project `index`. Mirrors
    /// [`Self::request_remove_root`]: the request is self-contained, so the overlay's selection
    /// moving (or the overlay closing) before the confirm resolves can't misfire it.
    pub fn request_remove_project(&mut self, index: usize) -> Effects {
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        let Some(project) = s.projects.get(index).cloned() else {
            return Effects::none();
        };
        let workspace = s.workspace_name.clone();
        s.error = None;
        self.prompt = Some(Prompt::Confirm {
            kind: ConfirmKind::RemoveProject {
                path: project.relative_path.clone(),
            },
            action: ConfirmAction::RemoveWorkspaceProject {
                workspace,
                path_index: project.path_index,
                relative_path: project.relative_path,
            },
        });
        Effects::none()
    }

    /// Open the shared confirm prompt for removing root `index` (the selected root row, or a
    /// clicked delete button). The actual `workspace/remove_root` request fires when the prompt is
    /// accepted ([`ConfirmAction::RemoveWorkspaceRoot`] → [`Self::run_confirm`]); the result lands as
    /// [`Event::WorkspaceRootRemoved`]. No-op if the overlay is closed or the index is out of range.
    pub fn request_remove_root(&mut self, index: usize) -> Effects {
        let Some(s) = self.workspace_settings.as_mut() else {
            return Effects::none();
        };
        let Some(path) = s.roots.get(index).cloned() else {
            return Effects::none();
        };
        let workspace = s.workspace_name.clone();
        s.error = None;
        self.prompt = Some(Prompt::Confirm {
            kind: ConfirmKind::RemoveRoot { path: path.clone() },
            action: ConfirmAction::RemoveWorkspaceRoot { workspace, path },
        });
        Effects::none()
    }

    /// Fetch the persisted application settings (`settings/get`) and seed the session from them once
    /// they arrive ([`Event::AppSettingsLoaded`]) — notably the soft-wrap default. Shells call this
    /// once their live session is established (at boot, and again after a reconnect rebuilds the
    /// session) and run the returned effect like any other.
    pub fn startup(&mut self) -> Effects {
        let fx = self.request_str::<SettingsGet>(SettingsGetParams {}, Event::AppSettingsLoaded);
        // The hint learning snapshot rides the same connect sequence; re-fetched after a
        // reconnect too, which reconciles the local mirror against the server's counters.
        let fx =
            fx.and(self.request_str::<HintsState>(HintsStateParams {}, Event::HintsStateLoaded));
        // So do the input-history lists. Empty at a boot chooser — no workspace is active yet — and
        // refetched by the switch that activates one.
        fx.and(self.fetch_history())
    }

    // ---- hints ------------------------------------------------------

    /// The hint context the session is in right now — which curriculum pool the corner draws
    /// from. `None` means hints have nowhere to display: the boot placeholder (with no picker),
    /// confirm/info prompts, the workspace-settings overlay, or an active sneak (whose
    /// keystrokes are query input, not bindings). Precedence mirrors [`Self::dispatch_key`]'s
    /// keyboard ownership — except the picker outranks the placeholder check: the boot chooser
    /// (all shells) is the Workspaces picker over a placeholder session, and it has its own hints.
    fn hint_context(&self) -> Option<HintCtx> {
        if let Some(prompt) = &self.prompt {
            return match prompt {
                // Both path prompts report the same context: they are the same `PathEditor`, with
                // the same Alt-l / Alt-j/k / Alt-Backspace vocabulary, so a hint written for one is
                // true of the other. A separate `OpenPath` variant would only split the curriculum
                // in two to say the same thing twice.
                Prompt::SaveAs(_) | Prompt::OpenPath(_) => Some(HintCtx::SaveAs),
                _ => None,
            };
        }
        if let Some(p) = &self.picker {
            return Some(HintCtx::Picker(p.kind));
        }
        if self.is_placeholder() {
            return None;
        }
        if self.workspace_settings.is_some() {
            return None;
        }
        if self.app_settings.is_some() {
            return Some(HintCtx::Settings);
        }
        if self.view.sneak.is_some() {
            return None;
        }
        match self.view.mode {
            Mode::Normal => Some(HintCtx::Normal),
            Mode::Insert => Some(HintCtx::Insert),
            Mode::Search => Some(HintCtx::Search),
            Mode::Read => Some(HintCtx::Read),
        }
    }

    /// The session facts that condition hint display eligibility beyond the context id — the engine
    /// is sans-IO, so it learns these only when stamped in.
    fn hint_facts(&self) -> HintFacts {
        HintFacts {
            workspaces_listed: self.picker.as_ref().and_then(|p| p.listed_workspaces()),
            mandatory_chooser: self.is_placeholder(),
            markdown_buffer: self.view.buffer.language.as_deref() == Some("markdown"),
            read_block_has_targets: self.read_block_has_targets(),
        }
    }

    /// Whether the reading view's focused block contains interactive elements — the fact
    /// gating the link-selection hints (`l/h`, Enter, Tab) to moments they can act.
    fn read_block_has_targets(&self) -> bool {
        let Some(read) = self.view.read.as_ref() else {
            return false;
        };
        let Some(idx) = read.block_focus(self.view.buffer.cursor.position) else {
            return false;
        };
        let span = read.elements[idx].span();
        !crate::markdown::interactive_within(&read.elements, span).is_empty()
    }

    /// The shared preamble of every hint-engine call: stamp the current facts and resolve the
    /// context.
    fn hint_env(&mut self) -> Option<HintCtx> {
        let facts = self.hint_facts();
        self.hints.set_facts(facts);
        self.hint_context()
    }

    /// The corner hint each shell renders top-right, if any. Pure read — safe to call per frame.
    pub fn hint_view(&self) -> Option<HintView> {
        self.hints.view(self.hint_context(), self.hints_enabled)
    }

    /// The shell's periodic hint tick (every couple of seconds, while the window has focus):
    /// stamps the wall clock into the engine, runs the display timer/rotation, and flushes any
    /// hint events to the wire. Cheap when hints are off or nothing is due.
    pub fn on_hint_tick(&mut self, now_ms: u64) -> Effects {
        let ctx = self.hint_env();
        let enabled = self.hints_enabled;
        let evs = self.hints.on_tick(ctx, now_ms, enabled);
        self.emit_hint_events(evs)
    }

    /// Re-sync the hint engine to the session's current context (called after key dispatch and
    /// event handling — both can open/close overlays). Emits the Shown for a freshly-filled slot.
    fn sync_hint_context(&mut self) -> Effects {
        let ctx = self.hint_env();
        let enabled = self.hints_enabled;
        let evs = self.hints.sync_context(ctx, enabled);
        self.emit_hint_events(evs)
    }

    /// An instrumented picker-vocabulary command fired (`on_picker_key` has no `Action` identity,
    /// so the curriculum-relevant arms report a [`PickerCmd`] explicitly).
    fn observe_picker_cmd(&mut self, cmd: PickerCmd) -> Effects {
        let ctx = self.hint_env();
        let enabled = self.hints_enabled;
        let evs = self.hints.observe_picker(cmd, ctx, enabled);
        self.emit_hint_events(evs)
    }

    /// Put hint events on the wire (`hints/record`). Fire-and-forget: the server's aggregate is
    /// reconciled at the next connect, so results are ignored.
    fn emit_hint_events(&mut self, evs: Vec<HintWireEvent>) -> Effects {
        let mut fx = Effects::none();
        for ev in evs {
            fx = fx.and(self.request_str::<HintsRecord>(
                HintsRecordParams {
                    hint_id: ev.hint_id.to_string(),
                    event: ev.event,
                },
                |_| Event::Noop,
            ));
        }
        fx
    }

    /// Open the application-settings overlay (`Space,`). Cheap — no RPC; the values it shows
    /// already live on the session. Focus lands on the first row.
    pub fn open_app_settings(&mut self) {
        self.app_settings = Some(AppSettingsOverlay { selected: 0 });
    }

    /// Keys while the app-settings overlay is open (it owns the keyboard, like the workspace-settings
    /// overlay). Esc closes; Alt-j/k or Up/Down move between rows; Enter/Space activates the focused
    /// row's toggle. The overlay has no text entry, so any other key is a no-op.
    pub fn on_app_settings_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        _text: Option<String>,
    ) -> Effects {
        let row_count = self.app_setting_rows().len();
        let Some(selected) = self.app_settings.as_ref().map(|s| s.selected) else {
            return Effects::none();
        };

        if code == KeyCode::Esc {
            self.app_settings = None;
            return Effects::none();
        }

        // Tab / Shift-Tab and the arrows traverse, matching the workspace-settings dialog. Alt-j/k
        // is deliberately absent for the same reason it is there: it belongs to the focused field,
        // and a key that traverses only when the field doesn't want it is unpredictable. Every row
        // here is a toggle with nothing to cycle, so Alt-j/k simply does nothing.
        //
        // Tab wraps at either end (as in the workspace dialog); the arrows clamp, which is what an
        // arrow key does in every list in the app.
        if matches!(code, KeyCode::Tab | KeyCode::BackTab) && row_count > 0 {
            if let Some(s) = self.app_settings.as_mut() {
                s.selected = if code == KeyCode::Tab {
                    (s.selected + 1) % row_count
                } else {
                    (s.selected + row_count - 1) % row_count
                };
            }
            return Effects::none();
        }
        if code == KeyCode::Up {
            if let Some(s) = self.app_settings.as_mut() {
                s.selected = s.selected.saturating_sub(1);
            }
            return Effects::none();
        }
        if code == KeyCode::Down {
            if let Some(s) = self.app_settings.as_mut() {
                s.selected = (s.selected + 1).min(row_count.saturating_sub(1));
            }
            return Effects::none();
        }

        // Left/Right step a multi-value row (either font size, the reading width) without
        // wrapping — a natural stepper. They're inert on a toggle row (Enter/Space flips those).
        let left = code == KeyCode::Left || (mods.alt && code == KeyCode::Char('h'));
        let right = code == KeyCode::Right || (mods.alt && code == KeyCode::Char('l'));
        if left || right {
            return match self.app_setting_rows().get(selected).map(|r| r.id) {
                Some(AppSettingId::EditorFontSize) => {
                    self.set_editor_font_size(step_font_size(self.editor_font_size, right, false))
                }
                Some(AppSettingId::UiFontSize) => {
                    self.set_ui_font_size(step_font_size(self.ui_font_size, right, false))
                }
                Some(AppSettingId::MarkdownWidth) => {
                    self.set_markdown_width(step_markdown_width(self.markdown_width, right, false))
                }
                _ => Effects::none(),
            };
        }

        if code == KeyCode::Enter || code == KeyCode::Char(' ') {
            return self.toggle_app_setting(selected);
        }
        Effects::none()
    }

    /// Toggle the setting at flat row `index` from a shell-side click on its checkbox (native/web).
    /// Moves the focus there too, so a click and a subsequent keypress agree on the row. The
    /// keyboard path (Enter/Space) calls [`Self::toggle_app_setting`] directly with the current
    /// selection. No-op if the overlay is closed or the index is out of range.
    pub fn app_settings_toggle(&mut self, index: usize) -> Effects {
        if self.app_settings.is_none() || index >= self.app_setting_rows().len() {
            return Effects::none();
        }
        if let Some(s) = self.app_settings.as_mut() {
            s.selected = index;
        }
        self.toggle_app_setting(index)
    }

    /// Apply app settings to the live session, reflowing only what changed. Shared by the boot fetch
    /// ([`Event::AppSettingsLoaded`]) and the live cross-client push (`settings/changed`). `Session.wrap`
    /// has exactly two values, so a value that differs from the current is its opposite — flipping via
    /// the existing wrap reflow path (anchor + `ToggleWrap`) lands on it; a matching value is a no-op.
    /// The shell ignores `ToggleWrap` until it has a viewport, so this is safe even at boot.
    fn apply_app_settings(&mut self, settings: AppSettings) -> Effects {
        // Ligatures is a pure client-side render choice: the shells read `self.ligatures` each frame
        // (native = text shaping, web = font feature), so adopting the value is enough — the
        // re-render after this event applies it. No reflow / round-trip like wrap needs.
        self.ligatures = settings.ligatures;
        // Both font sizes are likewise client-side: the GUI/web shells read them each render — the
        // buffer size re-measures the cell + reflows, the UI size rescales the chrome (the terminal
        // ignores both). Adopting the values is enough; the re-render after this event applies them.
        self.editor_font_size = settings.editor_font_size;
        self.ui_font_size = settings.ui_font_size;
        // Hints likewise: the engine reads the flag before observing/sampling, and the shells stop
        // rendering the corner hint when it's off.
        self.hints_enabled = settings.hints;
        // The markdown-read setting is the server's to apply, when a file is first presented;
        // the mirror only feeds the settings overlay.
        self.markdown_read_default = settings.markdown_read;
        // The reading width is shell-render-only: each shell resolves it through `read_layout`'s
        // measure table every frame, so adopting the value is enough. The TUI's layout cache keys
        // off the resolved column count, so it re-lays out on its own.
        self.markdown_width = settings.markdown_width;
        // Theme is shell-render-only too: the shells resolve `self.theme` to a role table each
        // frame (web also stamps `data-theme`), so adopting the mode is enough.
        self.theme = settings.theme;
        // Nothing to apply client-side: the server reads this off disk when its timer fires, so
        // adopting the value only keeps the overlay row showing the truth.
        self.git_auto_fetch = settings.git_auto_fetch;
        self.worktree_store = settings.worktree_store.clone();
        if settings.wrap != self.wrap {
            let mut fx = Effects::one(Effect::SaveContentAnchor);
            fx.push(Effect::ShellAction(ShellAction::ToggleWrap));
            fx
        } else {
            Effects::none()
        }
    }

    /// Flip the setting at flat row `index`: persist the new value and apply it live, keyed by the
    /// row's stable [`AppSettingId`] (not the raw index). Out-of-range indices no-op.
    fn toggle_app_setting(&mut self, index: usize) -> Effects {
        let Some(row) = self.app_setting_rows().into_iter().nth(index) else {
            return Effects::none();
        };
        match row.id {
            // Soft wrap. The persisted value is the *post-flip* wrap: the shell flips `Session.wrap`
            // when it runs the `ToggleWrap` shell action below, so compute the new value here to keep
            // disk and session agreeing. The reflow reuses the existing wrap path — the shell flips
            // `Session.wrap` and re-renders on [`Action::ToggleWrap`], with a content anchor captured
            // first so the viewport stays on the same content across the reflow.
            AppSettingId::SoftWrap => {
                let new_wrap = match self.wrap {
                    WrapMode::Soft => WrapMode::None,
                    WrapMode::None => WrapMode::Soft,
                };
                let mut fx = self.request_str::<SettingsSet>(
                    AppSettings {
                        wrap: new_wrap,
                        ..self.current_app_settings()
                    },
                    Event::AppSettingsSaved,
                );
                fx.push(Effect::SaveContentAnchor);
                fx.push(Effect::ShellAction(ShellAction::ToggleWrap));
                fx
            }
            // Ligatures is shell-render-only: flip the value + persist; the re-render after this
            // event applies it (native swaps text shaping, web toggles the font feature). No reflow.
            AppSettingId::Ligatures => {
                self.ligatures = !self.ligatures;
                self.persist_app_settings()
            }
            // Font sizes: activating either row cycles to the next preset (wrapping). Like ligatures
            // they're shell-render-only — set the value + persist, and the GUI/web re-render applies
            // it. `step` lets the Left/Right keys pass a non-wrapping direction.
            AppSettingId::EditorFontSize => {
                self.set_editor_font_size(step_font_size(self.editor_font_size, true, true))
            }
            AppSettingId::UiFontSize => {
                self.set_ui_font_size(step_font_size(self.ui_font_size, true, true))
            }
            // Hints: same flip (and toast) as `Space Alt-h`.
            AppSettingId::Hints => self.toggle_hints(),
            // Markdown reading view default: applies to files never presented (the server
            // remembers how each file was last shown; `Space u` re-presents without touching the
            // setting). Flip + persist.
            AppSettingId::MarkdownRead => {
                self.markdown_read_default = !self.markdown_read_default;
                self.persist_app_settings()
            }
            // Reading width: activating the row cycles to the next option (wrapping), like the
            // font-size rows. Shell-render-only — every shell re-resolves the measure on the next
            // frame, so an open reading view rewidens under the overlay as you step through.
            AppSettingId::MarkdownWidth => {
                self.set_markdown_width(step_markdown_width(self.markdown_width, true, true))
            }
            // Background fetch: the toast is the affordance, because turning this *on* has no
            // visible effect until the next tick — and turning it on is the moment to be explicit
            // that the editor will now talk to the network on its own.
            AppSettingId::GitAutoFetch => {
                self.git_auto_fetch = !self.git_auto_fetch;
                let msg = if self.git_auto_fetch {
                    "Background fetch enabled"
                } else {
                    "Background fetch disabled"
                };
                self.persist_app_settings().and(Effects::toast_grouped(
                    msg,
                    ToastKind::Info,
                    "git-auto-fetch",
                ))
            }
            // Theme: shell-render-only like ligatures — flip the mode + persist; every shell
            // re-resolves its role table on the next render.
            AppSettingId::Theme => {
                self.theme = match self.theme {
                    ThemeMode::Dark => ThemeMode::Light,
                    ThemeMode::Light => ThemeMode::Dark,
                };
                self.persist_app_settings()
            }
        }
    }

    /// Flip hints on/off, persist, and announce it. Shared by `Space Alt-h` and the settings
    /// row. The toast is the affordance: turning hints off just empties a corner, which reads as
    /// nothing happening — and the off-message names the chord, so off is discoverably
    /// reversible.
    fn toggle_hints(&mut self) -> Effects {
        self.hints_enabled = !self.hints_enabled;
        let msg = if self.hints_enabled {
            "Hints enabled"
        } else {
            "Hints disabled — the same key re-enables them"
        };
        self.persist_app_settings()
            .and(Effects::toast_grouped(msg, ToastKind::Info, "hints"))
    }

    /// The app settings exactly as this session currently holds them — the base every
    /// `settings/set` builds on, so a toggle can override one field without restating the rest.
    fn current_app_settings(&self) -> AppSettings {
        AppSettings {
            wrap: self.wrap,
            ligatures: self.ligatures,
            editor_font_size: self.editor_font_size,
            ui_font_size: self.ui_font_size,
            hints: self.hints_enabled,
            markdown_read: self.markdown_read_default,
            markdown_width: self.markdown_width,
            theme: self.theme,
            git_auto_fetch: self.git_auto_fetch,
            worktree_store: self.worktree_store.clone(),
        }
    }

    /// Persist the session's current app settings (`settings/set`), after a field was mutated in
    /// place. The result only reports persistence trouble — the optimistic local change already
    /// applied.
    fn persist_app_settings(&mut self) -> Effects {
        self.request_str::<SettingsSet>(self.current_app_settings(), Event::AppSettingsSaved)
    }

    /// Persist a new buffer text size + apply it (the GUI/web re-render reads
    /// `self.editor_font_size`, re-measures its cell and reflows). No-op when unchanged. Shared by
    /// the row's activate-cycle and the Left/Right stepper.
    fn set_editor_font_size(&mut self, font_size: u32) -> Effects {
        if font_size == self.editor_font_size {
            return Effects::none();
        }
        self.editor_font_size = font_size;
        self.persist_app_settings()
    }

    /// The same for the chrome around the buffer (`self.ui_font_size`) — no reflow, the GUI/web
    /// shells just rescale their chrome on the next render.
    fn set_ui_font_size(&mut self, font_size: u32) -> Effects {
        if font_size == self.ui_font_size {
            return Effects::none();
        }
        self.ui_font_size = font_size;
        self.persist_app_settings()
    }

    /// Persist a new reading-view width + apply it. No-op when unchanged (the Left/Right stepper
    /// clamps at the ends, so it lands here repeatedly). Nothing to reflow: the reading view is
    /// laid out shell-side from the measure, so the next frame is the new width.
    fn set_markdown_width(&mut self, width: MarkdownWidth) -> Effects {
        if width == self.markdown_width {
            return Effects::none();
        }
        self.markdown_width = width;
        self.persist_app_settings()
    }

    /// Keys in the search prompt. Text entry (insert / delete / caret) is owned by each shell's
    /// search input, which syncs the whole value via [`Self::search_set_query`]; the core handles
    /// the Search command keys (commit / abort / history / option toggles) via the keymap table,
    /// plus the option-chip row gestures (mirroring [`Self::on_picker_key`]): with a chip selected,
    /// Left/Right walk the row, Backspace/Delete remove, Enter cycles, Esc/typing deselect. A
    /// forwarded Left/Backspace with no chip selected is the "step into the chips from the query
    /// start" gesture each shell sends when the caret sits at column 0.
    pub fn on_search_key(&mut self, code: KeyCode, mods: Mods, _text: Option<String>) -> Effects {
        let no_chord = !mods.ctrl && !mods.alt;
        if let Some(sel) = self.view.search.chip_selected {
            let chips = self.view.search.option_chips();
            if chips.is_empty() {
                self.view.search.chip_selected = None;
            } else {
                let sel = sel.min(chips.len() - 1);
                match code {
                    KeyCode::Left if no_chord => {
                        self.view.search.chip_selected = Some(sel.saturating_sub(1));
                        return Effects::none();
                    }
                    KeyCode::Right if no_chord => {
                        self.view.search.chip_selected = (sel + 1 < chips.len()).then_some(sel + 1);
                        return Effects::none();
                    }
                    KeyCode::Esc => {
                        self.view.search.chip_selected = None;
                        return Effects::none();
                    }
                    KeyCode::Backspace | KeyCode::Delete if no_chord => {
                        return self.remove_search_chip(sel);
                    }
                    KeyCode::Enter if no_chord => {
                        return self.cycle_search_chip(sel);
                    }
                    KeyCode::Char(_) if no_chord => {
                        // Typing returns to the query (the shell's input takes the char).
                        self.view.search.chip_selected = None;
                    }
                    _ => {}
                }
            }
        } else if no_chord
            && matches!(code, KeyCode::Left | KeyCode::Backspace)
            && !self.view.search.option_chips().is_empty()
        {
            // Forwarded from the query start: step into the chip row, selecting the rightmost.
            return self.search_select_last_chip();
        }
        match lookup(KeyContext::Search, code, mods) {
            Some(b) => {
                // Hint observation: search-mode bindings resolve here rather than through
                // `run_action`, so mirror its pre-dispatch observation — the option hints
                // (Alt-c/w/e) must follow and rotate when their chord fires.
                let ctx = self.hint_env();
                let enabled = self.hints_enabled;
                let evs = self.hints.observe_action(&b.action, ctx, enabled);
                let observed = self.emit_hint_events(evs);
                observed.and(self.search_action(b.action))
            }
            None => Effects::none(),
        }
    }

    /// Select the rightmost option chip (the browser tag-input gesture — Left/Backspace at the
    /// query start steps into the row). No-op when there are no chips. Called directly by the
    /// rich shells (native `<input>` / `text_input`) and reached via [`Self::on_search_key`] from
    /// the TUI's forwarded boundary key.
    pub fn search_select_last_chip(&mut self) -> Effects {
        let n = self.view.search.option_chips().len();
        if n > 0 {
            self.view.search.chip_selected = Some(n - 1);
        }
        Effects::none()
    }

    /// Remove the selected option chip — reset the option it stands for to its default — and keep
    /// the selection on a neighbouring chip (or clear it when the row empties), then re-run search.
    fn remove_search_chip(&mut self, sel: usize) -> Effects {
        let Some(chip) = self.view.search.option_chips().get(sel).map(|c| c.id) else {
            return Effects::none();
        };
        match chip {
            ChipId::Case => self.view.search.options.case = CaseMode::Smart,
            ChipId::Word => self.view.search.options.whole_word = false,
            ChipId::Regex => self.view.search.options.regex = false,
            _ => {}
        }
        let remaining = self.view.search.option_chips().len();
        self.view.search.chip_selected = (remaining > 0).then(|| sel.min(remaining - 1));
        self.incremental_search()
    }

    /// Enter on the selected chip: cycle/toggle the option it stands for (case cycles
    /// smart → sensitive → insensitive → smart; word / regex flip). Keeps the selection on the
    /// same option while its chip is still present, else clamps into the row, then re-runs search.
    fn cycle_search_chip(&mut self, sel: usize) -> Effects {
        let Some(id) = self.view.search.option_chips().get(sel).map(|c| c.id) else {
            return Effects::none();
        };
        match id {
            ChipId::Case => self.cycle_search_case(),
            ChipId::Word => {
                self.view.search.options.whole_word = !self.view.search.options.whole_word
            }
            ChipId::Regex => self.view.search.options.regex = !self.view.search.options.regex,
            _ => {}
        }
        let chips = self.view.search.option_chips();
        self.view.search.chip_selected = chips
            .iter()
            .position(|c| c.id == id)
            .or_else(|| (!chips.is_empty()).then(|| sel.min(chips.len() - 1)));
        self.incremental_search()
    }

    fn cycle_search_case(&mut self) {
        self.view.search.options.case = match self.view.search.options.case {
            CaseMode::Smart => CaseMode::Sensitive,
            CaseMode::Sensitive => CaseMode::Insensitive,
            CaseMode::Insensitive => CaseMode::Smart,
        };
    }

    /// The Search-table actions (also reachable from the shell's action dispatch).
    pub fn search_action(&mut self, action: Action) -> Effects {
        match action {
            Action::SearchCommit => self.commit_search(),
            Action::SearchAbort => self.abort_search(),
            Action::SearchHistoryPrev => self.search_history_step(VerticalDirection::Up),
            Action::SearchHistoryNext => self.search_history_step(VerticalDirection::Down),
            Action::SearchDeleteWord => {
                let shortened = chips::pop_word(&self.view.search.query);
                // Goes through the setter so the history walk is abandoned and the search re-runs,
                // exactly as typing into the field would.
                self.search_set_query(shortened)
            }
            // The Alt-chord toggles deselect any chip — they're the "chord" interaction, distinct
            // from chip-row editing.
            Action::SearchToggleCase => {
                self.view.search.chip_selected = None;
                self.cycle_search_case();
                self.incremental_search()
            }
            Action::SearchToggleWord => {
                self.view.search.chip_selected = None;
                self.view.search.options.whole_word = !self.view.search.options.whole_word;
                self.incremental_search()
            }
            Action::SearchToggleRegex => {
                self.view.search.chip_selected = None;
                self.view.search.options.regex = !self.view.search.options.regex;
                self.incremental_search()
            }
            _ => Effects::none(),
        }
    }

    /// `Up`/`Down` (or `Alt-k`/`Alt-j`) in the search prompt: recall a prior query *with the match
    /// options it ran under* and re-run the incremental search, so stepping the history previews
    /// each match as you go. Restoring the options is not a nicety — a regex recalled under
    /// literal matching silently finds nothing. A step with nothing to recall leaves the prompt
    /// untouched.
    fn search_history_step(&mut self, dir: VerticalDirection) -> Effects {
        let current =
            HistoryEntry::with_options(self.view.search.query.clone(), self.view.search.options);
        match self.history_step(HistoryKind::Search, dir, current) {
            Some(entry) => {
                self.view.search.query = entry.value;
                self.view.search.options = entry.filters.match_options();
                self.incremental_search()
            }
            None => Effects::none(),
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) -> Effects {
        match action {
            ConfirmAction::Save { target, after } => self.save(target, true, after),
            ConfirmAction::DropStash { repo_id, oid } => self.request_str::<GitStashDrop>(
                GitStashDropParams {
                    repo_id: Some(repo_id),
                    buffer_id: None,
                    oid,
                },
                |result| Event::StashDone {
                    staged: false,
                    result,
                },
            ),
            ConfirmAction::AbandonOperation { buffer_id } => self.abort_operation(buffer_id),
            ConfirmAction::ReloadDiscard => self.reload(true),
            ConfirmAction::CloseDiscard => self.close_view(),
            ConfirmAction::ClosePickerView { buffer_id, view_id } => {
                self.close_picker_view(buffer_id, view_id)
            }
            ConfirmAction::DeletePath { path, noun } => self
                .request_str::<PathDelete>(PathDeleteParams { path }, move |result| {
                    Event::PathDeleted { noun, result }
                }),
            ConfirmAction::RemoveWorkspaceProject {
                workspace,
                path_index,
                relative_path,
            } => self.request_str::<WorkspaceRemoveProject>(
                WorkspaceRemoveProjectParams {
                    workspace,
                    path_index,
                    relative_path,
                },
                Event::WorkspaceProjectRemoved,
            ),
            ConfirmAction::RemoveWorkspaceRoot { workspace, path } => self
                .request_str::<WorkspaceRemoveRoot>(
                    WorkspaceRemoveRootParams { workspace, path },
                    Event::WorkspaceRootRemoved,
                ),
            ConfirmAction::DeleteWorkspace { name } => {
                let display = name.clone();
                self.request::<WorkspaceDelete>(WorkspaceDeleteParams { name }, move |r| {
                    // Surface the server's *message*, not the stringified `RpcError` (which carries
                    // a "RPC … returned error -32005:" prefix). The locally-active workspace is
                    // already guarded in `picker_stage_delete`, so an active-workspace refusal here
                    // means it's open in another window — for which "switch away" is wrong advice.
                    Event::WorkspaceDeleted(r.map_err(|e| {
                        if e.code == ErrorCode::ACTIVE_WORKSPACE_PREVENTS_DELETE.code() {
                            format!(
                                "\"{display}\" is active in another window — close it there first"
                            )
                        } else {
                            e.message
                        }
                    }))
                })
            }
            ConfirmAction::DeleteBranch { name, force } => self.git_delete_branch(name, force),
        }
    }

    /// Fire `git/checkout` for a branch row, naming the repo the *row* carried.
    ///
    /// The row's `repo_id` rather than a fresh resolution: resolution runs off the active buffer,
    /// and between opening the picker and pressing Enter the user may have switched buffers — or a
    /// transient preview may have closed — which would land the checkout in a different repo than
    /// the one whose branches are on screen.
    fn git_checkout(&mut self, repo_id: String, branch: String, create: bool) -> Effects {
        let echoed = branch.clone();
        self.request::<GitCheckout>(
            GitCheckoutParams {
                repo_id: Some(repo_id),
                buffer_id: Some(self.view.buffer.buffer_id),
                branch,
                create,
            },
            move |r| Event::CheckedOut {
                branch: echoed.clone(),
                result: r.map_err(|e| e.message),
            },
        )
    }

    /// Fire `workspace/bind_worktree`: point this repo at `worktree` in the current context and
    /// activate the result. An empty `worktree` unbinds — the `main` row.
    ///
    /// Lands like a workspace switch because it *is* one: the result is a `WorkspaceActivateResult`
    /// with the landing buffer already opened, so this reuses the same event the switcher does.
    fn bind_worktree(&mut self, repo_id: String, worktree: String) -> Effects {
        let hide = self.close_picker();
        hide.and(self.request_str::<WorkspaceBindWorktree>(
            WorkspaceBindWorktreeParams {
                repo_id: Some(repo_id),
                worktree,
                open_last: true,
                // What we are looking at, so the rebind lands us on the same file on the new tree
                // rather than on whatever heads the workspace's MRU. The server can't infer it: a
                // client may hold several viewports.
                buffer_id: Some(self.view.buffer.buffer_id),
                ..Default::default()
            },
            |r| Event::WorktreeBound(r.map_err(|e| e.to_string())),
        ))
    }

    /// Fire `git/worktree_add`, naming the repo the *row* carried — same reasoning as
    /// [`Self::git_checkout`]: resolution runs off the active buffer, which may have moved between
    /// opening the picker and pressing Enter.
    ///
    /// `branch` goes to the server exactly as the user typed it. The directory name is derived
    /// there and never comes back to touch this string.
    fn git_worktree_add(
        &mut self,
        repo_id: String,
        branch: String,
        create_branch: bool,
    ) -> Effects {
        let echoed = branch.clone();
        self.request::<GitWorktreeAdd>(
            GitWorktreeAddParams {
                repo_id: Some(repo_id),
                buffer_id: Some(self.view.buffer.buffer_id),
                branch,
                create_branch,
            },
            move |r| Event::WorktreeAdded {
                branch: echoed.clone(),
                result: r.map_err(|e| e.message),
            },
        )
    }

    /// Fire `git/worktree_remove`. `force` is set only by escalating from a `Dirty` refusal that
    /// has already told the user what it would discard.
    fn git_worktree_remove(&mut self, repo_id: String, name: String, force: bool) -> Effects {
        let echoed = name.clone();
        self.request::<GitWorktreeRemove>(
            GitWorktreeRemoveParams {
                repo_id: Some(repo_id),
                buffer_id: Some(self.view.buffer.buffer_id),
                name,
                force,
            },
            move |r| Event::WorktreeRemoved {
                name: echoed.clone(),
                result: r.map_err(|e| e.message),
            },
        )
    }

    /// Fire `git/delete_branch`. `force` is set only by the escalation from a `NotMerged` refusal.
    fn git_delete_branch(&mut self, branch: String, force: bool) -> Effects {
        let Some(repo_id) = self.branch_picker_repo_id() else {
            return Effects::none();
        };
        let echoed = branch.clone();
        self.request::<GitDeleteBranch>(
            GitDeleteBranchParams {
                repo_id: Some(repo_id),
                buffer_id: Some(self.view.buffer.buffer_id),
                branch,
                force,
            },
            move |r| Event::BranchDeleted {
                branch: echoed.clone(),
                forced: force,
                result: r.map_err(|e| e.message),
            },
        )
    }

    /// The repo the open branch picker is listing, taken off any of its rows (they all carry the
    /// same id — it's a property of the listing). `None` when the picker is closed or empty, which
    /// is also why a delete confirm can't outlive its picker.
    fn branch_picker_repo_id(&self) -> Option<String> {
        let p = self.picker.as_ref()?;
        if p.kind != PickerKind::GitBranches {
            return None;
        }
        p.items.iter().find_map(|it| match it {
            PickerItem::GitBranch { repo_id, .. } => Some(repo_id.clone()),
            _ => None,
        })
    }

    /// The `+ Create` row in the branch picker: create the typed branch and switch to it, which is
    /// what `git checkout -b` does and what the Explorer/Workspaces create rows do (they open and
    /// activate what they made). Also the only way into a repo whose HEAD is unborn.
    pub fn branch_create_from_query(&mut self) -> Effects {
        let (name, repo_id) = {
            let Some(p) = &self.picker else {
                return Effects::none();
            };
            if p.kind != PickerKind::GitBranches {
                return Effects::none();
            }
            (p.query.trim().to_string(), self.branch_picker_repo_id())
        };
        if name.is_empty() {
            return Effects::error("Type a name to create");
        }
        // An unborn-HEAD repo lists no branches, so there's no row to read the id off — fall back
        // to letting the server resolve from the active buffer, which is the same rule the picker
        // itself opened with. (Not a drift risk the way the checkout case is: nothing was listed.)
        let repo_id = repo_id.unwrap_or_default();
        let hide = self.close_picker();
        hide.and(if repo_id.is_empty() {
            let echoed = name.clone();
            self.request::<GitCheckout>(
                GitCheckoutParams {
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                    branch: name,
                    create: true,
                },
                move |r| Event::CheckedOut {
                    branch: echoed.clone(),
                    result: r.map_err(|e| e.message),
                },
            )
        } else {
            self.git_checkout(repo_id, name, true)
        })
    }

    /// Open the save-as prompt pre-filled with `(path_index, input)`. A brand-new buffer (empty
    /// input) in a multi-root workspace starts focused in the root field so you choose where to save;
    /// otherwise the path field has focus (the root is known). Kicks off a `directory/list` so the
    /// path field's ghost suggestions are ready.
    fn open_save_as(&mut self, path_index: u32, input: String) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let field = if workspace_paths.len() > 1 && input.is_empty() {
            ChipEditorField::Root
        } else {
            ChipEditorField::Path
        };
        // Files are offered: completing onto an existing one is how you overwrite it.
        let mut ed = PathEditor::new(input, field, path_index, true);
        ed.sync_dir_listing(&workspace_paths);
        self.prompt = Some(Prompt::SaveAs(Box::new(ed)));
        self.refresh_save_as_listing()
    }

    /// Declining a save-as overwrite returns to the path input (re-opened pre-filled, so a tweak
    /// and re-save is one gesture); other declines just close the dialog.
    fn decline_confirm(&mut self, action: ConfirmAction) -> Effects {
        if let ConfirmAction::Save {
            target: Some((path_index, input)),
            // Declining discards any save-and-quit/close intent — a cancelled save must not
            // quit or close.
            after: _,
        } = action
        {
            return self.open_save_as(path_index, input);
        }
        Effects::none()
    }

    /// The prompt's Yes/Save button.
    fn accept_prompt(&mut self) -> Effects {
        match self.prompt.take() {
            Some(Prompt::Confirm { action, .. }) => self.run_confirm(action),
            Some(p @ (Prompt::SaveAs(_) | Prompt::OpenPath(_))) => {
                // Submit via the same path as Enter.
                self.prompt = Some(p);
                self.on_prompt_key(KeyCode::Enter, Mods::default(), None)
            }
            // Informational dialogs have nothing to accept — the button dismisses them, which
            // taking the prompt above already did.
            Some(Prompt::LspInfo(_) | Prompt::AppInfo(_)) | None => Effects::none(),
        }
    }

    /// Dismiss the prompt without accepting (Esc / backdrop click).
    pub fn decline_prompt(&mut self) -> Effects {
        if let Some(Prompt::Confirm { action, .. }) = self.prompt.take() {
            return self.decline_confirm(action);
        }
        Effects::none()
    }

    /// `buffer/reload`, mapping `WOULD_DISCARD_CHANGES` to a confirmation that retries with
    /// `force: true`.
    pub fn reload(&mut self, force: bool) -> Effects {
        self.request::<BufferReload>(
            BufferReloadParams {
                buffer_id: self.view.buffer.buffer_id,
                force,
            },
            move |__r| {
                Event::ReloadTried(match __r {
                    Ok(r) => Ok(ReloadTry::Reloaded(r)),
                    Err(e) if e.code == ErrorCode::WOULD_DISCARD_CHANGES.code() => {
                        Ok(ReloadTry::NeedsConfirm)
                    }
                    Err(e) => Err(e.message),
                })
            },
        )
    }

    /// Twin of [`Self::on_event`] for keystrokes: drives symbol highlighting after the keystroke is
    /// handled. Cursor moves keyed here resolve asynchronously (via `CursorMsg` → `on_event`), so
    /// what this boundary uniquely catches is the *synchronous* search-clear paths — `drop_search`
    /// (Esc in Normal), `abort_search` / `commit_search` (the prompt) — which never reach `on_event`.
    pub fn on_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        // Every key is hint-relevant activity (the idle gate), and dispatch may have moved the
        // hint context (opened an overlay, left Insert) — re-sync so the corner follows.
        self.hints.note_input();
        let fx = self.dispatch_key(code, mods, text);
        let fx = fx.and(self.sync_hint_context());
        fx.and(self.sync_decoration_follow())
    }

    fn dispatch_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        // Input isn't gated here: client-only actions (Quit, scroll, help, mode toggles) stay
        // usable while the connection is down — most importantly, the user can still quit. Anything
        // that actually talks to the server is dropped at the point of issue (see `request`), so a
        // disconnected key press just no-ops instead of corrupting state.

        // An open modal prompt owns the keyboard outright; a picker likewise.
        if self.prompt.is_some() {
            let fx = self.on_prompt_key(code, mods, text);
            return fx;
        }
        if self.picker.is_some() {
            let fx = self.on_picker_key(code, mods, text);
            return fx;
        }
        // The workspace-settings overlay likewise owns the keyboard while open.
        if self.workspace_settings.is_some() {
            return self.on_workspace_settings_key(code, mods, text);
        }
        // As does the application-settings overlay.
        if self.app_settings.is_some() {
            return self.on_app_settings_key(code, mods, text);
        }

        // Search mode owns the keyboard: control keys via its table, anything printable is
        // query text (case-preserved — no normalisation of the literal query).
        if self.view.mode == Mode::Search {
            let fx = self.on_search_key(code, mods, text);
            return fx;
        }

        // An active sneak session owns the keyboard: keystrokes refine the query or pick a label.
        if self.view.sneak.is_some() {
            return self.on_sneak_key(code, mods, text);
        }

        // Stateful captures run before table lookup, like the TUI.
        match self.view.pending {
            Pending::Find {
                dir,
                till,
                extend,
                count,
            } => {
                self.view.pending = Pending::None;
                if code == KeyCode::Esc {
                    return Effects::none();
                }
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(ch) = ch.filter(|c| !c.is_control()) else {
                    return Effects::none();
                };
                let motion = Motion::FindChar {
                    ch,
                    direction: dir,
                    count,
                    till,
                };
                // `BeginFind` only armed the capture; the repeatable thing is this resolved
                // find (with its target char), so record it here.
                self.last_repeat = Some(RepeatTarget::Find(motion.clone()));
                return self.move_motion(motion, extend);
            }
            Pending::Surround(target) => {
                self.view.pending = Pending::None;
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(delimiter) = ch.filter(|c| !c.is_control()) else {
                    return Effects::none(); // Esc / non-char cancels
                };
                return self.edit::<InputSurround>(InputSurroundParams {
                    buffer_id: self.view.buffer.buffer_id,
                    delimiter,
                    target,
                });
            }
            Pending::Transform => {
                self.view.pending = Pending::None;
                // The next key picks the transform; an unmapped key (or Esc) just cancels.
                let kind = text
                    .as_deref()
                    .and_then(|t| t.chars().next())
                    .and_then(CaseKind::from_char);
                let Some(kind) = kind else {
                    return Effects::none();
                };
                return self.edit::<InputTransformCase>(InputTransformCaseParams {
                    buffer_id: self.view.buffer.buffer_id,
                    kind,
                    // Insert mode has no selection, so the server scans for the identifier
                    // under the caret; Normal mode recases exactly the selection (a point
                    // being the single char under the block).
                    scan_at_cursor: self.view.mode == Mode::Insert,
                });
            }
            Pending::Leader => {
                self.view.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::Leader, code, mods) {
                    // `Space g` re-arms into the git sub-leader from inside `run_action`, which is
                    // why the clear above happens first.
                    return self.run_action(b.action, 1, false, mods.shift);
                }
                return Effects::none();
            }
            Pending::LeaderGit => {
                self.view.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::LeaderGit, code, mods) {
                    return self.run_action(b.action, 1, false, mods.shift);
                }
                // An unbound key (or Esc) cancels the chord, exactly like the leader.
                return Effects::none();
            }
            Pending::LeaderAgent => {
                self.view.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::LeaderAgent, code, mods) {
                    return self.run_action(b.action, 1, false, mods.shift);
                }
                return Effects::none();
            }
            Pending::None => {}
        }

        // Count lexer (Normal and Read modes): digits accumulate; `0` only continues a count
        // (it's line-start otherwise).
        if matches!(self.view.mode, Mode::Normal | Mode::Read) && !mods.ctrl && !mods.alt {
            if let KeyCode::Char(c) = code {
                if c.is_ascii_digit() && (c != '0' || self.view.count.is_some()) {
                    let d = c.to_digit(10).unwrap();
                    self.view.count = Some(self.view.count.unwrap_or(0).saturating_mul(10) + d);
                    return Effects::none();
                }
            }
        }
        // Whether a count was *typed*, captured before it is consumed. Almost every motion treats
        // "no count" and "count 1" identically — but not the absolute jumps: bare `g` means the
        // field's top, while `1g` means buffer line 1, which in a composed view may not be in the
        // field at all. One is a destination, the other is a request that can be refused.
        let counted = self.view.count.is_some();
        let count = self.view.count.take().unwrap_or(1).max(1);
        // Insert mode never holds a selection, so Shift+motion must not extend one (the arrow
        // bindings match any modifier). It just moves the caret.
        let extend = mods.shift && self.view.mode != Mode::Insert;

        // Global table first (mode-identical Ctrl shortcuts), then the mode's own. Read mode skips
        // Global entirely and re-declares what it wants: Global's edit chords are *line*-grain
        // (join, indent, move lines) and the reading view acts on blocks, so it opts in binding by
        // binding — `Ctrl-z`, `Ctrl-a` — rather than inheriting a keymap written for the editor.
        // (It was once simply read-only; block editing came later, and the opt-in list is what
        // that blanket exclusion.)
        let ctx = match self.view.mode {
            Mode::Normal => KeyContext::Normal,
            Mode::Insert => KeyContext::Insert,
            Mode::Read => KeyContext::Read,
            Mode::Search => return Effects::none(), // handled above
        };
        let global = if self.view.mode == Mode::Read {
            None
        } else {
            lookup(KeyContext::Global, code, mods)
        };
        if let Some(b) = global.or_else(|| lookup(ctx, code, mods)) {
            return self.run_action(b.action, count, counted, extend);
        }

        // Insert mode: unmatched printable input is text.
        if self.view.mode == Mode::Insert && !mods.ctrl && !mods.alt {
            if let Some(typed) = text {
                let typed: String = typed
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\t')
                    .collect();
                if !typed.is_empty() {
                    return self.edit::<InputText>(InputTextParams {
                        buffer_id: self.view.buffer.buffer_id,
                        text: typed,
                        select_pasted: false,
                        replace_selection: false,
                        at: None,
                    });
                }
            }
        }
        Effects::none()
    }

    fn run_action(
        &mut self,
        action: Action,
        count: u32,
        // Whether the user actually typed a count. See the capture site — the absolute jumps are
        // the one family for which "no count" and "count 1" are different requests.
        counted: bool,
        extend: bool,
    ) -> Effects {
        // Hint observation: every resolved binding passes through here — except search-mode keys,
        // which resolve in `on_search_key` and observe there. Observed (and its record requests
        // emitted) *before* dispatch, so the context is the one the hint displayed in (dispatch may
        // open a picker and move it) — and so a `Quit`'s follow record hits the wire ahead of
        // `Effect::Exit` tearing the process down, rather than queuing behind it and being lost.
        let hint_ctx = self.hint_env();
        let enabled = self.hints_enabled;
        // `Space h` owns its hint learning inside the engine's `dismiss` — observing it here
        // would rotate a followed intro hint before the dismissal ran, dismissing its
        // replacement instead.
        let evs = if matches!(action, Action::DismissHint) {
            Vec::new()
        } else {
            self.hints.observe_action(&action, hint_ctx, enabled)
        };
        let hint_fx = self.emit_hint_events(evs);
        let task = self.dispatch_action(action, count, counted, extend);
        // Remember the action for `.` to replay. Recorded at dispatch (the RPC is still in flight —
        // a failed motion just leaves a harmless no-op target). `RepeatMotion` itself isn't
        // repeatable, so it never overwrites the target with itself; find records its resolved
        // motion at the capture site instead.
        if action.is_repeatable() {
            self.last_repeat = Some(RepeatTarget::Action {
                action,
                count,
                counted,
            });
        }
        hint_fx.and(task)
    }

    /// Which element of the view on screen is a shell's **input** — and so, whether this view is
    /// a shell at all.
    ///
    /// Derived from the window rather than from a kind flag on the view, which is what keeps every
    /// shell kind-blind: they already paint an editor element, and this only says which of them
    /// carries the caret's special meanings.
    pub fn shell_input(&self) -> Option<aether_protocol::ui::FieldId> {
        self.view.window.as_ref()?.root.input_element()
    }

    /// Whether this view is **composed** — its content generated by the server rather than loaded
    /// from a file — which is the condition `Enter` follows a line under rather than asking a
    /// language server.
    ///
    /// Two facts, and neither is a kind check on the view: `is_patch` is the flag an open already
    /// carries, and an input element is a thing the window itself says it has.
    fn composed_view(&self) -> bool {
        self.view.buffer.is_patch || self.shell_input().is_some()
    }

    /// Whether the caret is in that input right now — the condition `Enter` submits under.
    pub fn shell_input_focused(&self) -> bool {
        self.view.focused_is_input()
    }

    /// What the shell's input holds, when it is a single line — read off the window, which is the
    /// only copy of the text the client has. `None` when the focused element is not an input, or
    /// when it holds more than one line.
    fn shell_input_text(&self) -> Option<String> {
        let element = self.shell_input()?;
        let window = self.view.window.as_ref()?;
        let node = window
            .root
            .editors()
            .into_iter()
            .find(|n| matches!(n, Element::Editor { element: e, .. } if *e == element))?;
        let Element::Editor { rows, lines, .. } = node else {
            return None;
        };
        if *rows > 1 || lines.len() > 1 {
            return None;
        }
        Some(
            lines
                .first()
                .map(|l| {
                    l.visual_rows
                        .iter()
                        .flat_map(|r| r.segments.iter())
                        .map(|s| s.text.as_str())
                        .collect::<String>()
                })
                .unwrap_or_default(),
        )
    }

    /// Whether `Up`/`Down` should recall rather than move: the caret is in a shell's input, that
    /// input is one line, and the caret is on it.
    fn shell_recall_applies(&self) -> bool {
        self.shell_input_focused()
            && self.shell_input_text().is_some()
            && self.view.buffer.cursor.position.line == 0
    }

    /// Walk the shell's command history and install the entry, replacing the input's one line.
    ///
    /// Through `input/replace_line` rather than by sending the text as a value: the input is a
    /// document the server owns, and this is the ordinary edit for "this line now reads that" —
    /// which is what keeps undo, the pushes and every other viewer coherent.
    fn shell_history_step(&mut self, dir: VerticalDirection) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        let current = HistoryEntry::bare(self.shell_input_text().unwrap_or_default());
        match self.history_step(HistoryKind::Shell, dir, current) {
            Some(entry) => self.edit::<InputReplaceLine>(InputReplaceLineParams {
                buffer_id,
                text: entry.value,
            }),
            None => Effects::none(),
        }
    }

    fn dispatch_action(
        &mut self,
        action: Action,
        count: u32,
        // See `run_action`: distinguishes bare `g` (the field's top) from `1g` (buffer line 1).
        counted: bool,
        extend: bool,
    ) -> Effects {
        use Action as A;
        let buffer_id = self.view.buffer.buffer_id;
        // While disconnected (boot `Connecting` or a mid-session `Reconnecting`) the buffer is
        // read-only: the server can't accept edits, so the RPCs are dropped anyway. Entering Insert
        // would leave the user in a mode where typing silently vanishes — it reads as a hang. Refuse
        // the insert-entering actions and stay in Normal, with a hint so the inaction is explained.
        if self.conn != ConnState::Connected
            && matches!(
                action,
                A::EnterInsert(_)
                    | A::OpenLineBelow
                    | A::OpenLineAbove
                    | A::Change
                    | A::CutChange
                    | A::ReadInsert { .. }
                    | A::ReadChange
                    | A::ReadOpenBlock { .. }
            )
        {
            // Grouped: each blocked keystroke while disconnected refreshes one hint, not a stack.
            return Effects::toast_grouped_detail(
                "Not connected",
                "Editing is unavailable until the server is back",
                ToastKind::Info,
                "edit-blocked",
            );
        }
        match action {
            // ---- motions ----
            A::MoveChar(direction) => self.move_motion(Motion::Char { direction, count }, extend),
            // `b` / `Alt-b` in Normal (backward only — `w` there selects words via
            // `CursorSelectWord`), `Alt-←` / `Alt-→` in Insert (both directions).
            A::MoveWord { dir, boundary } => self.move_motion(
                Motion::Word {
                    direction: dir,
                    count,
                    boundary,
                },
                extend,
            ),
            A::MoveWordEnd { dir, boundary } => self.move_motion(
                Motion::WordEnd {
                    direction: dir,
                    count,
                    boundary,
                },
                extend,
            ),
            // `Up`/`Down` in a shell's input recall commands, as in any shell — but only while the
            // input is one line and the caret is on it. A multi-line command (`Alt-Enter`) is text
            // you are editing, and replacing it wholesale to walk a list would be a keystroke that
            // destroys work.
            A::MoveVisualLine(direction) if self.shell_recall_applies() => {
                self.shell_history_step(direction)
            }
            A::MoveVisualLine(direction) => {
                let Some(viewport_id) = self.view.viewport_id else {
                    return Effects::none();
                };
                self.move_motion(
                    Motion::VisualLine {
                        viewport_id,
                        direction,
                        count,
                    },
                    extend,
                )
            }
            A::MoveLogicalLine(direction) => self.move_motion(
                Motion::LogicalLine {
                    direction,
                    count,
                    preserve_col: true,
                },
                extend,
            ),
            A::MoveLineStart => self.move_motion(Motion::LineStart, extend),
            A::MoveLineEnd => self.move_motion(Motion::LineEnd, extend),
            A::MoveLineFirstNonblank => self.move_motion(Motion::LineFirstNonblank, extend),
            A::MoveLogicalLineFirstNonblank(direction) => self.move_motion(
                Motion::LogicalLineFirstNonblank { direction, count },
                extend,
            ),
            A::GotoLine { last } => {
                // Uncounted, these are the field's own edges, and the server is the only side that
                // knows where those are: `BufferStart`/`BufferEnd` resolve against the motion
                // scope, so in a composed view `g` lands on the focused hunk's first line rather
                // than the file's.
                //
                // This also retires a real bug on bare `Alt-g`, which used to synthesise an
                // absolute line from `window.view_line_count` — a **view** line count standing in
                // for a **buffer** line. Its own comment admitted the consequence ("`Alt-g` lands
                // on a clamped line rather than the file's true last"); the two spaces differ for
                // exactly the views this work is about.
                //
                // Counted, they stay absolute jumps: `N g` is buffer line N, 1-based, matching the
                // gutter, and `N Alt-g` is the N-th line from the field's end — resolved by the
                // server, the only side that knows where the focused element ends.
                if !counted {
                    let motion = if last {
                        Motion::BufferEnd
                    } else {
                        Motion::BufferStart
                    };
                    return self.move_jump(motion, extend);
                }
                let motion = if last {
                    Motion::LineFromEnd { count }
                } else {
                    Motion::Goto {
                        position: LogicalPosition {
                            line: count.saturating_sub(1),
                            col: 0,
                        },
                    }
                };
                self.move_jump(motion, extend)
            }
            A::MatchBracket { inner } => self.move_motion(Motion::MatchBracket { inner }, extend),
            // A page is its own motion, not a visual-line step with a big count. The count here
            // is *pages* — the row span comes from the viewport's height server-side, which the
            // server already tracks and which is the same number this shell would have used.
            //
            // Sending it as `Motion::VisualLine { count: rows / 2 }` put a number nobody typed into
            // the field the server's count rule reads as an assertion, so the whole variant had to
            // clamp to keep `v` working near a file's end — and `Alt-j` clamped with it, which is
            // how `100 Alt-j` landed short while `100 j` refused.
            A::PageMotion { dir, half } => {
                let Some(viewport_id) = self.view.viewport_id else {
                    return Effects::none();
                };
                self.move_motion(
                    Motion::Page {
                        viewport_id,
                        direction: dir,
                        count,
                        half,
                    },
                    extend,
                )
            }
            // `o`/`Alt-o` step the **outline**, and which outline that is depends on the view: an
            // ordinary buffer's is its document symbols, a composed view's is its files. The same
            // split `Space o` makes, and deliberately the same source — the view answers, so the
            // key and the picker cannot disagree about what the stops are, and the client need not
            // know what kind of view it is looking at.
            A::NavUnit(dir) => self.step_view(
                dir == Direction::Forward,
                count,
                extend,
                NavigateGrain::Outline,
            ),
            A::BeginFind { dir, till } => {
                self.view.pending = Pending::Find {
                    dir,
                    till,
                    extend,
                    count,
                };
                Effects::none()
            }
            A::BeginSneak { big } => {
                // Arm the session; the first typed char triggers the first `sneak/update`. `extend`
                // (Shift) and `big` (`Alt-s`) are fixed for the whole session.
                self.view.sneak = Some(SneakState {
                    extend,
                    big,
                    ..SneakState::default()
                });
                Effects::none()
            }

            // ---- selection ----
            A::SelectWord { boundary } => self.request_str::<CursorSelectWord>(
                CursorSelectWordParams {
                    buffer_id,
                    boundary,
                    extend,
                    count,
                },
                Event::CursorMsg,
            ),
            A::SelectLine(direction) => self.request_str::<CursorSelectLine>(
                CursorSelectLineParams {
                    buffer_id,
                    direction,
                    extend,
                    count,
                },
                Event::CursorMsg,
            ),
            A::SelectAll => self.request_str::<CursorSelectAll>(
                CursorSelectAllParams { buffer_id },
                Event::CursorMsg,
            ),
            A::SwapAnchor { forward_only } => self.request_str::<CursorSwapAnchor>(
                CursorSwapAnchorParams {
                    buffer_id,
                    forward_only,
                },
                Event::CursorMsg,
            ),
            A::CollapseSelection => {
                if self.view.buffer.cursor.is_point() {
                    return Effects::none();
                }
                let pos = self.view.buffer.cursor.position;
                self.request_str::<CursorSet>(
                    CursorSetParams {
                        buffer_id,
                        position: pos,
                        anchor: pos,
                        granularity: Granularity::Char,
                    },
                    Event::CursorMsg,
                )
            }
            A::TreeExpand => self.tree_select(TreeSelectDirection::Expand, count),
            A::TreeContract => self.tree_select(TreeSelectDirection::Contract, count),
            A::MotionUndo => self.motion_history::<CursorUndo>(count),
            A::MotionRedo => self.motion_history::<CursorRedo>(count),
            A::RepeatMotion => {
                // `.`'s own count is how many times to replay; the stored target keeps the
                // original count baked in. The replayed requests enqueue in order at build
                // time (the transport sends in call order), so the server applies them
                // sequentially even though the result futures resolve independently.
                let Some(target) = self.last_repeat.clone() else {
                    return Effects::none();
                };
                let mut fx = Effects::none();
                for _ in 0..count.max(1) {
                    let step = match &target {
                        RepeatTarget::Action {
                            action,
                            count,
                            counted,
                        } => self.dispatch_action(*action, *count, *counted, extend),
                        RepeatTarget::Find(motion) => self.move_motion(motion.clone(), extend),
                    };
                    fx = fx.and(step);
                }
                fx
            }
            // Geometry (pixel scroll, cell metrics) and viewport plumbing — the shell executes
            // these against its own state.
            A::PlaceCursor(place) => {
                Effects::one(Effect::ShellAction(ShellAction::PlaceCursor(place)))
            }
            A::Scroll { dir, unit } => {
                Effects::one(Effect::ShellAction(ShellAction::Scroll { dir, unit }))
            }
            A::ToggleWrap => {
                // Re-layout: capture a content anchor first (against the current window), then let
                // the shell flip wrap + re-render; the shell restores the anchor when it adopts the
                // new window. Keeps the viewport on the same content across the reflow.
                let mut fx = Effects::one(Effect::SaveContentAnchor);
                fx.push(Effect::ShellAction(ShellAction::ToggleWrap));
                fx
            }
            A::OpenHelp => {
                // The keyboard-shortcut reference is the Keybindings picker: the rows are built
                // from the keymap tables here in the core and shipped on the `picker/view`, so
                // every shell gets the same searchable list through the ordinary picker pipeline.
                self.open_picker(PickerKind::Keybindings, None, None, false, None)
            }
            A::OpenWorkspaceSettings => {
                // The workspace-settings overlay now lives in the core (state + key handling); every
                // shell renders it from `session.workspace_settings`.
                self.open_workspace_settings()
            }
            A::OpenAppSettings => {
                // Like the workspace-settings overlay, the app-settings overlay lives in the core;
                // shells render it from `session.app_settings`.
                self.open_app_settings();
                Effects::none()
            }
            // Fetched rather than assembled client-side: the build identity, pid, port and counts
            // all describe the *server* process, and half the value of the dialog is that it
            // reports the daemon you're actually connected to rather than the one you assume.
            A::ShowAppInfo => {
                // Disconnected is when the diagnostics dialog matters most — and the RPC would
                // be silently dropped (the core sends nothing while not `Connected`). Open the
                // client-side snapshot instead: our build identity + the connection state.
                if !matches!(self.conn, ConnState::Connected) {
                    self.prompt = Some(Prompt::AppInfo(None));
                    return Effects::none();
                }
                self.request::<AppInfoGet>(AppInfoParams {}, |r| {
                    Event::AppInfoLoaded(r.map_err(|e| e.message))
                })
            }
            // Dismiss the corner hint: a deliberate "not now" — down-weight it (heavier than a
            // lapsed display) and rotate to another. No-op on an empty corner.
            A::DismissHint => {
                let ctx = self.hint_env();
                let enabled = self.hints_enabled;
                let evs = self.hints.dismiss(ctx, enabled);
                self.emit_hint_events(evs)
            }
            // Toggle hints app-wide — the keyboard twin of the settings-overlay row.
            A::ToggleHints => self.toggle_hints(),
            A::NavBack | A::NavForward => {
                let forward = matches!(action, A::NavForward);
                let f = move |res: Result<NavStepResult, RpcError>| Event::NavDone {
                    forward,
                    result: res.map_err(|e| e.message),
                };
                let direction = if forward {
                    Direction::Forward
                } else {
                    Direction::Backward
                };
                self.request::<NavStep>(
                    NavStepParams {
                        buffer_id,
                        direction,
                    },
                    f,
                )
            }

            // ---- mode transitions ----
            A::EnterInsert(where_) => {
                // Refused up front rather than letting the mode change and toasting per keystroke:
                // a read-only buffer has nothing Insert mode could do.
                if self.view.buffer.read_only {
                    return crate::session::read_only_toast();
                }
                self.view.mode = Mode::Insert;
                self.enter_insert_at(where_)
            }
            A::LeaveInsert => {
                self.view.mode = Mode::Normal;
                Effects::none()
            }
            A::BeginLeader => {
                self.view.pending = Pending::Leader;
                Effects::none()
            }
            A::BeginGitLeader => {
                self.view.pending = Pending::LeaderGit;
                Effects::none()
            }
            A::BeginAgentLeader => {
                self.view.pending = Pending::LeaderAgent;
                Effects::none()
            }

            // ---- edits ----
            A::Backspace => self.edit::<InputBackspace>(BufferOnlyParams { buffer_id }),
            A::DeleteWord { dir, boundary } => {
                self.edit::<InputDeleteWord>(InputDeleteWordParams {
                    buffer_id,
                    direction: dir,
                    boundary,
                    count,
                })
            }
            A::NewlineIndent => self.edit::<InputNewlineAndIndent>(InputNewlineAndIndentParams {
                buffer_id,
                park_before: false,
            }),
            A::UnjoinLines => self.edit::<InputNewlineAndIndent>(InputNewlineAndIndentParams {
                buffer_id,
                park_before: true,
            }),
            // The whitespace itself is the server's call — it owns the buffer's indent style, so
            // `Tab` lands spaces or a tab to match what `Enter` and `Ctrl-l` already produce.
            A::InsertTab => self.edit::<InputTab>(BufferOnlyParams { buffer_id }),
            A::DeletePoint => self.edit::<InputDelete>(CountedEditParams {
                buffer_id,
                count: 1,
            }),
            // A selection delete is atomic — to remove more, extend the selection (matches
            // `Change`/`Cut`). The count is intentionally ignored: looping a selection-delete
            // degenerates into deleting `count - 1` characters forward, which reads as a bug.
            A::DeleteSelection => self.edit::<InputDelete>(CountedEditParams {
                buffer_id,
                count: 1,
            }),
            A::DeleteLine => self.edit::<InputDeleteLine>(BufferOnlyParams { buffer_id }),
            A::Undo => self.undo_redo::<EditUndo>(count),
            A::Redo => self.undo_redo::<EditRedo>(count),
            A::MoveLines(direction) => self.request_str::<InputMoveLines>(
                InputMoveLinesParams {
                    buffer_id,
                    direction,
                    count,
                },
                Event::EditDone,
            ),
            A::JoinLines => self.repeat_edit::<InputJoinLines>(count),
            A::Indent => self.repeat_edit::<InputIndent>(count),
            A::Dedent => self.repeat_edit::<InputDedent>(count),
            // Insert mode has no selection: scan for the number at the caret rather than acting on
            // the (nonexistent) selection, and collapse afterwards.
            A::IncrementNumber => self.adjust_value(true, count),
            A::DecrementNumber => self.adjust_value(false, count),
            A::ToggleComment(style, target) => {
                self.edit::<InputToggleComment>(ToggleCommentParams {
                    buffer_id,
                    style,
                    target,
                })
            }
            A::OpenLineBelow | A::OpenLineAbove => {
                // Vim's `o`/`O` as one server-side edit (park, open, land — smart indent
                // below, unindented above); stay in Insert (TUI semantics).
                self.view.mode = Mode::Insert;
                let side = if matches!(action, A::OpenLineBelow) {
                    LineSide::Below
                } else {
                    LineSide::Above
                };
                self.edit::<InputOpenLine>(InputOpenLineParams { buffer_id, side })
            }

            // ---- clipboard ----
            A::Copy => self.copy(CopyScope::Selection),
            A::CopyLine => self.copy(CopyScope::Line),
            A::Cut => self.cut(CopyScope::Selection),
            // Cut to the clipboard, then drop into Insert at the resulting gap — the server's cut
            // collapses the selection and parks the cursor there, so all that's left is the mode flip.
            A::CutChange => {
                self.view.mode = Mode::Insert;
                self.cut(CopyScope::Selection)
            }
            A::CutLine => self.cut(CopyScope::Line),
            A::Paste => read_clipboard_fx(PasteKind::Before { count }),
            A::ReplaceClipboard => read_clipboard_fx(PasteKind::Replace { count }),
            A::PasteAtCursor => read_clipboard_fx(PasteKind::AtCursor),
            A::ReplaceLineClipboard => read_clipboard_fx(PasteKind::Line),
            A::Change => {
                self.view.mode = Mode::Insert;
                self.edit::<InputChange>(CountedEditParams {
                    buffer_id,
                    count: 1,
                })
            }
            A::ChangeLine => self.edit::<InputChangeLine>(BufferOnlyParams { buffer_id }),
            A::BeginSurround(target) => {
                self.view.pending = Pending::Surround(target);
                Effects::none()
            }
            A::Unsurround(target) => {
                self.edit::<InputUnsurround>(InputUnsurroundParams { buffer_id, target })
            }
            A::BeginTransform => {
                self.view.pending = Pending::Transform;
                Effects::none()
            }

            // ---- search (core methods; the prompt-only actions also route here from
            // `Session::on_search_key`'s table lookup) ----
            A::EnterSearch => self.enter_search(false),
            A::EnterSearchToCursor => self.enter_search(true),
            A::SearchCommit
            | A::SearchAbort
            | A::SearchHistoryPrev
            | A::SearchHistoryNext
            | A::SearchToggleCase
            | A::SearchToggleWord
            | A::SearchToggleRegex
            | A::SearchDeleteWord => self.search_action(action),
            A::SearchCycle(direction) => self.search_cycle(direction, count, extend),
            A::SearchFromSelection => self.search_from_selection(),
            A::JumplistStep(direction) => {
                self.jumplist_step(direction, count, JumplistStepScope::Full)
            }
            A::ClearJumplist => self.clear_jumplist(),
            A::JumplistStepInFile(direction) => {
                self.jumplist_step(direction, count, JumplistStepScope::CurrentFile)
            }
            A::DropSearch => self.drop_search(),

            // ---- app ----
            // The server tears down all per-client state on disconnect, so quitting is just
            // closing the window.
            A::Quit => Effects::one(Effect::Exit),
            A::Save => self.save(None, false, AfterSave::Nothing),
            A::SaveAndQuit => self.save(None, false, AfterSave::Quit),
            // Save, then close. On a commit message that commits — but only because *closing*
            // commits, not because this chord knows anything about git.
            A::SaveAndClose => self.save(None, false, AfterSave::Close),
            A::SaveAs => {
                // Prefill with the buffer's current workspace-relative path, like the web dialog.
                let (path_index, input) = self
                    .view
                    .buffer
                    .path
                    .as_deref()
                    .and_then(|p| strip_longest_root(p, &self.workspace_paths))
                    .unwrap_or((0, String::new()));
                self.open_save_as(path_index, input)
            }
            A::OpenPath => {
                // The workspace-agnostic path overlay, seeded `~/` so its completions are on screen
                // before the first keystroke. Files as well as directories: a file is what it
                // opens. The shell focuses it and syncs typed text via `open_path_set_input`; Enter
                // opens via `workspace/open_path`.
                let mut ed = PathEditor::absolute(HOME_PREFIX.to_string(), true);
                ed.sync_dir_listing(&self.workspace_paths);
                self.prompt = Some(Prompt::OpenPath(Box::new(ed)));
                self.refresh_open_path_listing()
            }
            A::Reload => {
                if self.view.buffer.path.is_none() {
                    return Effects::toast_detail(
                        "A scratch has no path",
                        "There's nothing on disk to reload",
                        ToastKind::Warning,
                    );
                }
                self.reload(false)
            }
            A::ToggleKeep => {
                // Un-keeping the tether *releases* it: the buffer demotes to an ordinary transient
                // AND the client stops exiting when it closes — one-way; a re-keep is just a plain
                // keep. Atomic with the demotion, so it inherits the dirty guard — but audibly,
                // since the user asked for a release.
                if self.tethered() {
                    if self.view.focused_unsaved() {
                        return Effects::toast_detail(
                            "Unsaved changes",
                            "Save before releasing the tether",
                            ToastKind::Warning,
                        );
                    }
                    let view_id = self.view.view_id;
                    return self.request_str::<ViewSetTransient>(
                        ViewSetTransientParams {
                            view_id,
                            transient: true,
                        },
                        |r| Event::TetherReleased(r.map(|res| res.transient)),
                    );
                }
                // Keeps the **view**, not the file the cursor is in. A working-changes view is a
                // transient view over permanent files: toggling the focused element pinned a file
                // that was never going anywhere and left the view to close itself on the next
                // thing opened. For an ordinary view the two ids are the same and nothing changes.
                let target = !self.view.view_transient;
                // Refuse to make a view with unsaved edits transient — it would auto-close (and
                // discard them) once hidden. View-wide: *any* of its documents being dirty counts,
                // since closing the view drops them all. Silent no-op; pinning permanent, or
                // toggling a clean view, is fine.
                if target && self.view.unsaved() {
                    return Effects::none();
                }
                self.request_str::<ViewSetTransient>(
                    ViewSetTransientParams {
                        view_id: self.view.view_id,
                        transient: target,
                    },
                    |r| Event::KeepToggled(r.map(|res| res.transient)),
                )
            }
            A::CopyRelativePath => self.copy_buffer_path(false),
            A::CopyAbsolutePath => self.copy_buffer_path(true),
            A::CopyWebUrl => self.copy_web_url(),
            A::NewScratch => {
                // Opening a fresh scratch is a buffer switch — record the origin so Alt-Left
                // returns (folded into the open's `record_nav_from`).
                self.request_str::<ViewOpen>(
                    ViewOpenParams {
                        record_nav_from: Some(buffer_id),
                        ..Default::default()
                    },
                    Event::Switched,
                )
            }
            A::CloseView => {
                // The whole **view**, not just the element under the cursor. A composed view holds
                // several documents, and asking only about the focused one meant closing a
                // working-changes view with unsaved edits in a hunk you had scrolled past went
                // through without a word. (Nothing was lost — those files are separate buffers and
                // stay open — but "close without asking" is not what the prompt is for.) The
                // second term is the flag the status dot uses, so the prompt and the dot cannot
                // disagree about whether the view is dirty.
                if self.view.unsaved() {
                    self.prompt = Some(Prompt::Confirm {
                        kind: ConfirmKind::DiscardOnClose {
                            // The view's name: in a composed view the dirty document may not be
                            // the focused one, so naming that file would point at the wrong thing.
                            label: self.view.view_label.clone(),
                        },
                        action: ConfirmAction::CloseDiscard,
                    });
                    return Effects::none();
                }

                self.close_view()
            }
            // Spawning the new process is irreducibly shell-side (and GUI-only) — the shell reads
            // the workspace/path from its session and detaches a sibling `ae --gui`. The core just
            // asks for it; non-GUI shells ignore the action.
            A::NewWindow => Effects::one(Effect::ShellAction(ShellAction::NewWindow(
                self.current_view_target(),
            ))),

            // ---- git ----
            A::ToggleDiffView => {
                let Some(viewport_id) = self.view.viewport_id else {
                    return Effects::none();
                };
                let enabled = !self.diff_view;
                // Re-layout: capture a content anchor first (against the current window); it's
                // restored when the re-laid-out window is adopted (Event::DiffViewSet →
                // WindowAdopted), keeping the viewport on the same content across the toggle.
                let mut fx = Effects::one(Effect::SaveContentAnchor);
                fx = fx.and(self.request_str::<GitSetDiffView>(
                    GitSetDiffViewParams {
                        viewport_id,
                        enabled,
                    },
                    move |result| Event::DiffViewSet { enabled, result },
                ));
                fx
            }
            // A patch's changes live across its elements, each a window onto a different file;
            // an ordinary buffer's are its own hunks. The view knows which, so the client asks it
            // either way.
            A::NextHunk | A::PrevHunk => self.step_view(
                matches!(action, A::NextHunk),
                count,
                extend,
                NavigateGrain::Change,
            ),
            A::StageChange { scope } | A::UnstageChange { scope } | A::RevertChange { scope } => {
                let hunk_action = match action {
                    A::StageChange { .. } => HunkAction::Stage,
                    A::UnstageChange { .. } => HunkAction::Unstage,
                    _ => HunkAction::Revert,
                };
                self.request_str::<GitApplyHunk>(
                    GitApplyHunkParams {
                        buffer_id,
                        action: hunk_action,
                        scope,
                    },
                    move |result| Event::HunkApplied {
                        action: hunk_action,
                        scope,
                        result,
                    },
                )
            }

            A::ResolveConflict { side } => self.request_str::<GitResolveConflict>(
                GitResolveConflictParams { buffer_id, side },
                move |result| Event::ConflictResolved { side, result },
            ),

            // The one git verb that asks first. The abort resets the working tree from disk, so
            // every conflict resolution in it goes and the undo stack can't reach any of it — and
            // unlike the other destructive keys (revert is undoable, a stash is stored, an
            // uncommit keeps its changes), there is nothing to reach for afterwards.
            //
            // Confirmed only when we know an operation is stopped: the status bar's indicator is
            // that knowledge, and it's also what names the operation in the question. With none in
            // flight the request goes straight out and the server answers `NothingInProgress` —
            // there is nothing to lose and so nothing to ask about.
            A::GitAbortOperation => {
                if let Some(operation) = self.stopped_operation() {
                    self.prompt = Some(Prompt::Confirm {
                        kind: ConfirmKind::AbandonOperation { operation },
                        action: ConfirmAction::AbandonOperation { buffer_id },
                    });
                    return Effects::none();
                }
                self.abort_operation(buffer_id)
            }

            A::GitStashPush { staged } => self.request_str::<GitStashPush>(
                GitStashPushParams {
                    // Resolved server-side from the buffer we're on, like every other git verb.
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                    // No prompt: `git stash` with no message is the common gesture, and git's own
                    // `WIP on <branch>` names the entry well enough to recognise in the picker.
                    message: None,
                    staged,
                },
                move |result| Event::StashDone { staged, result },
            ),

            // One announced operation at a time. Not a server rule but a client one, because the
            // things that represent it are single-slot: the status bar shows one indicator, and
            // `Space g x` resolves its target *from* that indicator — so a second operation would
            // leave the user unable to say which one they meant to stop. Slightly over-strict in a
            // multi-repo workspace (an operation in one repo blocks starting one in another), which
            // is the same simplification the single indicator already makes.
            A::GitFetch | A::GitPush | A::GitPull if self.git_operation.is_some() => {
                Effects::toast_detail(
                    "A git operation is already running",
                    "Stop it first",
                    ToastKind::Info,
                )
            }

            // Server-resolved repo, like every other git verb: the client has no repo in hand.
            A::ShowWorkingChanges => self.request_str::<aether_protocol::git::GitShow>(
                aether_protocol::git::GitShowParams {
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                    target: aether_protocol::git::ShowTarget::WorkingChanges,
                    focus_path: None,
                },
                Event::Shown,
            ),
            A::GitFetch => self.request_str::<GitFetch>(
                GitFetchParams {
                    // Resolved server-side from the buffer we're on, like every other git verb.
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                },
                Event::FetchDone,
            ),

            A::GitPush => self.request_str::<GitPush>(
                GitPushParams {
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                },
                Event::PushDone,
            ),

            A::GitPull => self.request_str::<GitPull>(
                GitPullParams {
                    repo_id: None,
                    buffer_id: Some(self.view.buffer.buffer_id),
                },
                Event::PullDone,
            ),

            // Cancelling names the repo from the operation itself, never from the active buffer:
            // a transient preview closing mid-push would otherwise re-resolve to a different repo
            // than the one the indicator is showing.
            A::GitCancel => match self.git_operation.as_ref() {
                Some((repo_id, _)) => self.request_str::<GitCancel>(
                    GitCancelParams {
                        repo_id: repo_id.clone(),
                    },
                    Event::CancelDone,
                ),
                None => Effects::none(),
            },

            // The shell you can type into. Which one that is, is the server's to decide — it
            // holds the shells and knows which are busy; the client only says whether the view in
            // front of it is already a shell, since a `Space b` there means "another one".
            // Where the key was pressed is all the client says: whether that view is already a
            // conversation — and so whether this means "another one" — is the server's to know.
            A::AgentOpen => self.request::<aether_protocol::agent::AgentOpen>(
                aether_protocol::agent::AgentOpenParams {
                    from_view: Some(self.view.view_id),
                    agent: None,
                },
                Event::AgentOpened,
            ),
            // Cancelling names the *view*, exactly as `Space Alt-b` does for a shell: the
            // conversation in front of you is the one you meant.
            A::AgentCancel if !self.agent_turns.contains_key(&self.view.view_id) => {
                Effects::toast("Nothing is running here", ToastKind::Info)
            }
            A::AgentCancel => self.request_str::<aether_protocol::agent::AgentCancel>(
                aether_protocol::agent::AgentCancelParams {
                    view_id: self.view.view_id,
                },
                Event::AgentCancelled,
            ),
            A::AgentAnswer { allow } => self.request::<aether_protocol::agent::AgentRespond>(
                aether_protocol::agent::AgentRespondParams {
                    view_id: self.view.view_id,
                    answer: if allow {
                        aether_protocol::agent::Answer::Allow
                    } else {
                        aether_protocol::agent::Answer::Decline
                    },
                    // The one the conversation is blocked on: there is only ever one.
                    block: None,
                },
                Event::AgentAnswered,
            ),
            A::ShellOpen => self.request::<aether_protocol::shell::ShellOpen>(
                aether_protocol::shell::ShellOpenParams {
                    new: self.shell_input().is_some(),
                },
                Event::ShellOpened,
            ),
            // Cancelling names the *view*, exactly as `Space g x` names the repo: the shell in
            // front of you is the one you meant, and a shell you are not looking at is not
            // something `Space Alt-b` should reach into.
            A::ShellCancel if self.shell_input().is_none() => {
                Effects::toast("Not a shell", ToastKind::Info)
            }
            A::ShellCancel if !self.shell_runs.contains_key(&self.view.view_id) => {
                Effects::toast("Nothing is running here", ToastKind::Info)
            }
            A::ShellCancel => self.request_str::<aether_protocol::shell::ShellCancel>(
                aether_protocol::shell::ShellCancelParams {
                    view_id: self.view.view_id,
                },
                Event::ShellCancelled,
            ),
            // Reached from Normal-mode `Enter` (`Activate`) with the input focused. The guard is
            // kept so that nothing can submit from anywhere else, whatever dispatches it.
            A::SubmitInput if self.shell_input_focused() => {
                // Kept until the server answers: the line enters the recall list only if it was
                // accepted, which is the same rule the server records by, so the two cannot
                // disagree — and the input is cleared by the time the answer comes.
                self.pending_shell_submit = self.shell_input_text().map(|t| t.trim().to_string());
                self.request::<aether_protocol::view::ViewSubmitInput>(
                    aether_protocol::view::ViewSubmitInputParams {
                        view_id: self.view.view_id,
                    },
                    Event::InputSubmitted,
                )
            }
            A::SubmitInput => Effects::none(),

            A::GitUncommit => self.request_str::<GitReset>(
                GitResetParams {
                    // Resolved server-side from the buffer we're on, the same rule
                    // `git/prepare_commit` uses — the client never needs to know repo ids.
                    buffer_id: Some(self.view.buffer.buffer_id),
                    repo_id: None,
                    rev: "HEAD^".to_string(),
                },
                Event::Uncommitted,
            ),

            A::GitCommit { amend } => {
                // A message already being written is switched to, never rewritten. Preparing
                // again would overwrite `COMMIT_EDITMSG` underneath a dirty buffer: the watcher
                // would flag it externally-modified, and the save on the way to the commit would
                // then *refuse* — losing the message to a keystroke meant to resume it.
                if let Some(pending) = self.pending_commit.clone() {
                    if pending.buffer_id == self.view.buffer.buffer_id {
                        return Effects::toast_detail(
                            "Already writing this commit",
                            "Close the view to commit",
                            ToastKind::Info,
                        );
                    }
                    let mut fx = self.request_str::<ViewOpen>(
                        ViewOpenParams {
                            view_id: Some(pending.view_id),
                            record_nav_from: Some(self.view.buffer.buffer_id),
                            ..Default::default()
                        },
                        Event::Switched,
                    );
                    fx.push(Effect::Toast {
                        title: "Commit message already open".to_string(),
                        body: None,
                        kind: ToastKind::Info,
                        group: None,
                    });
                    return fx;
                }
                // The repo is resolved server-side, from the buffer we're looking at — that's
                // where the buffer→repo mapping already lives, and it's the only thing it's
                // resolved from, so a scratch buffer is refused rather than guessed at.
                self.request_str::<GitPrepareCommit>(
                    GitPrepareCommitParams {
                        buffer_id: Some(self.view.buffer.buffer_id),
                        amend,
                        ..Default::default()
                    },
                    move |result| Event::CommitPrepared { amend, result },
                )
            }

            // ---- pickers ----
            A::OpenPicker(PickerKind::Explorer) => self.open_explorer(false),
            A::OpenPicker(kind) => self.open_picker(kind, None, None, false, None),
            A::OpenFilesInFileDir => self.open_files_in_file_dir(),
            A::OpenGrepFromSelection => self.open_grep_from_selection(),
            A::OpenExplorerAtRoot => self.open_explorer(true),

            // ---- LSP ----
            // `Enter` means "follow what's under the cursor", and which resolver answers depends on
            // what the cursor is *in*, not on what kind of view is open.
            //
            // **Composed view, cursor in a bound element** — the element windows a real file and the
            // cursor is already inside that file's document, so the most-wanted destination is the
            // file itself: promote it to its own view. That is an ordinary `view/open`, naming this
            // view and the element — the file is named through the view because a file at a
            // revision has no path of its own. The test is structural rather than a kind flag:
            // "the buffer I am editing is not the one I opened" is exactly what composed means.
            //
            // This spends `Enter` on the file rather than on go-to-definition, knowingly. Inside a
            // patch, go-to-definition genuinely works (the element is a real buffer with a real
            // language server), which is what makes the trade affordable — `Enter` twice gets you
            // there, and `Ctrl-Enter` is not available as a shortcut because it already means
            // "activate in a new window" in the reading view.
            // `Enter` in a shell's input means the same thing in Normal mode as in Insert: run it.
            // Declared before the composed-view arm below, which would otherwise fire first — the
            // input windows a different buffer than the view's own, so it looks like a hunk over a
            // file and `Enter` would promote it to a view of its own.
            A::Activate if self.shell_input_focused() => {
                self.dispatch_action(A::SubmitInput, count, counted, extend)
            }
            A::Activate if self.view.buffer.buffer_id != self.view.view_buffer => {
                // Not transient: you asked for this file, so it stays. `record_nav_from` is the
                // view, so `Backspace` returns to the review rather than to the file you were
                // already in.
                let from = self.view.view_buffer;
                self.request_str::<ViewOpen>(
                    ViewOpenParams {
                        view_id: Some(self.view.view_id),
                        element: Some(self.view.focused_element),
                        record_nav_from: Some(from),
                        ..Default::default()
                    },
                    Event::Switched,
                )
            }
            // **Generated text** — a patch's metadata block or a deletion, a shell's output:
            // there is no element to promote because there is no file to window, so the only thing
            // that can say where the line leads is the document's own account of itself, which is
            // server-side. `view/follow_line` is total over the kinds of it, so this dispatch does
            // not branch on which.
            //
            // The condition, not the destination, is the client's: `view/follow_line` would answer
            // "nowhere" for an ordinary buffer, but asking it first would make every
            // go-to-definition wait for that answer.
            A::Activate if self.composed_view() => self
                .request_str::<aether_protocol::view::ViewFollowLine>(
                    aether_protocol::view::ViewFollowLineParams {
                        view_id: self.view.view_id,
                    },
                    Event::LineFollowed,
                ),
            A::Activate => self
                .request_str::<LspGotoDefinition>(LspBufferParams { buffer_id }, Event::Definition),
            // One verb, "tell me about the thing under the cursor", resolved against the mode:
            // over source that's the language server's hover, over the reading view it's the target
            // of the focused link or image. They were separate bindings on `Tab` until `Tab` was
            // needed for moving between a view's editors.
            A::FocusNextElement => self.focus_element(FocusTarget::Step {
                direction: FocusStep::Next,
            }),
            A::FocusPrevElement => self.focus_element(FocusTarget::Step {
                direction: FocusStep::Previous,
            }),

            A::Hover if self.view.mode == Mode::Read => self.read_show_target(),
            A::Hover => {
                self.request_str::<LspHover>(LspBufferParams { buffer_id }, Event::HoverInfo)
            }
            A::Format => {
                self.request_str::<LspFormat>(LspBufferParams { buffer_id }, Event::FormatDone)
            }
            A::ShowDiagnostic => self.show_diagnostic(),
            A::ShowCommitInfo => self.show_commit_info(),
            A::NextDiagnostic | A::PrevDiagnostic => {
                let direction = if matches!(action, A::NextDiagnostic) {
                    DiagnosticDirection::Next
                } else {
                    DiagnosticDirection::Prev
                };
                self.request_str::<LspNavigateDiagnostic>(
                    LspNavigateDiagnosticParams {
                        buffer_id,
                        direction,
                        count,
                        extend,
                    },
                    Event::DiagNav,
                )
            }

            // ---- markdown reading view ----
            A::ToggleReadView => self.toggle_read_view(),
            A::ReadStep(dir) => self.read_step(
                dir == Direction::Forward,
                count,
                extend,
                crate::markdown::Stop::is_block,
            ),
            A::ReadStepLink(dir) => self.read_step_link_in_block(dir == Direction::Forward, count),
            A::ReadShowTarget => self.read_show_target(),
            A::ReadSelectBlock(dir) => {
                self.read_select_block(dir == Direction::Forward, count, extend)
            }
            A::ReadInsert { at_end } => self.read_insert(at_end),
            A::ReadChange => self.read_change(),
            A::ReadOpenBlock { above } => self.request_str::<InputOpenBlock>(
                OpenBlockParams { buffer_id, above },
                Event::OpenBlockDone,
            ),
            A::MoveBlock { down, unit } => self.request_str::<InputMoveBlock>(
                MoveBlockParams {
                    buffer_id,
                    direction: if down {
                        VerticalDirection::Down
                    } else {
                        VerticalDirection::Up
                    },
                    unit,
                },
                Event::BlockEditDone,
            ),
            A::ReadCutBlock => self.request_str::<InputDeleteBlock>(
                BufferOnlyParams { buffer_id },
                Event::BlockEditDone,
            ),
            // The same removal, and deliberately the same RPC: what separates delete from cut
            // is what the *client* does with the payload, exactly as in the editor
            // (`DeleteSelection` vs `Cut`). The resolver computes the removed source either
            // way, so the payload is dropped here rather than asked for — no clipboard.
            A::ReadDeleteBlock => self
                .request_str::<InputDeleteBlock>(BufferOnlyParams { buffer_id }, |r| {
                    Event::BlockEditDone(r.map(|r| BlockEditResult { text: None, ..r }))
                }),
            A::ReadPasteBlock { replace } => {
                Effects::one(Effect::ReadClipboard(PasteKind::Block { replace }))
            }
            A::ReadBlockDepth { deeper } => self.request_str::<InputBlockDepth>(
                BlockDepthParams { buffer_id, deeper },
                Event::BlockEditDone,
            ),
            A::ReadEnds { last } => self.read_ends(last),
            A::ReadActivate => self.read_activate(),
            A::ReadActivateNewWindow => self.read_activate_new_window(),
            A::ReadCopy => self.read_copy(),
        }
    }

    /// Toggle the reading view on the current buffer (`Space u`): ask the server for the other
    /// kind, which it remembers for the file. Non-markdown buffers toast instead.
    fn toggle_read_view(&mut self) -> Effects {
        if self.view.buffer.language.as_deref() != Some("markdown") {
            return Effects::toast_grouped(
                "No reader view available",
                ToastKind::Info,
                "read-view",
            );
        }
        if self.view.read.is_some() {
            self.read_exit_for_edit();
            // Frame the reading position first — the block's row is still known — so the anchor
            // then captured has the cursor on screen, and the editor opens showing it: what
            // leaving the reader always did.
            return Effects::one(Effect::RevealCursor(RevealStyle::Jump))
                .and(self.open_sibling(aether_protocol::ui::ViewKind::Editor));
        }
        self.open_sibling(aether_protocol::ui::ViewKind::Reader)
    }

    /// Step the reading focus (`j`/`k`, `Tab`, `o` — the predicate picks the element class) and
    /// move the server cursor to the landed element's start: focus is derived from the cursor, so
    /// the `Goto` *is* the focus change. Quiet no-op at the ends. With `extend` (Shift) the step
    /// *selects* instead: the landed block's far line becomes the cursor via `cursor/set` +
    /// `Granularity::Line` (the server snaps to whole-line normal form), the anchor holding at the
    /// selection's origin — the first extending step plants it at the focused block's near edge, so
    /// the selection covers origin.=landed as whole blocks.
    fn read_step(
        &mut self,
        forward: bool,
        count: u32,
        extend: bool,
        pred: impl Fn(&crate::markdown::Stop) -> bool,
    ) -> Effects {
        enum Landing {
            Goto(LogicalPosition),
            Select {
                position: LogicalPosition,
                anchor: LogicalPosition,
            },
        }
        let landing = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor;
            // Step from the byte the bar is drawn at, not the raw cursor. A block selection parks
            // the cursor on its last line's terminating newline, and only some block spans reach
            // that far — a fence stops at its closing backtick — so the raw byte resolved forward
            // to the block *after* the fence and `j` silently skipped one.
            let byte = read.focus_byte(cursor.position);
            let Some(mut idx) = crate::markdown::element_at(&read.elements, byte) else {
                return Effects::none();
            };
            // Stepping is class-relative: when the derived focus doesn't match the predicate (a
            // focused link while stepping blocks), anchor at the innermost *matching* element
            // containing the cursor. Without this a lone-link paragraph traps `k`: the Goto to
            // the paragraph start re-derives focus to the link (innermost at that byte), and
            // stepping back from the link finds its own containing paragraph, forever.
            if !pred(&read.elements[idx]) {
                if let Some(c) = crate::markdown::containing_element(&read.elements, byte, &pred) {
                    idx = c;
                }
            }
            let mut moved = None;
            for _ in 0..count.max(1) {
                match crate::markdown::step_element(&read.elements, idx, forward, &pred) {
                    Some(next) => {
                        idx = next;
                        moved = Some(next);
                    }
                    None => break,
                }
            }
            let Some(idx) = moved else {
                return Effects::none();
            };
            if extend {
                let (first, last) = read.block_lines(idx);
                let anchor = if cursor.is_point() {
                    let origin = read.block_focus(cursor.position).unwrap_or(idx);
                    let (ofirst, olast) = read.block_lines(origin);
                    if forward {
                        ofirst
                    } else {
                        olast
                    }
                } else {
                    cursor.anchor
                };
                Landing::Select {
                    position: if forward { last } else { first },
                    anchor,
                }
            } else {
                // Land outside any leading interactive span, so the step selects the block alone
                // (the bar) — `l` opts into its links.
                Landing::Goto(read.pos_of(crate::markdown::block_rest_byte(&read.elements, idx)))
            }
        };
        match landing {
            Landing::Goto(position) => self.move_motion(Motion::Goto { position }, false),
            Landing::Select { position, anchor } => self.request_str::<CursorSet>(
                CursorSetParams {
                    buffer_id: self.view.buffer.buffer_id,
                    position,
                    anchor,
                    granularity: Granularity::Line,
                },
                Event::CursorMsg,
            ),
        }
    }

    /// `x`/`Alt-x` (+Shift): the editor's `cursor/select_line` state machine translated to block
    /// grain (see `cursor_select_line_once` server-side). Plain presses
    /// *walk*: `x` snaps the focused block first, then a press on a whole-block selection selects
    /// the *next* block alone; `Alt-x` selects the *previous* block even on the first press (the
    /// editor's first-press asymmetry). Shift *grows* — `Shift-x` the bottom, `Alt-Shift-x` the top
    /// — but only once a whole-block span exists: a non-whole selection first snaps (Shift) or
    /// collapses to its edge block (plain), without advancing. The cursor stays at whichever end it
    /// occupied (fresh selections default it to the bottom); edges saturate at the document ends.
    /// The editor's empty-line-point-is-whole rule has no analog — block walks step block to block
    /// and can't get stuck on separators. Counts iterate the machine; one `cursor/set` +
    /// `Granularity::Line` lands the result in whole-line normal form.
    fn read_select_block(&mut self, forward: bool, count: u32, extend: bool) -> Effects {
        let (position, anchor) = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor;
            let is_block = crate::markdown::Stop::is_block;
            let step = |idx: usize, fwd: bool| {
                crate::markdown::step_element(&read.elements, idx, fwd, is_block)
            };
            let (a, p) = (read.byte_of(cursor.anchor), read.byte_of(cursor.position));
            let cursor_at_top = !cursor.is_point() && p < a;
            // Current state: the block range under the selection, and whether the selection
            // already covers it whole ([`crate::session::ReadView::selection_blocks`]). A
            // point cursor is "the focused block, not yet selected".
            let Some((mut top, mut bottom, mut whole)) = read.selection_blocks(&cursor) else {
                return Effects::none();
            };
            let mut fresh = cursor.is_point();
            for _ in 0..count.max(1) {
                if fresh {
                    if !forward {
                        // Alt-x's first press selects the block *above*, saturating at the top.
                        top = step(top, false).unwrap_or(top);
                    }
                    bottom = top;
                    (fresh, whole) = (false, true);
                    continue;
                }
                if !whole {
                    // Snap before advancing: Shift keeps the (now whole) range, a plain press
                    // collapses to the direction's edge block.
                    if !extend {
                        if forward {
                            top = bottom;
                        } else {
                            bottom = top;
                        }
                    }
                    whole = true;
                    continue;
                }
                if forward {
                    let next = step(bottom, true).unwrap_or(bottom);
                    if !extend {
                        top = next;
                    }
                    bottom = next;
                } else {
                    let prev = step(top, false).unwrap_or(top);
                    if !extend {
                        bottom = prev;
                    }
                    top = prev;
                }
            }
            let (top_first, _) = read.block_lines(top);
            let (_, bottom_last) = read.block_lines(bottom);
            if cursor_at_top {
                (top_first, bottom_last)
            } else {
                (bottom_last, top_first)
            }
        };
        self.request_str::<CursorSet>(
            CursorSetParams {
                buffer_id: self.view.buffer.buffer_id,
                position,
                anchor,
                granularity: Granularity::Line,
            },
            Event::CursorMsg,
        )
    }

    /// Leave the reading view for the editor, locally and at once — the caller sets the
    /// destination mode, and asks for the editor's view with [`Self::open_sibling`], or the next
    /// pushed window would bring the reading view straight back.
    fn read_exit_for_edit(&mut self) {
        self.view.read = None;
        if self.view.mode == Mode::Read {
            self.view.mode = Mode::Normal;
        }
    }

    /// `i`/`a`: to the editor, inserting at the selection's start / end. An extended
    /// selection uses the editor's own Insert-entry motions (`SelectionEdge`, resolved
    /// server-side); a bare reading position enters at the focused block's start, or its
    /// append position — the caret gap before the block's terminating newline (buffer end
    /// when the last block has none).
    ///
    /// A document with no blocks at all (empty, or nothing but blank lines) has no focus to
    /// resolve, so the *document* is the target: `i` at its start, `a` at its end. Without that
    /// fallback the reading view is a dead end on exactly the buffer you most want to type
    /// into — nothing on screen and every edit transition a silent no-op.
    fn read_insert(&mut self, at_end: bool) -> Effects {
        let target = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor;
            if cursor.is_point() {
                let byte = match read.block_focus(cursor.position) {
                    Some(f) if at_end => read.block_append_byte(f),
                    Some(f) => read.elements[f].span().start,
                    None if at_end => read.text.len() as u32,
                    None => 0,
                };
                Some(read.pos_of(byte))
            } else {
                None
            }
        };
        self.read_exit_for_edit();
        self.view.mode = Mode::Insert;
        let fx = self.open_sibling(aether_protocol::ui::ViewKind::Editor);
        fx.and(match target {
            Some(position) => self.move_motion(Motion::Goto { position }, false),
            None => self.enter_insert_at(if at_end {
                // `LastLineEnd`, not `SelectionEnd`: a block selection is always in whole-line
                // normal form, so its last char *is* the terminating newline and "one past the
                // last char" is column 0 of the separator line below — typing there wedges the
                // text into the gap between blocks. The bare-position branch above parks before
                // that newline, and `a` has to mean the same thing either way.
                InsertWhere::LastLineEnd
            } else {
                InsertWhere::SelectionStart
            }),
        })
    }

    /// `Ctrl-e`: rewrite the selected block(s) — a *content-only* change. The selection is
    /// re-materialized from block start to the last content char (`Granularity::Char`,
    /// exact), so the bottom block's terminating newline and every separator survive the
    /// delete: the editor's `Change` then lands Insert on an emptied line with clean blanks
    /// either side (vim's `cc` shape, not a raw block-delete that would splice the next
    /// block up). A multi-block selection collapses to one new block the same way.
    fn read_change(&mut self) -> Effects {
        let (anchor, position) = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let Some((top, bottom, _)) = read.selection_blocks(&self.view.buffer.cursor) else {
                return Effects::none();
            };
            let start = read.elements[top].span().start;
            (
                read.pos_of(start),
                read.pos_of(read.block_content_end(bottom).max(start)),
            )
        };
        self.read_exit_for_edit();
        self.view.mode = Mode::Insert;
        let buffer_id = self.view.buffer.buffer_id;
        self.open_sibling(aether_protocol::ui::ViewKind::Editor)
            .and(self.request_str::<CursorSet>(
                CursorSetParams {
                    buffer_id,
                    position,
                    anchor,
                    granularity: Granularity::Char,
                },
                Event::CursorMsg,
            ))
            .and(self.edit::<InputChange>(CountedEditParams {
                buffer_id,
                count: 1,
            }))
    }

    /// `h`/`l`: step the Enter target among the interactive elements *inside the focused block*.
    /// `l` with no target selects the block's first interactive; `h` from the first steps back
    /// *out* — the cursor returns to the block's rest byte, so the bar stands alone again. Past the
    /// last link, and `h` with nothing selected, are quiet no-ops, like `j`/`k` at the document's
    /// ends.
    fn read_step_link_in_block(&mut self, forward: bool, count: u32) -> Effects {
        let target = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor.position;
            let Some(block) = read.block_focus(cursor) else {
                return Effects::none();
            };
            let ring =
                crate::markdown::interactive_within(&read.elements, read.elements[block].span());
            let Some(last) = ring.len().checked_sub(1) else {
                return Effects::none();
            };
            let current = read
                .target_focus(cursor)
                .and_then(|t| ring.iter().position(|&i| i == t));
            let steps = count.max(1) as usize;
            let next = if forward {
                match current {
                    Some(p) if p >= last => return Effects::none(),
                    Some(p) => Some((p + steps).min(last)),
                    None => Some((steps - 1).min(last)),
                }
            } else {
                match current {
                    None => return Effects::none(),
                    // Stepping back past the first element deselects: the block alone.
                    Some(p) if p < steps => None,
                    Some(p) => Some(p - steps),
                }
            };
            let byte = match next {
                Some(i) => read.elements[ring[i]].span().start,
                None => crate::markdown::block_rest_byte(&read.elements, block),
            };
            read.pos_of(byte)
        };
        self.move_motion(Motion::Goto { position: target }, false)
    }

    /// `Tab`: show the focused element's target without following it — a link's URL, an
    /// image's source, a footnote's definition text — in the hover popover (the editor's
    /// Tab-reveals-hover at reading grain; the popover's own keys apply, so `Ctrl-c` copies
    /// the shown target via `keymap::hover_action`). Quiet no-op on plain blocks, like
    /// `Enter`.
    fn read_show_target(&mut self) -> Effects {
        let text = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.view.buffer.cursor.position) else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Stop::Link { href, .. } => href.clone(),
                crate::markdown::Stop::Image { src, .. } => src.clone(),
                crate::markdown::Stop::FootnoteRef { label, .. } => {
                    match crate::markdown::footnote_def_span(&read.blocks, label) {
                        Some(span) => read.slice(span).trim_end().to_string(),
                        None => format!("No definition for footnote [{label}]"),
                    }
                }
                _ => return Effects::none(),
            }
        };
        Effects::one(Effect::ShowHover(HoverText::Blocks(vec![HoverBlock {
            severity: None,
            text,
        }])))
    }

    /// A pointer press on the reading view: the shell hit-tests its own rendering to a source byte
    /// (an element's span start) and the core moves the server cursor there — focus then derives
    /// from the cursor exactly like a keyboard step, so clicking a block sets the reading selection
    /// in every shell through one path.
    pub fn read_click(&mut self, byte: u32) -> Effects {
        let target = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            if read.loading && read.text.is_empty() {
                return Effects::none();
            }
            read.pos_of(byte)
        };
        self.move_motion(Motion::Goto { position: target }, false)
    }

    /// A pointer press that landed ON an interactive element — the shells route clicks on
    /// their rendered link/image/footnote nodes here, so `byte` is that element's span start:
    /// focus it like [`Self::read_click`], then follow links and footnote references like
    /// `Enter` — pointing at a target and clicking should act. Images stay arm-only (`Enter`
    /// opens them externally, which a stray click shouldn't).
    pub fn read_click_activate(&mut self, byte: u32) -> Effects {
        let follow = self.view.read.as_ref().and_then(|read| {
            read.elements.iter().position(|e| {
                e.span().start == byte
                    && matches!(
                        e,
                        crate::markdown::Stop::Link { .. }
                            | crate::markdown::Stop::FootnoteRef { .. }
                    )
            })
        });
        let fx = self.read_click(byte);
        match follow {
            Some(idx) => fx.and(self.read_activate_element(idx)),
            None => fx,
        }
    }

    /// Ctrl-click on a link — the pointer sibling of `Ctrl-Enter`: a *relative-path* link
    /// opens in a new window (GUI) / app tab (web); anything else falls back to the plain
    /// click-follow.
    pub fn read_click_new_window(&mut self, byte: u32) -> Effects {
        let href = self.view.read.as_ref().and_then(|read| {
            read.elements.iter().find_map(|e| match e {
                crate::markdown::Stop::Link { span, href }
                    if span.start == byte && !href.starts_with('#') && !has_url_scheme(href) =>
                {
                    Some(href.clone())
                }
                _ => None,
            })
        });
        match href {
            Some(href) => {
                let fx = self.read_click(byte);
                fx.and(self.read_open_link_new_window(&href))
            }
            None => self.read_click_activate(byte),
        }
    }

    /// `g` / `Alt-g`: the first / last block-grain element.
    fn read_ends(&mut self, last: bool) -> Effects {
        let target = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let mut blocks = read
                .elements
                .iter()
                .enumerate()
                .filter(|(_, e)| e.is_block());
            let el = if last {
                blocks.next_back()
            } else {
                blocks.next()
            };
            let Some((idx, _)) = el else {
                return Effects::none();
            };
            read.pos_of(crate::markdown::block_rest_byte(&read.elements, idx))
        };
        self.move_motion(Motion::Goto { position: target }, false)
    }

    /// `Enter`: follow the focused element — open a link (external → system handler, `#anchor` →
    /// the heading, relative path → open in Aether), open an image externally, or jump to a
    /// footnote's definition. No-op on non-interactive blocks. `Ctrl-a`/`Ctrl-Alt-a`: adjust the
    /// value under the cursor, up or down.
    ///
    /// Over a number that is increment/decrement. In a markdown buffer it is the task checkbox
    /// instead — up checks, down unchecks — so one pair of keys means the same thing in the
    /// editor and the reading view, which is the whole point of putting it here rather than on a
    /// read-only chord. Markdown gives up number adjustment for it: an ordered list's markers are
    /// positions a renderer assigns, not values worth nudging by hand.
    fn adjust_value(&mut self, up: bool, count: u32) -> Effects {
        let buffer_id = self.view.buffer.buffer_id;
        if self.view.buffer.language.as_deref() == Some("markdown") {
            return self.request_str::<InputToggleTask>(
                ToggleTaskParams {
                    buffer_id,
                    set: Some(up),
                },
                Event::BlockEditDone,
            );
        }
        let count = count as i32;
        self.edit::<InputAdjustNumber>(InputAdjustNumberParams {
            buffer_id,
            delta: if up { count } else { -count },
            // Insert mode has no selection: scan for the number at the caret rather than acting on
            // the (nonexistent) selection, and collapse afterwards.
            scan_at_cursor: self.view.mode == Mode::Insert,
        })
    }

    fn read_activate(&mut self) -> Effects {
        let idx = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor;
            // A link is followable only while it is *shown* as armed. With the selection extended
            // the shells suppress the target pill (`display_target`), so resolving innermost-any
            // here would follow a link nothing on screen marks — `Alt-x` up a paragraph that
            // opens with one, then Enter, and the view navigates away. With a selection up, Enter
            // is the block-grain action only.
            let armed = cursor
                .is_point()
                .then(|| read.focus(cursor.position))
                .flatten()
                .filter(|i| read.elements[*i].is_interactive());
            // Then the checkbox, resolved *outward* — see `ReadView::task_item`.
            let Some(idx) = armed.or_else(|| read.task_item(cursor.position)) else {
                return Effects::none();
            };
            idx
        };
        self.read_activate_element(idx)
    }

    /// Follow element `idx` — the shared body of `Enter` and a pointer click on a link or
    /// footnote reference.
    fn read_activate_element(&mut self, idx: usize) -> Effects {
        enum Act {
            Link(String),
            Image(String),
            Footnote(LogicalPosition),
            ToggleTask,
        }
        let act = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Stop::Link { href, .. } => Act::Link(href.clone()),
                crate::markdown::Stop::Image { src, .. } => Act::Image(src.clone()),
                crate::markdown::Stop::FootnoteRef { label, .. } => {
                    let Some(span) = crate::markdown::footnote_def_span(&read.blocks, label) else {
                        return Effects::toast_grouped(
                            format!("No definition for footnote [{label}]"),
                            ToastKind::Warning,
                            "read-view",
                        );
                    };
                    Act::Footnote(read.pos_of(span.start))
                }
                // A task item's activation IS toggling its checkbox — the armed-link pill keeps
                // precedence via `focus`'s innermost-any resolution, and clicks never route here
                // (`read_click_activate` filters to links/footnote refs).
                crate::markdown::Stop::Item {
                    checked: Some(_), ..
                } => Act::ToggleTask,
                _ => return Effects::none(),
            }
        };
        match act {
            // Enter *flips* — it acts on the box in front of you without your having to know
            // which way it is pointing. `Ctrl-a`/`Ctrl-Alt-a` are the directional pair.
            Act::ToggleTask => self.request_str::<InputToggleTask>(
                ToggleTaskParams {
                    buffer_id: self.view.buffer.buffer_id,
                    set: None,
                },
                Event::BlockEditDone,
            ),
            Act::Link(href) => self.read_follow_link(&href),
            Act::Image(src) => {
                // A remote image opens as the URL itself — resolving it against the buffer's
                // directory would fabricate a path like `/docs/https:/…`.
                let lower = src.to_ascii_lowercase();
                if lower.starts_with("http://") || lower.starts_with("https://") {
                    return Effects::one(Effect::ShellAction(ShellAction::OpenUrl(src)));
                }
                // Protocol-relative: a URL, like the link branch — default to https.
                if let Some(rest) = src.strip_prefix("//") {
                    return Effects::one(Effect::ShellAction(ShellAction::OpenUrl(format!(
                        "https://{rest}"
                    ))));
                }
                if has_url_scheme(&src) {
                    return Effects::toast_grouped(
                        format!("Can't open image source {src}"),
                        ToastKind::Warning,
                        "read-view",
                    );
                }
                // A leading `/` resolves workspace-root-relative like any link target
                // (`read_resolve_path`) and rides the asset route on the web. When it stays
                // filesystem-absolute (buffer outside every root), it can't ride the route —
                // the natives still open it, the web no-ops like its placeholder rendering.
                if src.starts_with('/') {
                    return match self.read_resolve_path(&src) {
                        Some(abs) if abs != src => {
                            Effects::one(Effect::ShellAction(ShellAction::OpenBufferFile {
                                absolute: abs,
                                buffer_id: self.view.buffer.buffer_id,
                                relative: src,
                            }))
                        }
                        _ => Effects::one(Effect::ShellAction(ShellAction::OpenUrl(src))),
                    };
                }
                match self.read_resolve_path(&src) {
                    Some(absolute) => {
                        Effects::one(Effect::ShellAction(ShellAction::OpenBufferFile {
                            absolute,
                            buffer_id: self.view.buffer.buffer_id,
                            relative: src,
                        }))
                    }
                    None => Effects::toast_grouped(
                        "Can't resolve the image path",
                        ToastKind::Warning,
                        "read-view",
                    ),
                }
            }
            Act::Footnote(pos) => self.read_jump_recorded(pos),
        }
    }

    /// Land an in-document *jump* — an anchor or footnote follow — as a nav-recorded move:
    /// re-open the current buffer with `record_nav_from` + `jump_to`, the same `view/open`
    /// composite cross-file follows and goto-definition ride, so `Backspace` returns. The
    /// same-buffer open is "a move, not a switch": nothing is discarded client- or
    /// server-side (see `adopt_navigation` and the handler's already-open branch), and jumps
    /// deliberately don't feed the motion history — `z` stays the undo for *motions*
    /// (j/k/o/g/v/search), `Backspace` the way back from *jumps*, in both modes. A pathless
    /// (scratch) buffer can't re-open itself: plain Goto, unrecorded.
    fn read_jump_recorded(&mut self, target: LogicalPosition) -> Effects {
        match self.view.buffer.path.clone() {
            Some(path) => self.open_path_at(path, Some(target), None),
            None => self.move_motion(Motion::Goto { position: target }, false),
        }
    }

    /// `Ctrl-Enter`: the picker's open-in-new-window at reading grain — a *relative-path*
    /// link opens in a new window (GUI) / app tab (web) via [`ShellAction::NewWindow`];
    /// everything else (external links, anchors, images, plain blocks) behaves like `Enter`.
    fn read_activate_new_window(&mut self) -> Effects {
        let href = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.view.buffer.cursor.position) else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Stop::Link { href, .. }
                    if !href.starts_with('#') && !has_url_scheme(href) =>
                {
                    href.clone()
                }
                _ => return self.read_activate(),
            }
        };
        self.read_open_link_new_window(&href)
    }

    /// Open a relative-path link target in a new window/tab (the tail of `Ctrl-Enter` and
    /// Ctrl-click). Callers have already filtered anchors and schemed URLs out.
    fn read_open_link_new_window(&mut self, href: &str) -> Effects {
        let path_part = href.split('#').next().unwrap_or(href);
        let Some(path) = self.read_resolve_path(path_part) else {
            return Effects::toast_grouped(
                "Can't resolve the link target",
                ToastKind::Warning,
                "read-view",
            );
        };
        let workspace = (!aether_protocol::is_ephemeral_workspace_id(&self.workspace))
            .then(|| self.workspace.clone());
        Effects::one(Effect::ShellAction(ShellAction::NewWindow(WindowTarget {
            workspace,
            // A relative link resolves against the tree you are reading in, so the window that
            // opens it belongs in the same context.
            worktrees: self.window_worktrees(),
            open: WindowOpen::Path { path, at: None },
        })))
    }

    /// Follow a link target from the reading view.
    fn read_follow_link(&mut self, href: &str) -> Effects {
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("mailto:")
        {
            return Effects::one(Effect::ShellAction(ShellAction::OpenUrl(href.to_string())));
        }
        if let Some(slug) = href.strip_prefix('#') {
            // In-document anchor: focus the heading (GitHub slug rules).
            let target = {
                let Some(read) = self.view.read.as_ref() else {
                    return Effects::none();
                };
                let Some(idx) = crate::markdown::heading_by_slug(&read.elements, slug) else {
                    return Effects::toast_grouped(
                        format!("No heading matches #{slug}"),
                        ToastKind::Warning,
                        "read-view",
                    );
                };
                read.pos_of(read.elements[idx].span().start)
            };
            return self.read_jump_recorded(target);
        }
        // Protocol-relative (`//cdn.example.com/x`): a URL, not a path — resolve it the way a
        // browser would, defaulting to https (GitHub's reading). Without this it would fall
        // through to the root-relative path branch and resolve to garbage.
        if let Some(rest) = href.strip_prefix("//") {
            return Effects::one(Effect::ShellAction(ShellAction::OpenUrl(format!(
                "https://{rest}"
            ))));
        }
        // An unhandled scheme (`ftp:`, `tel:`, …): say so rather than treating it as a relative
        // path and opening a bogus buffer named after the URL.
        if has_url_scheme(href) {
            return Effects::toast_grouped(
                format!("Can't open {href}"),
                ToastKind::Warning,
                "read-view",
            );
        }
        // A path, possibly with a `#fragment`: the file opens now; the fragment becomes the
        // pending anchor, landed by [`Self::consume_read_anchor`] once the target document is
        // parsed (heading slugs don't exist before then). File-shaped, so a markdown target
        // opens as a reading view: a doc tree browses like a wiki, and `Alt-Left`/Backspace
        // walks back (nav-recorded like any preview open).
        let (path_part, fragment) = match href.split_once('#') {
            Some((path, frag)) if !frag.is_empty() => (path, Some(frag.to_string())),
            Some((path, _)) => (path, None),
            None => (href, None),
        };
        match self.read_resolve_path(path_part) {
            Some(path) => {
                // An anchor asks for the reader outright: only a rendered document can land a
                // heading slug, whatever the file was last shown as. Set *after* the open —
                // `open_path_as` clears any stale anchor at entry.
                let kind = fragment
                    .is_some()
                    .then_some(aether_protocol::ui::ViewKind::Reader);
                let fx = self.open_path_as(path, None, None, kind);
                self.pending_read_anchor = fragment;
                fx
            }
            None => Effects::toast_grouped(
                "Can't resolve the link target",
                ToastKind::Warning,
                "read-view",
            ),
        }
    }

    /// The deferred half of a *cross-file* anchor at content adoption: resolve the slug against the
    /// freshly-staged parse, send the Goto, and hold the parse back — the visible view stays
    /// "Loading…" for the one `cursor/move` round-trip, and [`Self::install_staged_read`] swaps it
    /// in when the cursor lands, so the document paints exactly once, already in place (the
    /// editor's paint-once property for cross-file goto-def). A slug with no match installs
    /// immediately with the in-document branch's toast: nothing to place, and the hold must never
    /// outlive its reason.
    fn stage_read_place(&mut self, staged: ReadView) -> Effects {
        let Some(slug) = self.pending_read_anchor.take() else {
            return Effects::none();
        };
        let Some(read) = self.view.read.as_mut() else {
            return Effects::none();
        };
        match crate::markdown::heading_by_slug(&staged.elements, &slug) {
            Some(idx) => {
                let target = staged.pos_of(staged.elements[idx].span().start);
                read.staged = Some(Box::new(staged));
                self.move_motion(Motion::Goto { position: target }, false)
            }
            None => {
                *read = staged;
                self.read_fence_requests().and(Effects::toast_grouped(
                    format!("No heading matches #{slug}"),
                    ToastKind::Warning,
                    "read-view",
                ))
            }
        }
    }

    /// Install a staged reading-view parse once its anchor's cursor has landed — or failed:
    /// the hold must never wedge the view in "Loading…". No-op when nothing is staged.
    fn install_staged_read(&mut self) -> Effects {
        let Some(read) = self.view.read.as_mut() else {
            return Effects::none();
        };
        match read.staged.take() {
            Some(staged) => {
                *read = *staged;
                self.read_fence_requests()
            }
            None => Effects::none(),
        }
    }

    /// Land a pending cross-file anchor whose target is the document already on screen
    /// (`adopt_navigation`'s same-buffer branch): the live parse has the slugs, so this
    /// resolves immediately — no staging, the document is already painted. The in-document
    /// `#anchor` branch, one open later.
    fn consume_read_anchor(&mut self) -> Effects {
        let Some(slug) = self.pending_read_anchor.take() else {
            return Effects::none();
        };
        let target = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = crate::markdown::heading_by_slug(&read.elements, &slug) else {
                return Effects::toast_grouped(
                    format!("No heading matches #{slug}"),
                    ToastKind::Warning,
                    "read-view",
                );
            };
            read.pos_of(read.elements[idx].span().start)
        };
        self.move_motion(Motion::Goto { position: target }, false)
    }

    /// Resolve a (possibly relative) link/image target against the buffer. A leading `/` is
    /// **workspace-root-relative** (GitHub semantics): it joins the root containing the buffer
    /// (longest match, like every root computation). A buffer outside every root has no anchor, so
    /// such a target keeps its filesystem-absolute reading — the only sensible meaning there.
    /// Relative targets join the buffer's directory; `None` for a scratch buffer with a relative
    /// target. Callers scheme-check first — a URL joined onto either base is never meaningful.
    /// `pub`: the iced shell resolves image sources through this, so links and images can't drift.
    pub fn read_resolve_path(&self, target: &str) -> Option<String> {
        if target.starts_with('/') {
            let root = self
                .view
                .buffer
                .path
                .as_deref()
                .and_then(|p| strip_longest_root(p, &self.workspace_paths))
                .map(|(idx, _)| self.workspace_paths[idx as usize].as_str());
            return Some(match root {
                // `trim_start_matches`, not `[1..]`: a `//host` slipping through must not
                // re-absolutize the join (`Path::join` with a leading `/` replaces the base).
                Some(root) => std::path::Path::new(root)
                    .join(target.trim_start_matches('/'))
                    .to_string_lossy()
                    .into_owned(),
                None => target.to_string(),
            });
        }
        let parent = std::path::Path::new(self.view.buffer.path.as_deref()?).parent()?;
        Some(parent.join(target).to_string_lossy().into_owned())
    }

    /// `Ctrl-c`: copy — an extended selection's source (whole blocks, separators included),
    /// else the focused element: a link's URL, otherwise its markdown source.
    fn read_copy(&mut self) -> Effects {
        let (text, what) = {
            let Some(read) = self.view.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.view.buffer.cursor;
            if !cursor.is_point() {
                // Inclusive selection: the end cursor's char (the newline, in whole-line
                // normal form) is part of the range.
                let (a, b) = (read.byte_of(cursor.anchor), read.byte_of(cursor.position));
                let (start, end) = (a.min(b) as usize, a.max(b) as usize);
                let end = end
                    + read.text[end..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                (
                    read.text.get(start..end).unwrap_or("").to_string(),
                    "selection",
                )
            } else {
                let Some(idx) = read.focus(cursor.position) else {
                    return Effects::none();
                };
                match &read.elements[idx] {
                    crate::markdown::Stop::Link { href, .. } => (href.clone(), "link URL"),
                    el => (
                        read.slice(el.span()).trim_end().to_string(),
                        "element source",
                    ),
                }
            }
        };
        if text.is_empty() {
            return Effects::none();
        }
        let mut fx =
            Effects::toast_grouped(format!("Copied {what}"), ToastKind::Success, "read-copy");
        fx.push(Effect::WriteClipboard(text));
        fx
    }

    fn move_motion(&mut self, motion: Motion, extend: bool) -> Effects {
        self.request_str::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.view.buffer.buffer_id,
                motion,
                extend_selection: extend,
            },
            Event::CursorMsg,
        )
    }

    /// Handle a keystroke while a sneak (`s`/`S`) session is active: Esc cancels, Backspace unwinds
    /// the query, a key matching a live label jumps, and any other printable char refines the query.
    fn on_sneak_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        if code == KeyCode::Esc {
            return self.sneak_cancel();
        }
        if code == KeyCode::Backspace {
            let Some(sneak) = self.view.sneak.as_mut() else {
                return Effects::none();
            };
            sneak.query.pop();
            let query = sneak.query.clone();
            return self.sneak_update(query);
        }
        // Only plain printable input is query/label data; ignore chords and non-char keys (they
        // leave the session armed rather than trapping it — Esc is the explicit exit).
        if mods.ctrl || mods.alt {
            return Effects::none();
        }
        let Some(ch) = text
            .as_deref()
            .and_then(|t| t.chars().next())
            .filter(|c| !c.is_control())
        else {
            return Effects::none();
        };
        if self
            .view
            .sneak
            .as_ref()
            .is_some_and(|s| s.labels.contains(&ch))
        {
            return self.sneak_select(ch);
        }
        let Some(sneak) = self.view.sneak.as_mut() else {
            return Effects::none();
        };
        sneak.query.push(ch);
        let query = sneak.query.clone();
        self.sneak_update(query)
    }

    /// Push the current query to the server, which recomputes labels and refreshes the viewport.
    fn sneak_update(&mut self, query: String) -> Effects {
        let Some(viewport_id) = self.view.viewport_id else {
            return Effects::none();
        };
        let big = self.view.sneak.as_ref().is_some_and(|s| s.big);
        // Scope to what's actually on screen (reported by the shell). Fall back to the loaded
        // window's range until the shell has reported a scroll position.
        let (first_line, last_line) = self
            .view
            .visible_lines
            .or_else(|| {
                // The focused element's loaded **buffer** lines. The window's own range is in view
                // coordinates, which name no file's lines once a view spans several.
                self.view.window.as_ref().and_then(|w| {
                    crate::grid::loaded_line_range(w, self.view.focused_element)
                        .map(|(first, last)| (first, last.saturating_add(1)))
                })
            })
            .unwrap_or((0, 0));
        self.request_str::<SneakUpdate>(
            SneakUpdateParams {
                buffer_id: self.view.buffer.buffer_id,
                viewport_id,
                query,
                first_line,
                last_line,
                big,
            },
            Event::SneakUpdated,
        )
    }

    /// Jump to the labelled word (the server selects it / extends to the hull). Ends the session
    /// locally now; the cursor arrives via [`Event::CursorMsg`].
    fn sneak_select(&mut self, label: char) -> Effects {
        let extend = self.view.sneak.as_ref().is_some_and(|s| s.extend);
        self.view.sneak = None;
        self.request_str::<SneakSelect>(
            SneakSelectParams {
                buffer_id: self.view.buffer.buffer_id,
                label,
                extend,
            },
            Event::CursorMsg,
        )
    }

    /// Abandon the session (Esc). The cursor never moved; just clear the labels server-side.
    fn sneak_cancel(&mut self) -> Effects {
        self.cancel_sneak_on(self.view.buffer.buffer_id)
    }

    /// Like [`move_motion`](Self::move_motion) but reveals the landing as a jump (go-to-line) —
    /// a targeted destination, so the cursor rests a quarter down instead of minimal-scrolling.
    fn move_jump(&mut self, motion: Motion, extend: bool) -> Effects {
        self.request_str::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.view.buffer.buffer_id,
                motion,
                extend_selection: extend,
            },
            Event::CursorJump,
        )
    }

    /// A counted edit (`3J`, `3>`, …) — the repeat loop lives server-side.
    fn repeat_edit<M>(&mut self, count: u32) -> Effects
    where
        M: RpcMethod<Params = CountedEditParams, Result = EditResult> + 'static,
    {
        self.edit::<M>(CountedEditParams {
            buffer_id: self.view.buffer.buffer_id,
            count,
        })
    }

    /// Counted tree expand/contract — repeats server-side, stopping when the cursor stops
    /// changing.
    fn tree_select(&mut self, direction: TreeSelectDirection, count: u32) -> Effects {
        self.request_str::<CursorTreeSelect>(
            CursorTreeSelectParams {
                buffer_id: self.view.buffer.buffer_id,
                direction,
                count,
            },
            Event::CursorMsg,
        )
    }

    /// `z`/`Alt-z` — step the motion history; the count loop lives server-side, stopping
    /// once the history is exhausted (the cursor comes back unchanged then).
    fn motion_history<M>(&mut self, count: u32) -> Effects
    where
        M: RpcMethod<Params = CursorUndoParams, Result = CursorUndoResult> + 'static,
    {
        self.request_str::<M>(
            CursorUndoParams {
                buffer_id: self.view.buffer.buffer_id,
                count,
            },
            |r| Event::CursorMsg(r.map(|r| r.cursor)),
        )
    }

    /// Counted undo/redo — repeats server-side, stopping when the stack is exhausted.
    fn undo_redo<M>(&mut self, count: u32) -> Effects
    where
        M: RpcMethod<Params = UndoRedoParams, Result = UndoResult> + 'static,
    {
        self.request_str::<M>(
            UndoRedoParams {
                buffer_id: self.view.buffer.buffer_id,
                count,
                // Insert mode forbids selections — drop the one undo would otherwise restore.
                collapse_selection: self.view.mode == Mode::Insert,
            },
            Event::UndoRedoDone,
        )
    }

    /// `i`/`a`/`Alt-i`/`Alt-a` — collapse to the chosen selection edge. One RPC: the server owns
    /// the selection, so it resolves the edge (`Motion::SelectionEdge` — formerly a
    /// set-cursor-then-adjust chain).
    fn enter_insert_at(&mut self, where_: InsertWhere) -> Effects {
        let edge = match where_ {
            InsertWhere::SelectionStart => SelectionEdge::Start,
            InsertWhere::SelectionEnd => SelectionEdge::AfterEnd,
            InsertWhere::FirstLineStart => SelectionEdge::FirstLineNonblank,
            InsertWhere::LastLineEnd => SelectionEdge::LastLineEnd,
        };
        self.request_str::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.view.buffer.buffer_id,
                motion: Motion::SelectionEdge { edge },
                extend_selection: false,
            },
            Event::CursorMsg,
        )
    }

    fn copy(&mut self, scope: CopyScope) -> Effects {
        self.request_str::<BufferCopy>(
            BufferCopyParams {
                buffer_id: self.view.buffer.buffer_id,
                scope,
            },
            Event::CopyDone,
        )
    }

    fn cut(&mut self, scope: CopyScope) -> Effects {
        self.request_str::<BufferCut>(
            BufferCopyParams {
                buffer_id: self.view.buffer.buffer_id,
                scope,
            },
            Event::CutDone,
        )
    }
}

/// Translate the Explorer's filter set for a Grep/Files switch. The dir scope is the browsed
/// directory; changed-only copies as-is. For Grep the ignored/hidden visibility *inverts*:
/// the explorer's listing shows ignored/hidden entries unless hidden (`hide_*`), grep's walk
/// excludes them unless included (`include_*`) — flipping the polarity means the search sees
/// exactly what the listing showed. Files takes only dir + changed-only.
fn seeded_filters_for_switch(
    explorer: &PickerFilters,
    dir_scope: Option<ScopedPath>,
    target: PickerKind,
) -> PickerFilters {
    let mut seeded = PickerFilters::default();
    if let Some(d) = dir_scope {
        seeded.directories.push(d);
    }
    seeded.changed_only = explorer.changed_only;
    if target == PickerKind::Grep {
        seeded.include_ignored = !explorer.hide_ignored;
        seeded.include_hidden = !explorer.hide_hidden;
    }
    seeded
}

/// Ask the shell for the system clipboard; the text comes back as `ClipboardRead`.
fn read_clipboard_fx(kind: PasteKind) -> Effects {
    Effects::one(Effect::ReadClipboard(kind))
}

/// True when a markdown link/image target starts with a URL scheme (RFC 3986: a letter, then
/// letters/digits/`+`/`.`/`-`, then `:`) — anything schemed is *not* a buffer-relative path.
fn has_url_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') => {}
            _ => return false,
        }
    }
    false
}

/// The one-line toast for a working-changes view with nothing in it.
///
/// One line, not a title and a detail: there is exactly one fact here, and splitting it across two
/// lines had the second saying the first again in different words.
///
/// The baseline is named whenever one is pinned, because "nothing to commit" is only self-evident
/// against the default. Under a revision the tree may be thick with uncommitted work and still have
/// nothing *since that commit*; under the saved-file baseline the view is empty by construction —
/// it compares each file to its own content on disk — and reporting a clean tree would be wrong
/// about a dirty one. Same vocabulary as the `git/set_baseline` confirmation toast, so the two
/// don't describe the same setting differently.
fn nothing_to_commit(baseline: Option<&aether_protocol::git::GitBaselineSource>) -> String {
    match baseline {
        None => "Nothing to commit".to_string(),
        Some(aether_protocol::git::GitBaselineSource::Saved) => {
            "Nothing to commit (versus the files on disk)".to_string()
        }
        Some(aether_protocol::git::GitBaselineSource::Rev { label, .. }) => {
            format!("Nothing to commit (versus {label})")
        }
    }
}

/// What a completed fetch actually told us, phrased as the status bar would read it.
///
/// The three cases are genuinely different answers and the wording keeps them apart: no upstream
/// at all (nothing to be ahead or behind *of*), level with it, and diverged. Naming the upstream
/// matters in the last case — in a fork workflow "5 behind" is a very different sentence about
/// `origin/main` than about `upstream/main`.
fn fetch_summary(upstream: Option<&GitUpstreamStatus>) -> (String, String) {
    let Some(up) = upstream else {
        return ("Fetched".to_string(), String::new());
    };
    if up.is_level() {
        return (
            "Fetched".to_string(),
            format!("Up to date with {}", up.name),
        );
    }
    let mut parts: Vec<String> = Vec::new();
    if up.ahead > 0 {
        parts.push(format!("{} ahead", up.ahead));
    }
    if up.behind > 0 {
        parts.push(format!("{} behind", up.behind));
    }
    (
        "Fetched".to_string(),
        format!("{} {}", parts.join(", "), up.name),
    )
}

/// What a successful push accomplished. Names the upstream, because that's the fact the user is
/// checking — that the commits went where they meant them to go.
///
/// A first push says so explicitly: it's the one that *created* the tracking relationship, which is
/// also the moment the status bar's arrows start working for that branch, so it's worth a different
/// sentence rather than a silent success.
fn push_summary(result: &GitPushResult) -> (String, String) {
    let target = result
        .upstream
        .as_ref()
        .map(|u| u.name.clone())
        .unwrap_or_else(|| "the remote".to_string());
    if result.set_upstream {
        (
            format!("Pushed to {target}"),
            format!("Now tracking {target}"),
        )
    } else {
        (format!("Pushed to {target}"), String::new())
    }
}

/// What a pull did to local history, and what it disturbed on the way.
///
/// The verb comes from the server's read of the commit graph, not from git's summary line, and the
/// three are worth keeping apart: a fast-forward left the user's commits alone, a merge added one,
/// and a rebase rewrote them. The buffer counts are checkout's sentence for the same reason — a
/// pull that quietly left three buffers showing pre-merge content is the surprise the
/// reconciliation pass exists to prevent.
fn pull_summary(result: &GitPullResult) -> (String, String) {
    let verb = match result.status {
        GitPullStatus::Merged => "Merged",
        GitPullStatus::Rebased => "Rebased onto",
        _ => "Fast-forwarded to",
    };
    let target = result
        .upstream
        .as_ref()
        .map(|u| u.name.clone())
        .unwrap_or_else(|| "the remote".to_string());
    let mut detail = String::new();
    let moved = result.refreshed.reloaded.len();
    if moved > 0 {
        detail.push_str(&format!("Reloaded {moved} file(s)"));
    }
    if !result.refreshed.missing.is_empty() {
        let gone = result.refreshed.missing.len();
        if detail.is_empty() {
            detail.push_str(&format!("{gone} file(s) now gone"));
        } else {
            detail.push_str(&format!(", {gone} now gone"));
        }
    }
    (format!("{verb} {target}"), detail)
}

/// Name a short list in a toast: every entry up to three, then a count. Long enough to be
/// actionable when a merge conflicts in one or two files, short enough not to fill the screen when
/// it conflicts in thirty.
fn name_a_few(paths: &[String]) -> String {
    match paths {
        [] => "the working tree".to_string(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// The toast to show when a cursor-relative LSP request (hover / goto-definition) couldn't run
/// because the server wasn't ready — `None` once a ready server has answered, so the caller falls
/// back to its own "nothing here" message ("No hover info" / "No definition found").
fn lsp_readiness_message(readiness: LspReadiness) -> Option<(&'static str, &'static str)> {
    match readiness {
        LspReadiness::Ready => None,
        // The body is the part that saves a trip: each of these has a different cause, and only
        // one of them is worth waiting out.
        LspReadiness::NoServer => Some((
            "No language server for this file",
            "Aether has no server configured for this language",
        )),
        LspReadiness::Starting => Some((
            "Language server still starting",
            "Try again in a moment — the servers picker shows its progress",
        )),
        LspReadiness::Unavailable => Some((
            "Language server unavailable",
            "It isn't installed, isn't on PATH, or it failed to start — the servers picker says which",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session reading `/proj/docs/a.md`, ready to follow links.
    fn reading_session() -> Session {
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        s.view.buffer.path = Some("/proj/docs/a.md".into());
        s.view.mode = Mode::Read;
        s.view.read = Some(ReadView::loading(s.view.buffer.buffer_id));
        s
    }

    /// The window the server sends for a markdown file presented as the reader: one element the
    /// client lays out, carrying every line of `text` unwrapped.
    fn prose_window(buffer: BufferId, text: &str) -> aether_protocol::viewport::Window {
        use aether_protocol::viewport::{LogicalLineRender, Segment, Window, WrappedRow};
        let lines: Vec<LogicalLineRender> = text
            .split('\n')
            .enumerate()
            .map(|(i, t)| LogicalLineRender {
                change: Default::default(),
                logical_line: i as u32,
                visual_rows: vec![WrappedRow {
                    byte_offset: 0,
                    continuation_indent: 0,
                    segments: vec![Segment {
                        text: t.into(),
                        highlights: vec![],
                    }],
                }],
                search_matches: vec![],
                baseline_above: vec![],
                diagnostics: vec![],
                sneak_targets: vec![],
            })
            .collect();
        Window {
            other_elements_dirty: false,
            max_line_width: 0,
            git_status: None,
            root: Element::Editor {
                element: 0,
                buffer,
                rows: lines.len() as u32,
                first_row: aether_protocol::coords::ElementRow::ZERO,
                laid_out_by: aether_protocol::ui::LayoutOwner::Client,
                role: aether_protocol::ui::ElementRole::Field,
                first_buffer_line: 0,
                lines,
            },
        }
    }

    /// A link anchor decides the open: `[x](./other.md#section)` asks for the reader outright,
    /// whatever the file was last shown as, because a heading slug only resolves against the
    /// rendered document — landing in the editor would silently drop it. A plain link asks for
    /// nothing and takes the server's answer.
    #[test]
    fn a_followed_anchor_asks_for_the_reader() {
        let mut s = reading_session();
        let fx = s.read_follow_link("./other.md#section-two");
        let open =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "view/open" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("the link target opens");
        assert_eq!(open["kind"], serde_json::json!("reader"));
        assert_eq!(s.pending_read_anchor.as_deref(), Some("section-two"));

        let mut s = reading_session();
        let fx = s.read_follow_link("./plain.md");
        let open =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "view/open" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("the link target opens");
        assert!(open.get("kind").is_none(), "no anchor, no opinion");

        // A switch to a non-markdown target drops a pending anchor: nothing could land it.
        let mut s = reading_session();
        let _ = s.read_follow_link("./other.md#section-two");
        s.view.buffer.language = Some("rust".into());
        s.sync_read_anchor_on_switch();
        assert_eq!(s.pending_read_anchor, None);
        assert!(s.view.read.is_none());
    }

    /// The reading view is a consequence of the window: an element the client lays out puts the
    /// session in Read over its lines; an ordinary editor takes it down again.
    #[test]
    fn the_window_decides_the_reading_view() {
        let mut s = reading_session();
        s.view.read = None;
        s.view.mode = Mode::Normal;
        let id = s.view.buffer.buffer_id;
        s.view.window = Some(prose_window(id, "# Title\n\nbody\n"));
        let fx = s.sync_read_presentation();
        assert_eq!(s.view.mode, Mode::Read);
        let read = s.view.read.as_ref().expect("reading view");
        assert!(!read.loading);
        assert_eq!(read.text, "# Title\n\nbody\n");
        assert_eq!(read.blocks.len(), 2);
        assert!(
            !fx.0.iter().any(|e| matches!(e, Effect::Request { .. })),
            "no fences, nothing to ask for"
        );

        // The same text again (a push about the cursor) is not a re-parse.
        let gen = s.view.read.as_ref().unwrap().hl_gen;
        let _ = s.sync_read_presentation();
        assert_eq!(s.view.read.as_ref().unwrap().hl_gen, gen);

        // Changed text re-parses in place.
        s.view.window = Some(prose_window(id, "# Title\n\nbody\n\nmore\n"));
        let _ = s.sync_read_presentation();
        let read = s.view.read.as_ref().unwrap();
        assert_eq!(read.blocks.len(), 3);
        assert_eq!(read.hl_gen, gen + 1);

        // A partial load is a window still on its way: the view waits rather than parsing half.
        let mut partial = prose_window(id, "# Title\n\nbody\n");
        if let Element::Editor { rows, .. } = &mut partial.root {
            *rows += 5;
        }
        s.view.read = None;
        s.view.window = Some(partial);
        let _ = s.sync_read_presentation();
        let read = s.view.read.as_ref().expect("entered, loading");
        assert!(read.loading && read.blocks.is_empty());

        // An editor window takes the reading view down.
        let mut editor = prose_window(id, "# Title\n");
        if let Element::Editor { laid_out_by, .. } = &mut editor.root {
            *laid_out_by = aether_protocol::ui::LayoutOwner::Server;
        }
        s.view.window = Some(editor);
        let _ = s.sync_read_presentation();
        assert!(s.view.read.is_none());
        assert_eq!(s.view.mode, Mode::Normal);
    }

    /// Cross-file anchors: following `[x](./other.md#section)` opens the file and arms the
    /// fragment; the anchor lands as a `cursor/move` Goto once the target document's window
    /// arrives and parses.
    #[test]
    fn cross_file_anchor_lands_after_target_adopts() {
        let mut s = reading_session();
        let fx = s.read_follow_link("./other.md#section-two");
        assert!(
            fx.0.iter()
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "view/open")),
            "the link target opens"
        );
        assert_eq!(s.pending_read_anchor.as_deref(), Some("section-two"));

        // The switch lands: the new buffer's window carries the document, laid out by us.
        s.view.buffer.buffer_id += 1;
        s.view.read = None;
        let id = s.view.buffer.buffer_id;
        s.view.window = Some(prose_window(
            id,
            "# One\n\ntext\n\n## Section Two\n\nbody\n",
        ));
        let fx = s.sync_read_presentation();
        assert_eq!(s.pending_read_anchor, None, "the anchor is consumed");
        // The parse is *staged*, not installed: the visible view stays "Loading…" for the
        // cursor round-trip, so the document paints exactly once, already in place.
        let read = s.view.read.as_ref().unwrap();
        assert!(
            read.loading && read.blocks.is_empty(),
            "held back while the Goto flies"
        );
        let staged = read
            .staged
            .as_deref()
            .expect("parse staged behind the anchor");
        let idx = crate::markdown::heading_by_slug(&staged.elements, "section-two").unwrap();
        let expected = staged.pos_of(staged.elements[idx].span().start);
        let goto =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "element/move" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("the anchor lands as a cursor move");
        assert_eq!(
            goto["motion"],
            serde_json::to_value(Motion::Goto { position: expected }).unwrap(),
            "…to the heading's position"
        );

        // The cursor reply installs the staged parse — the first paint is in place.
        s.on_event(Event::CursorMsg(Ok(CursorState {
            position: expected,
            anchor: expected,
            match_bracket: None,
            jumplist_position: None,
        })));
        let read = s.view.read.as_ref().unwrap();
        assert!(
            !read.loading && !read.blocks.is_empty(),
            "installed on landing"
        );
        assert!(read.staged.is_none());
        assert_eq!(s.view.buffer.cursor.position, expected);
    }

    /// A fragment naming no heading in the target document warns (same toast as the
    /// in-document branch) instead of moving the cursor.
    #[test]
    fn cross_file_anchor_missing_heading_warns() {
        let mut s = reading_session();
        s.read_follow_link("./other.md#nope");
        assert_eq!(s.pending_read_anchor.as_deref(), Some("nope"));
        s.view.buffer.buffer_id += 1;
        s.view.read = None;
        let id = s.view.buffer.buffer_id;
        s.view.window = Some(prose_window(id, "# Only Heading\n"));
        let fx = s.sync_read_presentation();
        assert_eq!(s.pending_read_anchor, None);
        assert!(
            fx.0.iter().any(|e| matches!(
                e,
                Effect::Toast {
                    kind: ToastKind::Warning,
                    ..
                }
            )),
            "a missing anchor warns"
        );
        assert!(
            !fx.0
                .iter()
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "element/move")),
            "…and moves nothing"
        );
        // Nothing to place, so no hold: the document installs immediately.
        let read = s.view.read.as_ref().unwrap();
        assert!(!read.loading && !read.blocks.is_empty() && read.staged.is_none());
    }

    /// In-document anchors are *jumps*, not motions: following `#section` re-opens the current
    /// buffer with `record_nav_from` + `jump_to` — the same composite cross-file follows and
    /// goto-definition ride — so `Backspace` returns.
    #[test]
    fn in_document_anchor_follow_is_nav_recorded() {
        let mut s = reading_session();
        s.view
            .read
            .as_mut()
            .unwrap()
            .adopt(0, "# One\n\ntext\n\n## Section Two\n\nbody\n".into());
        let fx = s.read_follow_link("#section-two");
        let open =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "view/open" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("an in-document anchor rides view/open");
        assert_eq!(
            open["record_nav_from"],
            serde_json::json!(s.view.buffer.buffer_id),
            "the origin is recorded"
        );
        let read = s.view.read.as_ref().unwrap();
        let idx = crate::markdown::heading_by_slug(&read.elements, "section-two").unwrap();
        let expected = read.pos_of(read.elements[idx].span().start);
        assert_eq!(
            open["jump_to"],
            serde_json::to_value(expected).unwrap(),
            "…and the jump lands on the heading"
        );

        // Missing slugs still resolve client-side: toast, no RPC, no stray nav entry.
        let fx = s.read_follow_link("#nope");
        assert!(fx.0.iter().all(|e| !matches!(e, Effect::Request { .. })));
        assert!(fx.0.iter().any(|e| matches!(e, Effect::Toast { .. })));
    }

    /// A leading `/` resolves workspace-root-relative (GitHub semantics, longest matching
    /// root); a buffer outside every root keeps the filesystem-absolute reading; `//host`
    /// targets are URLs, not paths.
    #[test]
    fn root_relative_targets_resolve_against_the_buffers_root() {
        let mut s = reading_session();
        assert_eq!(
            s.read_resolve_path("/other.md").as_deref(),
            Some("/proj/other.md")
        );
        assert_eq!(
            s.read_resolve_path("/a/b.png").as_deref(),
            Some("/proj/a/b.png")
        );
        // Buffer-dir joins are textual (the OS normalizes `./` at open) — unchanged.
        assert_eq!(
            s.read_resolve_path("./x.md").as_deref(),
            Some("/proj/docs/./x.md")
        );

        // Composes with cross-file anchors: the fragment splits off before resolution.
        let fx = s.read_follow_link("/other.md#section-two");
        assert!(
            fx.0.iter()
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "view/open")),
            "a root-relative link opens"
        );
        assert_eq!(s.pending_read_anchor.as_deref(), Some("section-two"));

        // Outside every root there is no anchor — the filesystem-absolute reading stands.
        s.view.buffer.path = Some("/elsewhere/notes.md".into());
        assert_eq!(
            s.read_resolve_path("/etc/hosts").as_deref(),
            Some("/etc/hosts")
        );

        // Protocol-relative is a URL: open it, https-defaulted, never root-join it.
        let fx = s.read_follow_link("//cdn.example.com/x.png");
        assert!(
            fx.0.iter().any(|e| matches!(
                e,
                Effect::ShellAction(ShellAction::OpenUrl(u)) if u == "https://cdn.example.com/x.png"
            )),
            "protocol-relative opens as a URL"
        );
    }

    /// A pending anchor is armed for exactly one open: any unrelated `open_path_at`
    /// disarms it, and a plain (fragment-less) follow arms nothing.
    #[test]
    fn unrelated_open_disarms_pending_anchor() {
        let mut s = reading_session();
        s.read_follow_link("./other.md#section");
        assert!(s.pending_read_anchor.is_some());
        s.open_path_at("/proj/src/main.rs".into(), None, None);
        assert_eq!(
            s.pending_read_anchor, None,
            "a fresh open disarms the anchor"
        );

        s.read_follow_link("./plain.md");
        assert_eq!(s.pending_read_anchor, None, "no fragment, nothing armed");
    }

    // Mirrors the TUI's seeded_filters_for_switch tests: the explorer's visibility filters
    // invert for Grep (its walk excludes what the listing shows), and Files takes only
    // dir + changed-only.
    #[test]
    fn explorer_switch_translates_filters() {
        let scope = ScopedPath {
            path_index: 0,
            relative_path: "src".into(),
            is_file: false,
        };
        let defaults = PickerFilters::default();
        let seeded = seeded_filters_for_switch(&defaults, Some(scope.clone()), PickerKind::Grep);
        assert!(seeded.include_ignored && seeded.include_hidden);
        assert_eq!(seeded.directories, vec![scope.clone()]);

        let hiding = PickerFilters {
            hide_ignored: true,
            changed_only: true,
            ..PickerFilters::default()
        };
        let seeded = seeded_filters_for_switch(&hiding, Some(scope.clone()), PickerKind::Grep);
        assert!(!seeded.include_ignored && seeded.include_hidden && seeded.changed_only);

        let seeded = seeded_filters_for_switch(&hiding, Some(scope), PickerKind::Files);
        assert!(!seeded.include_ignored && !seeded.include_hidden && seeded.changed_only);

        // Roots mode: no dir scope — the target covers the whole workspace.
        let seeded = seeded_filters_for_switch(&defaults, None, PickerKind::Grep);
        assert!(seeded.directories.is_empty());
    }

    /// `picker_item_target` — the "open in a new window" descriptor — supports the same item set as
    /// the web client's `pickerItemUrl`: files, grep hits (with location), file-backed and scratch
    /// buffers, explorer files, and workspaces; it declines everything else.
    fn target_of(kind: PickerKind, item: PickerItem, selected: u32) -> Option<WindowTarget> {
        use crate::picker::PickerState;
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        let mut p = PickerState::new(kind);
        if kind == PickerKind::Explorer {
            p.directory = Some("/proj/src".into());
        }
        p.items = vec![item];
        p.offset = 0;
        p.selected = selected;
        s.picker = Some(p);
        s.picker_item_target()
    }

    #[test]
    fn picker_item_target_resolves_files_and_grep_hits_to_absolute_paths() {
        assert_eq!(
            target_of(
                PickerKind::Files,
                PickerItem::File {
                    path_index: 0,
                    relative_path: "src/main.rs".into(),
                    match_indices: vec![],
                    git_status: None,
                },
                0,
            ),
            Some(WindowTarget {
                workspace: Some("proj".into()),
                worktrees: Vec::new(),
                open: WindowOpen::Path {
                    path: "/proj/src/main.rs".into(),
                    at: None,
                },
            })
        );
        // A grep hit carries its 0-based location so the new window jumps to the match.
        assert_eq!(
            target_of(
                PickerKind::Grep,
                PickerItem::GrepHit {
                    path_index: 0,
                    relative_path: "src/main.rs".into(),
                    line: 41,
                    col: 9,
                    preview: "let x = 1;".into(),
                    match_indices: vec![],
                },
                0,
            ),
            Some(WindowTarget {
                workspace: Some("proj".into()),
                worktrees: Vec::new(),
                open: WindowOpen::Path {
                    path: "/proj/src/main.rs".into(),
                    at: Some((41, 9)),
                },
            })
        );
    }

    #[test]
    fn picker_item_target_reopens_a_scratch_buffer_by_id() {
        assert_eq!(
            target_of(
                PickerKind::Views,
                PickerItem::View {
                    buffer_id: 7,
                    view_id: ViewId(7),
                    view_kind: None,
                    display: "(scratch 1)".into(),
                    status: aether_protocol::picker::BufferDirtyState::default(),
                    path_index: None,
                    relative_path: None,
                    match_indices: vec![],
                    transient: false,
                },
                0,
            ),
            Some(WindowTarget {
                workspace: Some("proj".into()),
                worktrees: Vec::new(),
                open: WindowOpen::View(ViewId(7)),
            })
        );
    }

    #[test]
    fn picker_item_target_opens_a_workspace_row_in_its_own_window() {
        assert_eq!(
            target_of(
                PickerKind::Workspaces,
                PickerItem::Workspace {
                    name: "other".into(),
                    unsaved: 0,
                    match_indices: vec![],
                },
                0,
            ),
            Some(WindowTarget {
                workspace: Some("other".into()),
                worktrees: Vec::new(),
                open: WindowOpen::Workspace,
            })
        );
    }

    #[test]
    fn picker_item_target_declines_directories() {
        // A directory navigates *within* the picker — it isn't a new-window target (nor is it on web).
        assert_eq!(
            target_of(
                PickerKind::Explorer,
                PickerItem::DirEntry {
                    name: "sub".into(),
                    is_dir: true,
                    match_indices: vec![],
                    git_status: None,
                },
                0,
            ),
            None
        );
    }

    /// A file handed to us by the desktop (macOS "Open With") opens like the `Space Alt-w` overlay
    /// commit: `workspace/open_path`, non-transient, existing-files-only — and it takes over from
    /// whatever overlay happened to be up, since the user's attention just moved to the new file.
    #[test]
    fn open_path_from_os_opens_a_real_buffer_and_drops_any_prompt() {
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        s.prompt = Some(Prompt::OpenPath(Box::new(
            crate::path_editor::PathEditor::absolute("half-typed".into(), true),
        )));
        // The boot chooser, which an "Open With" launch races: naming a file answers the question
        // the workspace picker is asking, so it must not be left sitting over the document.
        s.picker = Some(crate::picker::PickerState::new(PickerKind::Workspaces));

        let fx = s.open_path_from_os("/elsewhere/notes.md".into());
        let params =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "workspace/open_path" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("an OS-delivered file rides workspace/open_path");
        assert_eq!(params["path"], serde_json::json!("/elsewhere/notes.md"));
        // Not a preview: it must survive being hidden.
        assert_eq!(params["transient"], serde_json::Value::Null);
        // The OS only hands us files that exist; a create here would mint buffers for typos.
        assert_eq!(params["create_if_missing"], serde_json::json!(false));
        assert!(s.prompt.is_none(), "the overlay gives way to the new file");
        assert!(
            s.picker.is_none(),
            "the boot chooser gives way to the new file too"
        );
    }

    #[test]
    fn picker_click_new_window_selects_the_clicked_row_then_spawns_and_closes() {
        use crate::picker::PickerState;
        let file = |name: &str| PickerItem::File {
            path_index: 0,
            relative_path: name.into(),
            match_indices: vec![],
            git_status: None,
        };
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        let mut p = PickerState::new(PickerKind::Files);
        p.items = vec![file("a.rs"), file("b.rs")];
        p.offset = 0;
        p.selected = 0;
        s.picker = Some(p);

        // Ctrl-click the *second* row: the click moves the selection there, then spawns for it.
        let fx = s.picker_click_new_window(1);
        let target = fx.0.iter().find_map(|e| match e {
            Effect::ShellAction(ShellAction::NewWindow(t)) => Some(t.clone()),
            _ => None,
        });
        assert_eq!(
            target,
            Some(WindowTarget {
                workspace: Some("proj".into()),
                worktrees: Vec::new(),
                open: WindowOpen::Path {
                    path: "/proj/b.rs".into(),
                    at: None,
                },
            })
        );
        // Like a normal accept / Ctrl-Enter, the panel closes.
        assert!(s.picker.is_none());
    }

    #[test]
    fn picker_click_new_window_falls_through_for_non_targets() {
        use crate::picker::PickerState;
        // A directory row isn't a new-window target — a Ctrl-click on it behaves like a normal click
        // (navigate into the dir), never a window spawn.
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        let mut p = PickerState::new(PickerKind::Explorer);
        p.directory = Some("/proj/src".into());
        p.items = vec![PickerItem::DirEntry {
            name: "sub".into(),
            is_dir: true,
            match_indices: vec![],
            git_status: None,
        }];
        p.offset = 0;
        p.selected = 0;
        s.picker = Some(p);

        let fx = s.picker_click_new_window(0);
        assert!(
            !fx.0
                .iter()
                .any(|e| matches!(e, Effect::ShellAction(ShellAction::NewWindow(_)))),
            "a directory Ctrl-click must not spawn a window"
        );
    }

    /// A workspace session on `/proj/src/a.rs` for the copy-web-url tests.
    fn web_url_session() -> Session {
        let mut s = Session::placeholder();
        s.workspace = "proj".into();
        s.workspace_paths = vec!["/proj".into()];
        s.view.buffer.path = Some("/proj/src/a.rs".into());
        s
    }

    /// The `path_query` of the [`ShellAction::CopyWebUrl`] a session emits, if any.
    fn copied_web_url(fx: &Effects) -> Option<String> {
        fx.0.iter().find_map(|e| match e {
            Effect::ShellAction(ShellAction::CopyWebUrl { path_query }) => Some(path_query.clone()),
            _ => None,
        })
    }

    /// `Space Alt-z` on a file buffer: the root-relative `?workspace=&file=` link the web boot
    /// parses, with the cursor as its 1-based `#L:C` fragment, plus the confirmation toast.
    /// The base is deliberately absent — the shell prepends its own.
    #[test]
    fn copy_web_url_emits_a_file_link_with_cursor_fragment() {
        let mut s = web_url_session();
        s.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 41, col: 9 };
        let fx = s.copy_web_url();
        assert_eq!(
            copied_web_url(&fx).as_deref(),
            Some("?workspace=proj&file=src/a.rs#42:10")
        );
        assert!(
            fx.0.iter().any(
                |e| matches!(e, Effect::Toast { title, kind: ToastKind::Success, .. }
                if title == "Copied web URL")
            ),
            "a concise confirmation toast rides alongside"
        );
    }

    /// A scratch has no path but is web-addressable as a `?view=` link (the same form the web
    /// client's own picker share links use for scratches).
    #[test]
    fn copy_web_url_links_a_scratch_by_view_id() {
        let mut s = web_url_session();
        s.view.buffer.path = None;
        s.view.buffer.buffer_id = 7;
        s.view.view_id = ViewId(70);
        let fx = s.copy_web_url();
        assert_eq!(
            copied_web_url(&fx).as_deref(),
            Some("?workspace=proj&view=70")
        );
    }

    /// A file with no workspace to be relative to — outside every root, or in an ephemeral
    /// (no-workspace) context — is addressed by absolute path, the `?path=` link the web boot hands
    /// to `workspace/open_path`. It carries no `workspace`: naming one would be a lie about where
    /// the file lives, and a temporary context's id is recycled, so it can't be named at all.
    #[test]
    fn copy_web_url_addresses_unrooted_files_by_absolute_path() {
        // An external file, hosted as a guest by a named workspace.
        let mut s = web_url_session();
        s.view.buffer.path = Some("/elsewhere/b.rs".into());
        s.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 41, col: 9 };
        assert_eq!(
            copied_web_url(&s.copy_web_url()).as_deref(),
            Some("?path=/elsewhere/b.rs#42:10")
        );
        // A file in a temporary context, even one under the root that context adopted.
        let mut s = web_url_session();
        s.workspace = format!("{}1", aether_protocol::EPHEMERAL_WORKSPACE_PREFIX);
        s.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 41, col: 9 };
        assert_eq!(
            copied_web_url(&s.copy_web_url()).as_deref(),
            Some("?path=/proj/src/a.rs#42:10")
        );
    }

    /// The one target left with no address: a *pathless* buffer in a temporary context. A scratch
    /// is reachable only by an id scoped to the workspace holding it, and that workspace is not
    /// something a link can name — so this warns rather than copying a link that won't reopen.
    #[test]
    fn copy_web_url_warns_for_a_scratch_with_no_workspace() {
        let mut s = web_url_session();
        s.workspace = format!("{}1", aether_protocol::EPHEMERAL_WORKSPACE_PREFIX);
        s.view.buffer.path = None;
        let fx = s.copy_web_url();
        assert_eq!(copied_web_url(&fx), None, "nothing lands on the clipboard");
        assert!(
            fx.0.iter().any(|e| matches!(
                e,
                Effect::Toast {
                    kind: ToastKind::Warning,
                    ..
                }
            )),
            "the refusal is audible"
        );
    }
}

/// What feeding a key to a [`PathEditor`] means for its owner.
///
/// The editor itself is shared: the save-as prompt and the workspace-settings add-project row want
/// identical completion behaviour (root typeahead, ghost suggestions, `Tab` accept, fish-style
/// `Alt-Backspace`) but commit to entirely different places. So the key *mechanics* live here and
/// the two callers interpret [`Self::Commit`] / [`Self::Cancel`] their own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathEditorKey {
    /// Consumed by the editor. `refresh` means its directory listing went stale and the owner
    /// should refetch (`directory/list`).
    Handled { refresh: bool },
    /// Enter in the path field — the owner commits whatever the editor now holds.
    Commit,
    /// Esc — the owner closes or cancels.
    Cancel,
    /// `Tab` past the editor's last segment: the owner should move to whatever follows it.
    NextField,
    /// `Shift-Tab` before its first segment: the owner should move to whatever precedes it.
    PrevField,
    /// Not a key the editor acts on. The owner may handle it.
    Ignored,
}

/// Drive a [`PathEditor`] from one key. See [`PathEditorKey`]; in-field text entry (characters,
/// plain Backspace, caret movement) is owned by each shell's native input and synced separately, so
/// anything not listed here is a no-op.
pub(crate) fn path_editor_key(
    ed: &mut PathEditor,
    workspace_paths: &[String],
    code: KeyCode,
    mods: Mods,
    text: Option<String>,
) -> PathEditorKey {
    let labels = super::labels::root_labels(workspace_paths);
    // Asked of the editor, not of the root count: an absolute editor has no root segment however
    // many roots the workspace has, and the arms below move focus into one whenever this is true.
    let multi_root = ed.multi_root(workspace_paths);
    let in_root = multi_root && ed.field == ChipEditorField::Root;
    let no_chord = !mods.ctrl && !mods.alt;
    // Whether the path field's suggestion listing went stale and needs a directory/list.
    let mut refresh = false;
    match code {
        // Enter in the path field commits; in the root field it confirms the root and advances.
        KeyCode::Enter if no_chord && !in_root => return PathEditorKey::Commit,
        KeyCode::Enter if no_chord => {
            refresh = ed.commit_root_field(&labels, workspace_paths);
        }
        KeyCode::Esc => return PathEditorKey::Cancel,
        // Tab / Shift-Tab traverse — the editor's segments are fields like any other, so they step
        // root → path and back, and hand off past either end. Accepting a suggestion is Alt-l
        // (below), never Tab: one key, one meaning, everywhere.
        KeyCode::Tab if no_chord => {
            if in_root {
                // Traversal only — the root ghost is *not* adopted. Accepting a suggestion is
                // Alt-l, here as everywhere; Tab that quietly completed on its way past would be
                // the same overloading this scheme exists to remove.
                refresh = ed.advance_to_path(workspace_paths);
            } else {
                return PathEditorKey::NextField;
            }
        }
        KeyCode::BackTab => {
            if in_root {
                return PathEditorKey::PrevField;
            }
            if multi_root {
                ed.field = ChipEditorField::Root;
            } else {
                return PathEditorKey::PrevField;
            }
        }
        // Alt-l accepts the focused segment's suggestion (root — adopt + advance; path — absorb the
        // next directory segment, repeated presses walk down the tree).
        KeyCode::Char('l') if mods.alt && !mods.ctrl => {
            if in_root {
                refresh = ed.commit_root_field(&labels, workspace_paths);
            } else {
                refresh = ed.accept_path_suggestion(workspace_paths);
            }
        }
        KeyCode::Char('h') if mods.alt && !mods.ctrl && multi_root => {
            ed.field = ChipEditorField::Root;
        }
        // `:` on a completed root value confirms it and moves into the path.
        KeyCode::Char(':') if no_chord && in_root => {
            if ed.root_complete(&labels) {
                refresh = ed.commit_root_field(&labels, workspace_paths);
            }
        }
        // Alt-Backspace: in the path it deletes the rightmost segment, fish-style; at an empty
        // path it clears the root selection. In the root field it clears the filter.
        KeyCode::Backspace if mods.alt && !mods.ctrl => {
            if ed.field == ChipEditorField::Path {
                if ed.input.text.is_empty() {
                    if multi_root {
                        ed.field = ChipEditorField::Root;
                        ed.root_filter.clear();
                        ed.root_selected = 0;
                    }
                } else {
                    refresh = ed.pop_path_segment(workspace_paths);
                }
            } else {
                ed.root_filter.clear();
                ed.root_selected = 0;
            }
        }
        // Backspace at an empty path steps back into the root field.
        KeyCode::Backspace
            if no_chord
                && multi_root
                && ed.field == ChipEditorField::Path
                && ed.input.text.is_empty() =>
        {
            ed.field = ChipEditorField::Root;
        }
        // Cycle the focused segment's matches: root typeahead (wrapping) or path suggestions
        // (clamped).
        KeyCode::Char(c @ ('j' | 'k')) if mods.alt && !mods.ctrl => {
            let down = c == 'j';
            if in_root {
                let n = chips::root_candidates(&labels, &ed.root_filter.text).len();
                if n > 0 {
                    let sel = ed.root_selected.min(n - 1);
                    ed.root_selected = if down {
                        (sel + 1) % n
                    } else {
                        (sel + n - 1) % n
                    };
                    refresh = ed.sync_dir_listing(workspace_paths);
                }
            } else {
                ed.cycle_path_suggestion(down);
            }
        }
        _ => {
            let _ = text;
            return PathEditorKey::Ignored;
        }
    }
    PathEditorKey::Handled { refresh }
}
