//! The core update function, grown arm by arm (docs/client-core.md phase 3): each migrated
//! subsystem moves its `Message` variants into [`Event`], its handler logic into
//! [`Session::on_event`], and its RPC chains into effect-returning methods here. The shell
//! bridges with a single `Message::Core(Event)` variant and an effect executor.

use super::chips::{self, ChipEditor, ChipEditorField, ChipId};
use super::effect::{
    Effect, Effects, RevealStyle, ShellAction, ToastKind, WindowOpen, WindowTarget,
};
use super::hints::{
    ContextId as HintCtx, HintFacts, HintView, PickerCmd, WireEvent as HintWireEvent,
};
use super::keymap::{lookup, Action, InsertWhere, KeyCode, KeyContext, Mods};
use super::path_editor::PathEditor;
use super::picker::{item_key, PickerState, Reveal, FETCH_LIMIT, VISIBLE_ROWS};
use super::session::{
    buffer_info, min_pos, severity_label, step_font_size, strip_longest_root, AfterSave,
    AppSettingId, AppSettingsOverlay, CommitDetails, ConfirmAction, ConfirmKind, ConnState,
    HoverBlock, HoverText, Mode, PasteKind, Pending, Prompt, ReadView, ReloadTry, RepeatTarget,
    SaveTry, SearchSnapshot, SearchState, Session, SettingsRow, SneakState, TextField,
    WorkspaceSettings,
};
use super::transport::RpcError;
use aether_protocol::app::{AppInfoGet, AppInfoParams};
use aether_protocol::buffer::{
    BufferChanged, BufferChangedParams, BufferClose, BufferCloseParams, BufferClosed,
    BufferClosedParams, BufferContent, BufferContentParams, BufferContentResult, BufferCopy,
    BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen, BufferOpenParams,
    BufferOpenResult, BufferReload, BufferReloadParams, BufferSave, BufferSaveParams,
    BufferSetTransient, BufferSetTransientParams, BufferState, BufferStateParams, CopyScope,
};
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
    ApplyHunkStatus, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult, GitBlameLine,
    GitBlameLineParams, GitNavigateHunk, GitNavigateHunkParams, GitNavigateHunkResult,
    GitSetDiffView, GitSetDiffViewParams, HunkAction, HunkDirection,
};
use aether_protocol::hints::{
    HintsRecord, HintsRecordParams, HintsState, HintsStateParams, HintsStateResult,
};
use aether_protocol::history::{
    HistoryEntry, HistoryKind, HistoryRecord, HistoryRecordParams, HistoryState,
    HistoryStateParams, HistoryStateResult,
};
use aether_protocol::input::{
    BufferOnlyParams, CaseKind, CountedEditParams, EditRedo, EditResult, EditUndo,
    InputAdjustNumber, InputAdjustNumberParams, InputBackspace, InputChange, InputChangeLine,
    InputDedent, InputDelete, InputDeleteLine, InputIndent, InputJoinLines, InputMoveLines,
    InputMoveLinesParams, InputNewlineAndIndent, InputNewlineAndIndentParams, InputOpenLine,
    InputOpenLineParams, InputReplaceLine, InputReplaceLineParams, InputSurround,
    InputSurroundParams, InputTab, InputText, InputTextParams, InputToggleComment,
    InputTransformCase, InputTransformCaseParams, InputUnsurround, InputUnsurroundParams, LineSide,
    ToggleCommentParams, UndoRedoParams, UndoResult,
};
use aether_protocol::jumplist::{
    JumplistCapture, JumplistCaptureParams, JumplistCaptureResult, JumplistStep,
    JumplistStepParams, JumplistStepResult, JumplistStepScope,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspDocumentHighlight, LspDocumentHighlightParams, LspFormat,
    LspFormatResult, LspGotoDefinition, LspGotoDefinitionResult, LspHover, LspHoverResult,
    LspNavigateDiagnostic, LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspReadiness,
    LspRestartServer, LspRestartServerParams, LspServerStatus, LspStatusChanged,
};
use aether_protocol::nav::NavStepResult;
use aether_protocol::nav::{NavStep, NavStepParams};
use aether_protocol::path::{PathDelete, PathDeleteParams, PathDeleteResult};
use aether_protocol::picker::{
    BufferDirtyState, CaseMode, MatchOptions, PickerFilters, PickerHide, PickerHideParams,
    PickerItem, PickerKind, PickerQuery, PickerQueryParams, PickerReset, PickerSectionJump,
    PickerSectionJumpParams, PickerSelect, PickerSelectParams, PickerSelectResult, PickerUpdate,
    PickerUpdateParams, PickerView, PickerViewParams, PickerViewResult, ScopedPath,
    MIN_GREP_QUERY_LEN,
};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavResult, SearchSet, SearchSetParams, SearchSetResult,
    SearchStateChanged, SearchStep, SearchStepParams, SearchSummary,
};
use aether_protocol::settings::{
    AppSettings, SettingsChanged, SettingsGet, SettingsGetParams, SettingsSet,
};
use aether_protocol::sneak::{
    SneakCancel, SneakCancelParams, SneakSelect, SneakSelectParams, SneakUpdate, SneakUpdateParams,
    SneakUpdateResult,
};
use aether_protocol::syntax::{SyntaxHighlightSnippet, SyntaxHighlightSnippetParams};
use aether_protocol::viewport::{
    DiagnosticSeverity, ViewportLinesChanged, ViewportLinesChangedParams, ViewportSubscribeResult,
    ViewportWindowResult, Window, WrapMode,
};
use aether_protocol::workspace::{
    WorkspaceActivate, WorkspaceActivateParams, WorkspaceActivateResult, WorkspaceAddProject,
    WorkspaceAddProjectParams, WorkspaceAddRoot, WorkspaceAddRootParams, WorkspaceCreate,
    WorkspaceCreateParams, WorkspaceDelete, WorkspaceDeleteParams, WorkspaceInferLanguage,
    WorkspaceInferLanguageParams, WorkspaceInfo, WorkspaceOpenPath, WorkspaceOpenPathParams,
    WorkspaceRemoveProject, WorkspaceRemoveProjectParams, WorkspaceRemoveRoot,
    WorkspaceRemoveRootParams, WorkspaceRemoveRootResult, WorkspaceRename, WorkspaceRenameParams,
    WorkspaceRenamed, WorkspaceRenamedParams,
};
use aether_protocol::{BufferId, LogicalPosition};

/// A core event: an async result (or shell-forwarded input) the core's update consumes.
#[derive(Debug)]
pub enum Event {
    SaveTried(Result<SaveTry, String>),
    ReloadTried(Result<ReloadTry, String>),
    /// A cursor-returning RPC resolved (motions, selections, clicks). Reveals as a `Follow`.
    CursorMsg(Result<CursorState, String>),
    /// As [`CursorMsg`](Event::CursorMsg), but the move was a targeted jump (go-to-line) so the
    /// reveal rests the cursor a quarter down rather than scrolling the minimum.
    CursorJump(Result<CursorState, String>),
    /// An edit resolved: adopt the new revision + cursor.
    EditDone(Result<EditResult, String>),
    UndoRedoDone(Result<UndoResult, String>),
    CopyDone(Result<BufferCopyResult, String>),
    CutDone(Result<BufferCutResult, String>),
    /// The shell read the system clipboard for a paste gesture.
    ClipboardRead(PasteKind, Option<String>),
    /// A buffer switch resolved (close, new scratch, path opens): rebind to this buffer. An open
    /// picker survives the switch (see [`Session::adopt_switch`]) — closing it is the pick path's
    /// own job — so the Buffers picker closing the active buffer keeps its list up.
    Switched(Result<BufferOpenResult, String>),
    /// A `buffer/content` fetch for the markdown reading view resolved: parse and adopt
    /// (docs/markdown-view.md §3.1). Guarded against staleness — the buffer may have switched, or
    /// moved to a newer revision, while the fetch was in flight.
    ReadContent(Result<BufferContentResult, String>),
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
    /// Cursor-line blame resolved; `text` is pre-formatted by the shell ("author · 3w ago"
    /// needs a clock, which the core deliberately lacks).
    BlameLine {
        buffer_id: aether_protocol::BufferId,
        line: u32,
        text: Option<String>,
    },
    HunkNav(Result<GitNavigateHunkResult, String>),
    HunkApplied {
        action: HunkAction,
        result: Result<GitApplyHunkResult, String>,
    },
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
    /// `directory/list` for the save-as path editor resolved; `abs` is the staleness key.
    SaveAsListing {
        abs: String,
        result: Result<DirectoryListResult, String>,
    },
    /// `directory/list` for the settings overlay's add-project row, keyed by the absolute directory
    /// it was requested for so a stale reply (the editor moved on) is dropped.
    AddProjectListing {
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
    /// `picker/section_jump` resolved: the next/prev section start (None at the ends) — the
    /// next file's first hit (Grep) or the next top-level symbol (DocumentSymbols).
    SectionJumped(Result<Option<PickerItem>, String>),
    /// `path/delete` (Explorer/Files trash) resolved. `noun` labels the success toast; the
    /// open picker re-lists. Buffer closes for the deleted path arrive via the `buffer/closed`
    /// push, which already switches us off a deleted current buffer.
    PathDeleted {
        noun: &'static str,
        result: Result<PathDeleteResult, String>,
    },
    /// `buffer/set_transient` (the `Space k` keep toggle) resolved. The bool is the buffer's new
    /// transient flag; the toast confirms it (`self.buffer.transient` itself rides the `buffer/state`
    /// push). Errors surface as an error toast.
    KeepToggled(Result<bool, String>),
    /// `directory/create` (Explorer "+ Create … name/") resolved: navigate into the new directory.
    DirCreated(Result<DirectoryCreateResult, String>),
    /// Workspace switch resolved: the activated workspace + the buffer to land on.
    WorkspaceActivated(Result<(WorkspaceInfo, BufferOpenResult), String>),
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
    /// `buffer/close` resolved for a buffer in an *ephemeral* ("(workspace N)") context, closed
    /// without an `open_next` scratch. Carries the workspace's next remaining buffer: `Some` →
    /// attach to it; `None` → the context is empty, so leave it (quit on native, chooser on web —
    /// see [`App::leave_ephemeral_workspace`]).
    EphemeralClosed(Result<Option<BufferId>, String>),
    /// `buffer/close` resolved for the [tether](Session::tether) (docs/tether.md): the client's
    /// job is done, so exit. No successor to adopt — the close was issued without `open_next`.
    TetherClosed(Result<(), String>),
    /// `buffer/set_transient` resolved for the un-keep that *releases* the tether (`Space k` on
    /// the tethered buffer): drop the tether — one-way — and toast the release. The transient
    /// flag itself rides the `buffer/state` push, as with [`Event::KeepToggled`].
    TetherReleased(Result<bool, String>),
    /// `settings/get` resolved at boot: seed the session from the persisted app settings (notably
    /// the soft-wrap default). A failure is non-fatal — we keep the defaults.
    AppSettingsLoaded(Result<AppSettings, String>),
    /// `hints/state` resolved at boot (alongside the settings fetch): adopt the hint
    /// learning snapshot (docs/hints.md). The engine stays dormant until this lands — which is
    /// also the "server connection succeeded" gate for the very first hint. Failure is non-fatal:
    /// no hints this session.
    HintsStateLoaded(Result<HintsStateResult, String>),
    /// `history/state` resolved: adopt the active workspace's `Up`/`Down` recall lists
    /// (docs/input-history.md). Fetched at boot and after every workspace switch. Failure is
    /// non-fatal — the lists stay as they were and recall just has less to offer.
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
        open: BufferOpenResult,
        restarted: bool,
    },
    /// A fire-and-forget RPC completed; result ignored.
    Noop,
}

impl Session {
    /// Dispatch one core event. The shell feeds these from its bridge variant and executes
    /// the returned effects.
    ///
    /// Wraps [`Self::dispatch_event`] to drive LSP symbol highlighting (see
    /// [`Self::after_step_highlight`]). Cursor moves arrive here as `CursorMsg` results, so this
    /// covers motions, edits, jumps, and undo; the synchronous search-clear paths are caught by the
    /// twin wrapper on [`Self::on_key`].
    pub fn on_event(&mut self, event: Event) -> Effects {
        let before = self.highlight_trigger_state();
        let fx = self.dispatch_event(event);
        // Events move the hint context too (a picker/view result opens the picker overlay, a
        // buffer switch lands, …) — keep the corner in sync outside the key path as well.
        let fx = fx.and(self.sync_hint_context());
        self.after_step_highlight(fx, before)
    }

    /// Snapshot of the inputs that decide whether a step should re-request symbol highlights: the
    /// cursor position, the buffer revision, whether a search is active, and the mode.
    fn highlight_trigger_state(&self) -> (LogicalPosition, u64, bool, Mode) {
        (
            self.buffer.cursor.position,
            self.buffer.revision,
            self.search.active,
            self.mode,
        )
    }

    /// After a reducer step, keep the symbol highlight set in sync with the cursor and mode. Leaving
    /// Normal mode (into Insert or the search prompt) clears the set so a stale highlight can't
    /// linger; otherwise the set is re-requested when the cursor landed somewhere new, the buffer
    /// was edited (the server drops the now-stale set on every mutation, so it needs re-resolving
    /// even when the cursor stayed put — e.g. a line-comment toggle with the cursor in the indent),
    /// a search just ended (its highlights were dropped and the symbol set should return), or we
    /// just came back to Normal mode. One trigger shared by both reducer entry points so every such
    /// transition is covered exactly once; the server debounces and only paints when no search is
    /// active.
    fn after_step_highlight(
        &mut self,
        fx: Effects,
        before: (LogicalPosition, u64, bool, Mode),
    ) -> Effects {
        let (before_pos, before_rev, before_search, before_mode) = before;
        if before_mode == Mode::Normal && self.mode != Mode::Normal {
            return fx.and(self.set_document_highlight(false));
        }
        let moved = self.buffer.cursor.position != before_pos;
        let edited = self.buffer.revision != before_rev;
        let search_ended = before_search && !self.search.active;
        let entered_normal = before_mode != Mode::Normal && self.mode == Mode::Normal;
        if moved || edited || search_ended || entered_normal {
            fx.and(self.set_document_highlight(true))
        } else {
            fx
        }
    }

    /// Sync the server-side symbol highlight set for the current buffer (fire-and-forget; the result
    /// rides `viewport/lines_changed`). `active` resolves and paints the symbol under the cursor;
    /// `!active` clears it. Painting is gated to Normal mode with no active search — a search owns
    /// the highlight layer, and in Insert mode the symbol is mid-edit, so re-highlighting on every
    /// keystroke would just be noise. Either way it's gated to buffers that actually have a language
    /// server, so plain-text buffers never round-trip.
    fn set_document_highlight(&mut self, active: bool) -> Effects {
        if self.buffer.lsp_server.is_none() {
            return Effects::none();
        }
        if active && (self.mode != Mode::Normal || self.search.active) {
            return Effects::none();
        }
        self.request::<LspDocumentHighlight>(
            LspDocumentHighlightParams {
                buffer_id: self.buffer.buffer_id,
                active,
            },
            |_r| Event::Noop,
        )
    }

    fn dispatch_event(&mut self, event: Event) -> Effects {
        match event {
            Event::CursorMsg(Ok(cursor)) => {
                self.buffer.cursor = cursor;
                // A staged cross-file-anchor parse installs now — the cursor is on the
                // heading, so the first paint lands in place (§2.4).
                self.install_staged_read()
                    .and(Effects::one(Effect::RevealCursor(RevealStyle::Follow)))
            }
            Event::CursorMsg(Err(e)) => self.install_staged_read().and(Effects::error(e)),

            // Go-to-line and other targeted motions reveal as a jump (rest a quarter down).
            Event::CursorJump(Ok(cursor)) => self.jump_to_cursor(cursor),
            Event::CursorJump(Err(e)) => Effects::error(e),

            Event::EditDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                Effects::one(Effect::RevealCursor(RevealStyle::Follow))
            }
            Event::EditDone(Err(e)) => Effects::error(e),

            Event::UndoRedoDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                let mut fx = if r.applied {
                    Effects::none()
                } else {
                    // Grouped so mashing undo/redo at the ends of the stack updates one toast
                    // in place instead of stacking duplicates on every shell.
                    Effects::toast_grouped("Nothing to undo or redo", ToastKind::Info, "undo-redo")
                };
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                fx
            }
            Event::UndoRedoDone(Err(e)) => Effects::error(e),

            // Opening replaces whatever prompt was up: `Space ?` is only reachable from Normal mode
            // via the leader, so nothing that owns the keyboard can be underneath it.
            Event::AppInfoLoaded(Ok(info)) => {
                self.prompt = Some(Prompt::AppInfo(Some(Box::new(info))));
                Effects::none()
            }
            Event::AppInfoLoaded(Err(e)) => Effects::error(format!("App info failed: {e}")),

            Event::CopyDone(Ok(r)) => {
                let mut fx =
                    Effects::toast(format!("Copied {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx
            }
            Event::CopyDone(Err(e)) => Effects::error(format!("Copy failed: {e}")),

            Event::CutDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                let mut fx =
                    Effects::toast(format!("Cut {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx.push(Effect::RevealCursor(RevealStyle::Follow));
                fx
            }
            Event::CutDone(Err(e)) => Effects::error(format!("Cut failed: {e}")),

            Event::ClipboardRead(kind, text) => {
                let Some(text) = text.filter(|t| !t.is_empty()) else {
                    return Effects::error("Clipboard is empty");
                };
                self.paste(kind, text)
            }

            Event::Switched(Ok(open)) => self.adopt_navigation(open),
            Event::Switched(Err(e)) => {
                // A failed jump-shaped open must not leave its flag armed for the next
                // (unrelated) switch — it would wrongly land a markdown file in the editor.
                self.open_route_jumped = false;
                Effects::error(e)
            }

            Event::ReadContent(Ok(c)) => {
                let Some(read) = self.read.as_mut() else {
                    return Effects::none(); // reading view was left while the fetch was in flight
                };
                if read.buffer_id != self.buffer.buffer_id {
                    return Effects::none(); // buffer switched under the fetch
                }
                // The buffer moved on while the fetch was in flight — chase the newer
                // revision. A pending anchor stays armed for the fresh fetch, and the view
                // stays loading meanwhile (the anchor-hold invariant: no paint before place).
                if self.buffer.revision > c.revision {
                    if self.pending_read_anchor.is_none() {
                        read.adopt(c.revision, c.text);
                    }
                    return self.refetch_read_content();
                }
                // A followed cross-file anchor is pending: stage the parse instead of
                // installing it — the document paints once, already in place (§2.4).
                if self.pending_read_anchor.is_some() {
                    let mut staged = ReadView::loading(self.buffer.buffer_id);
                    staged.adopt(c.revision, c.text);
                    return self.stage_read_place(staged);
                }
                read.adopt(c.revision, c.text);
                self.read_fence_requests()
            }
            Event::ReadContent(Err(e)) => {
                self.pending_read_anchor = None;
                // Fall back to the editor rather than showing an empty page.
                if self.read.take().is_some() && self.mode == Mode::Read {
                    self.mode = Mode::Normal;
                }
                Effects::error(format!("Reading view failed to load: {e}"))
            }

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
                let Some(read) = self.read.as_mut() else {
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
                // A sibling buffer still lives in this ephemeral context — attach to it.
                self.request_str::<BufferOpen>(
                    BufferOpenParams {
                        buffer_id: Some(next),
                        ..Default::default()
                    },
                    Event::Switched,
                )
            }
            Event::EphemeralClosed(Ok(None)) => self.leave_ephemeral_workspace(),
            Event::EphemeralClosed(Err(e)) => Effects::error(format!("Close failed: {e}")),

            // The tether closed cleanly — the quick edit this client was launched for is over.
            Event::TetherClosed(Ok(())) => Effects::one(Effect::Exit),
            Event::TetherClosed(Err(e)) => Effects::error(format!("Close failed: {e}")),
            Event::TetherReleased(Ok(_)) => {
                self.tether = None;
                // Same toast group as the plain keep toggle, so repeated presses update in place.
                Effects::toast_grouped("Tether released", ToastKind::Success, "transient")
            }
            Event::TetherReleased(Err(e)) => Effects::error(format!("Keep toggle failed: {e}")),

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
                        line: 0,
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
            Event::JumplistCaptured(Err(e), _) => Effects::error(format!("Capture failed: {e}")),

            Event::JumplistStepped(Ok(JumplistStepResult::Moved(t)), _, _) => match t.opened {
                Some(open) => {
                    // Jumplist steps are jump-shaped: a markdown target opens in the editor
                    // (docs/markdown-view.md §1.6).
                    self.open_route_jumped = true;
                    self.adopt_navigation(open)
                }
                None => Effects::none(), // open:true is always sent; defensive
            },
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
                    "No jumplist entries in this file — ] steps across files",
                    ToastKind::Info,
                    "jumplist",
                )
            }
            // Grouped so repeatedly pressing `]` with nothing captured coalesces to one toast.
            Event::JumplistStepped(Ok(JumplistStepResult::Empty), _, _) => Effects::toast_grouped(
                "Jumplist is empty — Ctrl-j in a picker captures results",
                ToastKind::Info,
                "jumplist",
            ),
            Event::JumplistStepped(Err(e), _, _) => Effects::error(e),

            Event::PromptAccept => self.accept_prompt(),
            Event::PromptCancel => self.decline_prompt(),

            Event::SearchApplied(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                let zero = r.summary.total == 0;
                self.search.summary = Some(r.summary);
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
                self.search.summary = Some(SearchSummary {
                    buffer_id: self.buffer.buffer_id,
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
                self.search.summary = Some(r.summary);
                Effects::none()
            }
            Event::SearchRestored(Err(e)) => Effects::error(e),

            Event::SearchNav(Ok(r)) => {
                self.search.summary = Some(r.summary);
                self.jump_to_cursor(r.cursor)
            }
            Event::SearchNav(Err(e)) => Effects::error(e),

            Event::SneakUpdated(Ok(result)) => {
                // The session may have ended (label pressed, Esc) before this result landed; only
                // adopt labels while still sneaking.
                if let Some(sneak) = self.sneak.as_mut() {
                    sneak.labels = result.labels;
                }
                Effects::none()
            }
            Event::SneakUpdated(Err(e)) => Effects::error(e),

            Event::SearchFromSel(Ok(Some((query, r)))) => {
                self.search.query = query.clone();
                // Mirror the defaults the request went out with, so the committed search's state
                // matches how the server is actually matching it.
                self.search.options = MatchOptions::default();
                self.search.active = true;
                self.search.summary = Some(r.summary);
                let entry = HistoryEntry::with_options(query, self.search.options);
                self.record_history(HistoryKind::Search, entry)
            }
            Event::SearchFromSel(Ok(None)) => Effects::none(), // empty selection
            Event::SearchFromSel(Err(e)) => Effects::error(e),

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
                Err(e) => Effects::error(e),
            },

            Event::Definition(Ok(r)) => match lsp_readiness_message(r.readiness) {
                Some(msg) => Effects::toast(msg, ToastKind::Info),
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
            Event::Definition(Err(e)) => Effects::error(e),

            Event::DiagNav(Ok(r)) => self.step_to_cursor(r.cursor, r.moved, "No more diagnostics"),
            Event::DiagNav(Err(e)) => Effects::error(e),

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
                    let msg = lsp_readiness_message(r.readiness).unwrap_or("No hover info");
                    let mut fx = Effects::one(Effect::DismissHover);
                    fx.push(Effect::Toast {
                        message: msg.into(),
                        kind: ToastKind::Info,
                        group: None,
                    });
                    fx
                }
            },
            Event::HoverInfo(Err(e)) => Effects::error(format!("Hover failed: {e}")),

            Event::FormatDone(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                // Specific feedback per outcome — "nothing happened" has several causes.
                let note = match r.status {
                    FormatStatus::Applied => None,
                    FormatStatus::NoChange => Some("Already formatted".to_string()),
                    FormatStatus::NotReady => Some("Language server still starting".to_string()),
                    FormatStatus::Unavailable => Some("Language server unavailable".to_string()),
                    FormatStatus::Unsupported => Some(match self.buffer.language.as_deref() {
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
            Event::FormatDone(Err(e)) => Effects::error(format!("Format failed: {e}")),

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
            Event::CommitLookup(Err(e)) => Effects::error(format!("Commit info failed: {e}")),

            Event::BlameLine {
                buffer_id,
                line,
                text,
            } => {
                if buffer_id == self.buffer.buffer_id && line == self.buffer.cursor.position.line {
                    self.blame = text.map(|t| (line, t));
                }
                Effects::none()
            }

            Event::HunkNav(Ok(r)) => self.step_to_cursor(r.cursor, r.moved, "No more changes"),
            Event::HunkNav(Err(e)) => Effects::error(e),

            Event::HunkApplied { action, result } => match result {
                Ok(r) => {
                    self.buffer.cursor = r.cursor;
                    let (msg, kind) = match r.status {
                        ApplyHunkStatus::Staged => ("Staged change", ToastKind::Success),
                        ApplyHunkStatus::Unstaged => ("Unstaged change", ToastKind::Success),
                        ApplyHunkStatus::Reverted => ("Reverted change", ToastKind::Success),
                        ApplyHunkStatus::NoChange => (
                            match action {
                                HunkAction::Toggle => "No change here",
                                HunkAction::Revert => "No change to revert here",
                            },
                            ToastKind::Info,
                        ),
                        ApplyHunkStatus::DirtyBuffer => {
                            ("Unsaved changes — save first", ToastKind::Warning)
                        }
                        ApplyHunkStatus::Unavailable => {
                            ("Not in a git repository", ToastKind::Info)
                        }
                    };
                    Effects::toast(msg, kind)
                }
                Err(e) => Effects::error(e),
            },

            Event::DiffViewSet { enabled, result } => match result {
                Ok(r) => {
                    self.diff_view = enabled;
                    self.window = Some(r.window);
                    let mut fx = Effects::one(Effect::WindowAdopted);
                    // Grouped so repeated toggling updates one toast in place rather than stacking.
                    fx.push(Effect::Toast {
                        message: format!("Diff {}", if enabled { "on" } else { "off" }),
                        kind: ToastKind::Info,
                        group: Some("diff".into()),
                    });
                    fx
                }
                Err(e) => Effects::error(e),
            },

            Event::PickerViewed { initial, result } => match result {
                Ok(r) => {
                    let chase_offset = if let Some(p) = &mut self.picker {
                        // The single in-flight refetch slot is free again (Rule 2 below may re-arm
                        // it). Harmless for an initial open, which never set it.
                        p.refetch_in_flight = false;
                        p.offset = r.effective_offset;
                        if let Some(center) = r.effective_center_on {
                            p.pending_center = Some(center);
                            // File-grouped centering (cursor-hit opens, file jumps) aligns the
                            // target to the top — its file header sits just above and there's
                            // context below to read.
                            p.reveal_on_update = Some(if p.kind.groups_by_file() {
                                Reveal::Top
                            } else {
                                Reveal::Minimal
                            });
                        }
                        p.directory = r.directory_path;
                        p.directory_parent = r.directory_parent;
                        if initial {
                            // Adopt the resumed query (the changes pickers preserve theirs across
                            // opens; every other kind comes back empty) and the persisted filters
                            // (seeded opens get their seed echoed).
                            p.generation = r.generation;
                            p.query = r.query;
                            p.total_candidates = r.total_candidates;
                            p.adopt_filters(&r.filters);
                        }
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
                    Effects::error(format!("Picker failed: {e}"))
                }
            },

            // Selections open in place: the window shows one buffer, and the one being
            // replaced is a `Space b` away (buffers persist server-side). Opens are
            // transient previews — switching away from one closes it.
            Event::PickerSelected { result: Ok(result) } => match result {
                PickerSelectResult::File { path } => self.open_path_at(path, None, None),
                PickerSelectResult::FileAt {
                    path,
                    position,
                    anchor,
                } => self.open_path_at(path, Some(position), anchor),
                PickerSelectResult::Buffer { buffer_id } => {
                    if buffer_id == self.buffer.buffer_id {
                        return Effects::none(); // already showing it
                    }
                    self.request_str::<BufferOpen>(
                        BufferOpenParams {
                            buffer_id: Some(buffer_id),
                            record_nav_from: Some(self.buffer.buffer_id),
                            ..Default::default()
                        },
                        Event::Switched,
                    )
                }
                PickerSelectResult::Workspace { name } => {
                    // Activate and land on the workspace's last buffer (or a fresh transient
                    // scratch) — the bootstrap convention, now one server-side composite.
                    self.request_str::<WorkspaceActivate>(
                        WorkspaceActivateParams {
                            name,
                            open_last: true,
                        },
                        |r| {
                            Event::WorkspaceActivated(r.and_then(|a| {
                                let opened = a.opened.ok_or_else(|| {
                                    "workspace/activate returned no landing buffer".to_string()
                                })?;
                                Ok((a.workspace, opened))
                            }))
                        },
                    )
                }
            },
            Event::PickerSelected { result: Err(e), .. } => {
                Effects::error(format!("Select failed: {e}"))
            }

            Event::WorkspaceActivated(Ok((workspace, open))) => {
                self.workspace = workspace.name;
                self.workspace_paths = workspace.paths;
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
            Event::WorkspaceActivated(Err(e)) => {
                Effects::error(format!("Workspace switch failed: {e}"))
            }

            Event::WorkspaceCreated(Ok(activate)) => {
                let WorkspaceActivateResult {
                    workspace, opened, ..
                } = activate;
                self.workspace = workspace.name.clone();
                self.workspace_paths = workspace.paths;
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
                    None => self.request::<BufferOpen>(BufferOpenParams::default(), move |__r| {
                        Event::Switched(__r.map_err(|e| e.to_string()))
                    }),
                });
                fx.push(Effect::Toast {
                    message: format!("Created workspace {}", workspace.name),
                    kind: ToastKind::Success,
                    group: None,
                });
                // The natural next step for a freshly created (rootless) workspace is adding a root,
                // so — unlike the default open, which focuses the name field — land on the add-root
                // input here.
                self.open_workspace_settings();
                if let Some(s) = self.workspace_settings.as_mut() {
                    s.selected = s.input_index();
                }
                fx
            }
            Event::WorkspaceCreated(Err(e)) => {
                Effects::error(format!("Create workspace failed: {e}"))
            }

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
                        if let Some(s) = self.workspace_settings.as_mut() {
                            s.add.clear();
                            s.error = None;
                            // Re-focus the add-root input (now one row further down).
                            s.selected = s.input_index();
                        }
                        Effects::toast(format!("Added root to {name}"), ToastKind::Success)
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
                    let mut fx = Effects::toast(
                        if closed.is_empty() {
                            format!("Removed root from {name}")
                        } else {
                            format!(
                                "Removed root from {name}; closed {} buffer(s)",
                                closed.len()
                            )
                        },
                        ToastKind::Success,
                    );
                    // If our current buffer was one of the closed ones, switch to the server-
                    // indicated next buffer (or a fresh scratch).
                    if closed.contains(&self.buffer.buffer_id) {
                        fx = fx.and(self.request::<BufferOpen>(
                            BufferOpenParams {
                                buffer_id: r.next_buffer_id,
                                ..Default::default()
                            },
                            move |__r| Event::Switched(__r.map_err(|e| e.to_string())),
                        ));
                    }
                    fx
                }
                Err(e) => {
                    if let Some(s) = self.workspace_settings.as_mut() {
                        s.error = Some(e);
                        Effects::none()
                    } else {
                        Effects::error(format!("Remove root failed: {e}"))
                    }
                }
            },
            Event::WorkspaceDeleted(result) => match result {
                // The switcher stays open; the refreshed list (sans the deleted row) arrives as a
                // `picker/update` push from the server's `refresh_workspace_pickers`.
                Ok(()) => Effects::toast("Deleted workspace", ToastKind::Success),
                // Covers the active-workspace and dirty-buffer refusals — the server messages are
                // already user-facing.
                Err(e) => Effects::error(e),
            },

            Event::PickerClicked(abs) => {
                if let Some(p) = &mut self.picker {
                    p.selected = abs;
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
                Err(_) if self.hints_enabled => Effects::toast_grouped(
                    "Hints unavailable — restart the Aether server (ae server stop)",
                    ToastKind::Warning,
                    "hints",
                ),
                Err(_) => Effects::none(),
            },

            Event::AppSettingsSaved(result) => match result {
                Ok(_) => Effects::none(),
                Err(e) => Effects::error(format!("Couldn't save settings: {e}")),
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

            Event::SaveAsListing { abs, result } => {
                // Stale responses (the editor moved to another directory, or closed) are dropped
                // by the abs-path staleness key. Refreshes only the ghost — no live results behind
                // the save prompt, so nothing else to re-run.
                if let Some(Prompt::SaveAs(ed)) = self.prompt.as_mut() {
                    if ed.listing_dir_abs == abs {
                        match result {
                            Ok(r) => ed.set_dir_listing(r.entries),
                            Err(_) => ed.set_dir_listing_failed(),
                        }
                    }
                }
                Effects::none()
            }

            Event::AddProjectListing { abs, result } => {
                // Same staleness rule as `SaveAsListing`, against the settings overlay's editor.
                if let Some(st) = self.workspace_settings.as_mut() {
                    if st.add_project.listing_dir_abs == abs {
                        match result {
                            Ok(r) => st.add_project.set_dir_listing(r.entries),
                            Err(_) => st.add_project.set_dir_listing_failed(),
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

            Event::SectionJumped(Ok(None)) => Effects::none(), // already at the first/last
            Event::SectionJumped(Ok(Some(target))) => {
                let Some(p) = &mut self.picker else {
                    return Effects::none();
                };
                // In the loaded window → purely local move, no refetch; the target aligns
                // to the top so the file reads from its first hit.
                let key = item_key(&target);
                if let Some(idx) = p.items.iter().position(|i| item_key(i) == key) {
                    p.selected = p.offset + idx as u32;
                    return Effects::one(Effect::RevealPickerSelection(Reveal::Top));
                }
                // Past the window → re-frame around the target; the arriving push lands the
                // highlight via the `effective_center_on` echo (Reveal::Top for grep).
                let kind = p.kind;

                self.request::<PickerView>(
                    PickerViewParams {
                        kind,
                        reset: PickerReset::Keep,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: Some(target),
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
                        result: __r.map_err(|e| e.to_string()),
                    },
                )
            }
            Event::SectionJumped(Err(e)) => Effects::error(format!("File jump failed: {e}")),

            Event::PathDeleted { noun, result } => match result {
                Err(e) => Effects::error(format!("Delete failed: {e}")),
                Ok(_) => {
                    // Any close of *our* buffer rides the `buffer/closed` push (it switches us
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
                Err(e) => Effects::error(format!("Keep toggle failed: {e}")),
                // Grouped: toggling keep/release updates one toast rather than stacking a pair.
                Ok(transient) => Effects::toast_grouped(
                    if transient {
                        "Buffer released"
                    } else {
                        "Buffer kept"
                    },
                    ToastKind::Success,
                    "transient",
                ),
            },
            Event::DirCreated(Err(e)) => Effects::error(format!("Create directory failed: {e}")),
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
                    had_unsaved: self.buffer.revision != self.buffer.saved_revision,
                };
                // Drop out of Insert: edits can't reach the server while down, and a live insert
                // cursor with vanishing keystrokes reads as a freeze. We don't restore it on
                // reconnect (the buffer may have changed under us, or the daemon restarted and lost
                // it) — the user re-enters insert deliberately. A reading view stays a reading
                // view (it's client-rendered; only its refreshes need the server).
                self.mode = self.search_return_mode();
                tracing::warn!(buffer = %self.buffer.label, "connection lost; reconnecting");
                // Grouped "connection": the matching "Reconnected" toast replaces this one in place.
                let mut fx = Effects::toast_grouped(
                    "Server disconnected — reconnecting…",
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
                Effects::toast_grouped(
                    format!("Reconnect failed: {e}"),
                    ToastKind::Error,
                    "connection",
                )
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
                let old_cursor = self.buffer.cursor;
                let old_buffer_id = self.buffer.buffer_id;
                self.workspace = workspace.name;
                self.workspace_paths = workspace.paths;
                self.workspace_projects = workspace.projects;
                let same_file = open.path == self.buffer.path;
                self.buffer = buffer_info(open, &self.workspace_paths);
                // Buffer ids don't survive a daemon restart: remap the tether onto the reopened
                // buffer when it's the same file we were tethered to, else drop it — a stale id
                // could collide with an unrelated new buffer and exit under the user.
                if restarted {
                    self.tether = (same_file && self.tether == Some(old_buffer_id))
                        .then_some(self.buffer.buffer_id);
                }
                self.conn = ConnState::Connected;
                // Server-side per-client state died with the old connection; drop the client
                // overlays that fronted it. The frozen window stays rendered until the
                // resubscribe replaces it.
                self.viewport_id = None;
                self.blame = None;
                self.blame_requested = None;
                self.prompt = None;
                self.picker = None;
                let buffer_id = self.buffer.buffer_id;
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
                if same_file && self.search.active && !self.search.query.is_empty() {
                    fx = fx.and(self.request::<SearchSet>(
                        SearchSetParams {
                            buffer_id,
                            query: self.search.query.clone(),
                            anchor: None,
                            extend: false,
                            from_selection: false,
                            options: self.search.options,
                        },
                        move |__r| Event::SearchRestored(__r.map_err(|e| e.to_string())),
                    ));
                }
                fx.push(if restarted && had_unsaved {
                    Effect::Toast {
                        message: "Reconnected — the server restarted, unsaved changes were lost"
                            .into(),
                        kind: ToastKind::Warning,
                        group: Some("connection".into()),
                    }
                } else {
                    Effect::Toast {
                        message: "Reconnected".into(),
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
                self.buffer.revision = result.revision;
                self.buffer.saved_revision = result.revision;
                self.buffer.transient = false; // saving promotes a transient buffer
                self.externally_modified = false;
                self.externally_deleted = false;
                let note = match target {
                    Some((path_index, rel)) => {
                        // Save-as: the buffer's identity changed — adopt the new path/label. The
                        // label takes the same canonical `"[root]: [path]"` form as buffer-open, so
                        // a renamed buffer reads identically in the status bar, title, and picker.
                        let root = self.workspace_paths.get(path_index as usize);
                        self.buffer.path =
                            root.map(|r| format!("{}/{rel}", r.trim_end_matches('/')));
                        self.buffer.label = crate::labels::root_relative_display(
                            &self.workspace_paths,
                            path_index,
                            &rel,
                        );
                        format!("Saved as {rel} (rev {})", result.revision)
                    }
                    None => format!("Saved (rev {})", result.revision),
                };
                let mut fx = Effects::toast(note, ToastKind::Success);
                match after {
                    AfterSave::Nothing => {}
                    // Save-and-quit (`Space Alt-q`): the save landed, so close the window — the
                    // server drops per-client state on disconnect, so this is exactly `Space q`.
                    AfterSave::Quit => fx.push(Effect::Exit),
                    // Save-and-close (`Space Alt-x`): the buffer is clean now, so this close
                    // never re-prompts — and when the buffer is the tether, it exits the client.
                    AfterSave::Close => fx = fx.and(self.close_buffer()),
                }
                fx
            }
            Event::SaveTried(Ok(SaveTry::NeedsConfirm { kind, action })) => {
                self.prompt = Some(Prompt::Confirm { kind, action });
                Effects::none()
            }
            Event::SaveTried(Err(e)) => Effects::error(format!("Save failed: {e}")),

            Event::ReloadTried(Ok(ReloadTry::Reloaded(r))) => {
                self.buffer.revision = r.revision;
                self.buffer.saved_revision = r.revision;
                self.buffer.transient = false; // reloading promotes, like save
                self.externally_modified = false;
                self.externally_deleted = false;
                Effects::toast(format!("Reloaded (rev {})", r.revision), ToastKind::Success)
            }
            Event::ReloadTried(Ok(ReloadTry::NeedsConfirm)) => {
                self.prompt = Some(Prompt::Confirm {
                    kind: ConfirmKind::DiscardOnReload,
                    action: ConfirmAction::ReloadDiscard,
                });
                Effects::none()
            }
            Event::ReloadTried(Err(e)) => Effects::error(format!("Reload failed: {e}")),
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
        let buffer_id = self.buffer.buffer_id;
        let (path_index, relative_path) = match &target {
            Some((i, p)) => (Some(*i), Some(p.clone())),
            None => (None, None),
        };

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
                    Err(e) => Err(e.to_string()),
                })
            },
        )
    }

    /// Fire an edit RPC; the result lands as [`Event::EditDone`].
    /// Allocate a token, park the result mapping, and emit `Effect::Request` — the
    /// sans-IO replacement for spawning an RPC future (docs/client-core.md). The shell
    /// performs the call and feeds the outcome back through [`Session::on_rpc_result`].
    fn request<M>(
        &mut self,
        params: M::Params,
        f: impl FnOnce(Result<M::Result, RpcError>) -> Event + Send + 'static,
    ) -> Effects
    where
        M: RpcMethod + 'static,
    {
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
        self.request::<M>(params, move |r| f(r.map_err(|e| e.to_string())))
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
        let event = f(result);
        self.on_event(event)
    }

    pub fn edit<M>(&mut self, params: M::Params) -> Effects
    where
        M: RpcMethod<Result = EditResult> + 'static,
    {
        self.request_str::<M>(params, Event::EditDone)
    }

    /// Insert clipboard text per the paste gesture (each one server-side edit; `Before`
    /// collapses to the selection start via `at` on the way in).
    pub fn paste(&mut self, kind: PasteKind, text: String) -> Effects {
        let buffer_id = self.buffer.buffer_id;
        match kind {
            PasteKind::Before { count } => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: text.repeat(count.max(1) as usize),
                select_pasted: true,
                replace_selection: false,
                // Insert at the selection start — the collapse rides the edit
                // (docs/protocol-composites.md, D) instead of a prior cursor/set.
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
            || self.sneak.is_some()
        {
            return Effects::none();
        }
        match self.mode {
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
        if self.mode != Mode::Insert || text.is_empty() {
            return Effects::none();
        }
        self.edit::<InputText>(InputTextParams {
            buffer_id: self.buffer.buffer_id,
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
        self.buffer.cursor = cursor;
        Effects::one(Effect::RevealCursor(RevealStyle::Jump))
    }

    /// [`Self::jump_to_cursor`] for a *stepping* motion (next/prev diagnostic or hunk), which can
    /// run out of places to go: reveal the cursor as a jump, but when the step found nowhere new
    /// (`moved == false`) toast `exhausted` instead of silently re-revealing the same spot.
    pub fn step_to_cursor(&mut self, cursor: CursorState, moved: bool, exhausted: &str) -> Effects {
        self.buffer.cursor = cursor;
        let mut fx = if moved {
            Effects::none()
        } else {
            // Grouped so repeatedly stepping with nowhere left to go coalesces to one toast.
            Effects::toast_grouped(exhausted, ToastKind::Info, "step-nav")
        };
        fx.push(Effect::RevealCursor(RevealStyle::Jump));
        fx
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
    pub fn adopt_navigation(&mut self, open: BufferOpenResult) -> Effects {
        if open.buffer_id == self.buffer.buffer_id {
            // Same buffer — the open-route flag (a cross-buffer concern) must not leak into the
            // next genuine switch.
            self.open_route_jumped = false;
            if self.pending_read_anchor.is_some() && self.read.is_some() {
                // `[x](./this-file.md#section)`: the target is the document already on
                // screen, so the anchor resolves against the live parse — no refetch fires.
                return self.consume_read_anchor();
            }
            self.pending_read_anchor = None;
            self.jump_to_cursor(open.cursor)
        } else {
            self.adopt_switch(open)
        }
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
    pub fn adopt_switch(&mut self, open: BufferOpenResult) -> Effects {
        self.mode = Mode::Normal;
        self.pending = Pending::None;
        self.count = None;
        self.diagnostics = DiagnosticCounts::default();
        self.lsp = None;
        self.externally_modified = false;
        self.externally_deleted = false;
        self.window = None;
        self.viewport_id = None;
        self.drag = None;
        self.blame = None;
        self.blame_requested = None;
        self.prompt = None;
        self.search = SearchState::default();
        self.buffer = buffer_info(open, &self.workspace_paths);
        let read_fx = self.sync_read_on_switch();
        Effects::one(Effect::Resubscribe).and(read_fx)
    }

    /// Decide the freshly adopted buffer's read/edit presentation (docs/markdown-view.md §1.6):
    /// markdown buffers follow this session's per-buffer choice when one was made, else the
    /// app-wide default — except jump-shaped opens (grep hits, references, jumplist steps), which
    /// land in the editor. Everything else opens in the editor as always.
    fn sync_read_on_switch(&mut self) -> Effects {
        let jumped = std::mem::replace(&mut self.open_route_jumped, false);
        self.read = None;
        let is_md = self.buffer.language.as_deref() == Some("markdown");
        let want = is_md
            && match self.read_pref.get(&self.buffer.buffer_id) {
                Some(explicit) => *explicit,
                None => self.markdown_read_default && !jumped,
            };
        if want {
            self.begin_read()
        } else {
            // A pending cross-file anchor can only land in a reading view; this switch went
            // to the editor (non-markdown target, or a jump-shaped route), so drop it.
            self.pending_read_anchor = None;
            Effects::none()
        }
    }

    /// Apply the open-route presentation rules (docs/markdown-view.md §1.6) to a freshly
    /// *booted* session: `ae file.md` launches install the session directly, never passing
    /// through `adopt_switch`, so the shells call this once after boot. `jumped` = the launch
    /// carried a jump target (`ae file:line`), which lands in the editor like any other jump.
    pub fn boot_read_presentation(&mut self, jumped: bool) -> Effects {
        self.open_route_jumped = jumped;
        self.sync_read_on_switch()
    }

    /// [`Self::boot_read_presentation`] with an explicit read/source choice, overriding the
    /// open-route rules: the web shell records the current presentation in the URL (`view=`),
    /// so a refresh restores exactly what was on screen — the `#line:col` cursor restore in the
    /// same URL must not read as a jump-shaped open (docs/markdown-view.md §1.6).
    pub fn boot_read_presentation_explicit(&mut self, read: bool) -> Effects {
        self.read_pref.insert(self.buffer.buffer_id, read);
        self.boot_read_presentation(false)
    }

    /// Enter the reading view on the current buffer: flip the mode and fetch the full content
    /// (the parse adopts via [`Event::ReadContent`]).
    fn begin_read(&mut self) -> Effects {
        self.mode = Mode::Read;
        self.pending = Pending::None;
        self.count = None;
        self.sneak = None;
        self.read = Some(ReadView::loading(self.buffer.buffer_id));
        let buffer_id = self.buffer.buffer_id;
        self.request_str::<BufferContent>(BufferContentParams { buffer_id }, Event::ReadContent)
    }

    /// Ask the server to highlight every fenced code block of the freshly parsed document —
    /// tree-sitter lives server-side, so this is the reading view's route to editor-grade code
    /// colour (docs/markdown-view.md §2.8). One request per fence; results adopt via
    /// [`Event::ReadHighlights`] and paint in as they land.
    fn read_fence_requests(&mut self) -> Effects {
        let fences = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let mut fences = crate::markdown::fenced_code_blocks(&read.blocks);
            // Sanity cap — no real document has hundreds of fences, but a pathological one
            // shouldn't turn into a request storm.
            fences.truncate(200);
            fences
        };
        let (buffer_id, revision) = {
            let read = self.read.as_ref().expect("checked above");
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

    /// Re-fetch the reading view's content (an external change notified a newer revision). The
    /// `loading` flag debounces: a fetch already in flight will chase the newest revision itself
    /// when it adopts.
    fn refetch_read_content(&mut self) -> Effects {
        let Some(read) = self.read.as_mut() else {
            return Effects::none();
        };
        if read.loading {
            return Effects::none();
        }
        read.loading = true;
        let buffer_id = read.buffer_id;
        self.request_str::<BufferContent>(BufferContentParams { buffer_id }, Event::ReadContent)
    }

    /// React to a change signal for `buffer_id` at `revision`: when the reading view shows that
    /// buffer at an older revision, re-fetch (docs/markdown-view.md §3.1).
    fn maybe_refresh_read(&mut self, buffer_id: BufferId, revision: u64) -> Effects {
        let stale = self
            .read
            .as_ref()
            .is_some_and(|r| r.buffer_id == buffer_id && revision > r.revision);
        if stale {
            self.refetch_read_content()
        } else {
            Effects::none()
        }
    }

    /// Adopt the result of a `viewport/subscribe` the shell issued: install the viewport binding
    /// and the buffer-wide status that rides with it atomically (diagnostics, language-server
    /// health, external-change flags), plus the first window. Pure core state — the shell owns the
    /// pixel work it does afterward (seeding the scroll, revealing the cursor). One definition
    /// shared by every shell: the native shells pass the typed result; the wasm shell deserialises
    /// the same struct. Shells must never write these fields directly (docs/web-core.md).
    pub fn adopt_subscribe(&mut self, res: ViewportSubscribeResult) {
        self.viewport_id = Some(res.viewport_id);
        self.diagnostics = res.buffer_status.diagnostics;
        self.lsp = res.buffer_status.lsp_status;
        self.externally_modified = res.buffer_status.externally_modified;
        self.externally_deleted = res.buffer_status.externally_deleted;
        self.window = Some(res.window);
    }

    /// Adopt the window from a geometry RPC the shell issued (`viewport/scroll`, `scroll_to_row`,
    /// `resize`). Pure core state; the shell clamps its scroll and reveals the cursor around it.
    pub fn adopt_window(&mut self, res: ViewportWindowResult) {
        self.window = Some(res.window);
    }

    /// Report the viewport's current scroll position so the core knows what's actually on screen
    /// (the shell owns the pixel scroll). `top_visual_row` is absolute (whole-buffer); the core maps
    /// it through the loaded window to a logical-line range that scopes sneak candidates. Cheap —
    /// safe to call every render/scroll.
    pub fn set_visible_lines(&mut self, top_visual_row: u32, viewport_rows: u32) {
        self.visible_lines = self.window.as_ref().map(|w| {
            let (first, _) = crate::grid::line_at_row(w, top_visual_row);
            let bottom = top_visual_row.saturating_add(viewport_rows.saturating_sub(1));
            let (last, _) = crate::grid::line_at_row(w, bottom);
            (first, last.saturating_add(1))
        });
    }

    /// Close the buffer, then attach to the server-indicated next MRU buffer (or a fresh
    /// scratch). Closing the [tether](Session::tether) instead exits the client — no successor
    /// needed (docs/tether.md). In an *ephemeral* context, never replace it with a scratch — an
    /// empty ephemeral workspace is pointless — so we close without `open_next` and either attach
    /// to a remaining sibling buffer or leave the context entirely (see
    /// [`Self::leave_ephemeral_workspace`]).
    pub fn close_buffer(&mut self) -> Effects {
        if self.tethered() {
            return self.request_str::<BufferClose>(
                BufferCloseParams {
                    buffer_id: self.buffer.buffer_id,
                    open_next: false,
                },
                |r| Event::TetherClosed(r.map(|_| ())),
            );
        }
        if aether_protocol::is_ephemeral_workspace_id(&self.workspace) {
            return self.request_str::<BufferClose>(
                BufferCloseParams {
                    buffer_id: self.buffer.buffer_id,
                    open_next: false,
                },
                |r| Event::EphemeralClosed(r.map(|closed| closed.next_buffer_id)),
            );
        }
        self.request_str::<BufferClose>(
            BufferCloseParams {
                buffer_id: self.buffer.buffer_id,
                open_next: true,
            },
            |r| {
                Event::Switched(r.and_then(|closed| {
                    closed
                        .opened
                        .ok_or_else(|| "buffer/close returned no successor".into())
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
    /// on-disk path (`Space Alt-a`), otherwise the workspace-relative path (`Space a`). Scratch
    /// buffers have no path, so it warns instead.
    fn copy_buffer_path(&mut self, absolute: bool) -> Effects {
        let Some(path) = self.buffer.path.as_deref() else {
            return Effects::toast("Scratch buffer has no path", ToastKind::Warning);
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
        // Any fresh open invalidates a not-yet-landed cross-file anchor (`read_follow_link`
        // re-arms after this call for its own open).
        self.pending_read_anchor = None;
        let (path_index, relative_path, absolute_path) =
            match strip_longest_root(&path, &self.workspace_paths) {
                Some((idx, rel)) => (Some(idx), Some(rel), None),
                None => (None, None, Some(path)),
            };
        // A jump-shaped open (a grep hit, a reference) is a working context — it lands in the
        // editor even when the target is markdown (docs/markdown-view.md §1.6).
        self.open_route_jumped = jump_to.is_some();
        self.request_str::<BufferOpen>(
            BufferOpenParams {
                path_index,
                relative_path,
                absolute_path,
                jump_to,
                jump_to_anchor,
                transient: Some(true),
                record_nav_from: Some(self.buffer.buffer_id),
                ..Default::default()
            },
            Event::Switched,
        )
    }

    /// Record a committed value to its input-history list (docs/input-history.md) and, when that
    /// actually changed the list, tell the server so it persists and other windows see it. The
    /// local apply is not optimism — it's the same [`HistoryLists::record`] rule the server runs,
    /// so the two can't disagree; the round-trip is fire-and-forget.
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
            message: format!("Restarting {name}"),
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
            Prompt::OpenPath(field) => {
                // Plain single-line path field — text entry is shell-owned (synced via
                // `open_path_set_input`); only Enter (open) and Esc (cancel) are command keys.
                let no_chord = !mods.ctrl && !mods.alt;
                match code {
                    KeyCode::Enter if no_chord => {
                        let path = field.text.trim().to_string();
                        if path.is_empty() {
                            self.prompt = Some(Prompt::OpenPath(field)); // nothing typed — stay open
                            Effects::none()
                        } else {
                            self.commit_open_path(path)
                        }
                    }
                    // Esc: the prompt was already taken above, so just leaving it `None` cancels.
                    KeyCode::Esc => Effects::none(),
                    _ => {
                        self.prompt = Some(Prompt::OpenPath(field));
                        Effects::none()
                    }
                }
            }
        }
    }

    /// `Space j` — show the diagnostic(s) at the cursor in the hover box. Prefers
    /// diagnostics under the cursor column (zero-width points widened to one cell), falling
    /// back to all on the line. Reads the cached window render — no round-trip.
    pub fn show_diagnostic(&self) -> Effects {
        let cursor = self.buffer.cursor.position;
        let diags: Vec<(DiagnosticSeverity, String)> = self
            .window
            .as_ref()
            .and_then(|w| w.lines.iter().find(|l| l.logical_line == cursor.line))
            .map(|line| {
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
                message: "No diagnostics on this line".into(),
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
    /// (`include_commit_info`, docs/protocol-composites.md, G).
    pub fn show_commit_info(&mut self) -> Effects {
        self.request_str::<GitBlameLine>(
            GitBlameLineParams {
                buffer_id: self.buffer.buffer_id,
                line: self.buffer.cursor.position.line,
                include_commit_info: true,
            },
            |r| {
                Event::CommitLookup(r.map(|r| match r.blame {
                    Some(b) if b.is_uncommitted => {
                        CommitDetails::Note("Uncommitted line — no commit details")
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
    /// `from_selection` (Grep, `Space Alt-g`) tells the server to seed the query from the buffer's
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
        // "nothing can answer here" (docs/workspace-symbols.md § Scope).
        fresh.workspace_has_projects = !self.workspace_projects.is_empty();
        self.picker = Some(fresh);
        // A fresh input owns the keyboard now; anything the last one was recalling is over.
        self.history.reset();
        let buffer_id = self.buffer.buffer_id;
        let has_center_override = center_on_override.is_some();
        // Buffers / Workspaces / Explorer / LspServers all open with the highlight on "where you
        // are" — the active buffer/workspace/file/language-server — matched by item key via the
        // `effective_center_on` echo (the display-only fields below are ignored by the match).
        // Buffers: the active buffer (key is `buffer_id`).
        // Workspaces: the active workspace (key is `name`).
        // Explorer: the active buffer's filename, so the listing lands on the current file.
        // LspServers: the active buffer's own language server (key is `language` + `workspace_root`).
        let center_on = center_on_override.or(match kind {
            PickerKind::Buffers => Some(PickerItem::Buffer {
                buffer_id,
                display: String::new(),
                status: Default::default(),
                path_index: None,
                relative_path: None,
                match_indices: Vec::new(),
                transient: false,
            }),
            PickerKind::Workspaces => Some(PickerItem::Workspace {
                name: self.workspace.clone(),
                unsaved_buffers: 0,
                match_indices: Vec::new(),
            }),
            PickerKind::Explorer => self.buffer.path.as_deref().and_then(|path| {
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
                self.buffer
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
                buffer_id: (from_selection
                    || matches!(
                        kind,
                        PickerKind::Diagnostics
                            | PickerKind::References
                            | PickerKind::DocumentSymbols
                            | PickerKind::GitChangesFile
                    ))
                .then_some(buffer_id),
                from_selection,
                filters: seed_filters,
                // The binding tables live here in the client core, so a fresh Keybindings open
                // ships its rows for the server to match against.
                keybindings: (kind == PickerKind::Keybindings)
                    .then(crate::keymap::keybinding_entries),
            },
            move |__r| Event::PickerViewed {
                initial: true,
                result: __r.map_err(|e| e.to_string()),
            },
        );
        // Every open starts the list at the top. A kind that wants to land somewhere else centres
        // via the `effective_center_on` echo, which arrives with the response and reveals *after*
        // this — the same order Buffers and the Explorer have always opened in.
        Effects::one(Effect::PickerScrollReset).and(request)
    }

    /// `Space Alt-f`: open Files pre-scoped to the active buffer's directory — a normal dir filter
    /// chip, visible/editable/removable, composable with globs. Falls back to an unscoped open for
    /// scratch buffers or files outside every root. (Grep's `Space Alt-g` is the unrelated
    /// [`Session::open_grep_from_selection`].)
    pub fn open_files_in_buffer_dir(&mut self) -> Effects {
        let seed = self
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

    /// `Space Alt-g`: open Grep with the query seeded from the buffer's selection — the grep
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
        let dir = self.buffer.path.as_deref().and_then(|path| {
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
                result: __r.map_err(|e| e.to_string()),
            },
        ));
        fx
    }

    /// Tab in the Explorer: adopt the common-prefix completion ghost into the query (extending the
    /// filter part), then re-query. No-op when there's no completion to apply.
    /// `None` when the query has no completion ghost to adopt, so a caller that folds this together
    /// with another action (Explorer's Alt-l, which otherwise descends) can tell the two apart.
    fn apply_explorer_completion(&mut self) -> Option<Effects> {
        let suffix = match self.picker.as_ref().and_then(|p| p.explorer_completion()) {
            Some(s) if !s.is_empty() => s,
            _ => return None,
        };
        if let Some(p) = self.picker.as_mut() {
            p.query.push_str(&suffix);
        }
        Some(self.picker_query_changed())
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
                result: __r.map_err(|e| e.to_string()),
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
        p.selected = 0;
        p.offset = 0;
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
        if self.mode != Mode::Search || self.search.query == query {
            return Effects::none();
        }
        self.search.query = query;
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
        if let Some(s) = self.workspace_settings.as_mut() {
            if s.add.text != text {
                s.add.set(text);
                s.error = None;
            }
        }
        Effects::none()
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

    /// Fire `directory/list` for the add-project editor's current (root, dir-portion) pair. Mirrors
    /// [`Self::refresh_save_as_listing`]; the requested path rides on the result event so a stale
    /// response (the editor moved on) can be discarded.
    fn refresh_add_project_listing(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let path = self
            .workspace_settings
            .as_ref()
            .and_then(|s| s.add_project.dir_listing_path(&workspace_paths));
        let Some(path) = path else {
            return Effects::none();
        };
        let abs = path.clone();
        self.request::<DirectoryList>(DirectoryListParams { path }, move |__r| {
            Event::AddProjectListing {
                abs,
                result: __r.map_err(|e| e.to_string()),
            }
        })
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
            | PickerKind::GitChangesFile => self.picker_query_changed(),
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
                        result: __r.map_err(|e| e.to_string()),
                    },
                ))
            }
            _ => Effects::none(),
        }
    }

    /// Re-run the live query when an open glob/dir editor's in-progress value changes the
    /// effective filter set, so results update as you type (docs/picker-filters.md). A no-op
    /// when the editor leaves the effective filters unchanged (focus moves, edits that don't
    /// move the would-commit value), when a dir listing is still loading (hold — `live_filters`
    /// returns `None`), or outside the streaming kinds. Also the path back to the committed set
    /// when the editor closes: with no editor open `live_filters` is the committed `wire_filters`,
    /// so a cancel that had a preview applied reverts here.
    fn sync_live_filters(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if !matches!(
            p.kind,
            PickerKind::Grep | PickerKind::Files | PickerKind::GitChanges
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
        if !chips::filter_applies(p.kind, id) {
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
        if !chips::filter_applies(p.kind, ChipId::Glob(0)) {
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
        if !chips::filter_applies(p.kind, ChipId::Dir(0)) {
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
        // Grep / GitChanges may scope to a single file; the Files picker stays directory-only
        // (narrowing a file list to one file is degenerate).
        let allow_files = matches!(p.kind, PickerKind::Grep | PickerKind::GitChanges);
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

        self.request::<DirectoryList>(DirectoryListParams { path }, move |__r| {
            Event::PickerChipListing {
                abs,
                result: __r.map_err(|e| e.to_string()),
            }
        })
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
        // The committed field text goes to the input history (docs/input-history.md), whether or
        // not it changed the chip row: re-committing the same scope is still a use, and recording
        // the *typed* text (not the parsed scope) is what lets recall replay it verbatim.
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

    /// Alt-h / Alt-Backspace: progressively unwind — clear the query, then (explorer) pop one
    /// directory segment per press — landing the highlight on the directory just left — into roots
    /// mode in multi-root workspaces, and only then pop the rightmost filter chip. The breadcrumb
    /// sits closest to the cursor and unwinds first; chips have their own toggle bindings.
    fn picker_back(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.query.is_empty() {
            p.query.clear();
            return self.picker_query_changed();
        }
        // Explorer: unwind the breadcrumb one directory segment per press before touching chips.
        if p.kind == PickerKind::Explorer {
            match p.directory_parent.clone() {
                Some(parent) => {
                    // Pre-select the directory we're leaving in the parent's listing.
                    let leaving = p.directory.as_deref().and_then(|d| {
                        std::path::Path::new(d)
                            .file_name()
                            .and_then(|os| os.to_str())
                            .map(str::to_string)
                    });
                    return self.explorer_navigate(Some(parent), false, leaving);
                }
                // At a root with siblings: step out into Roots mode (the root name is the last
                // breadcrumb segment).
                None if p.directory.is_some() && workspace_paths.len() > 1 => {
                    return self.explorer_navigate(None, true, None);
                }
                // Single-root top, or already in Roots mode: nothing left to unwind — fall through
                // to chips.
                _ => {}
            }
        }
        if let Some(chip) = p.chip_row(&workspace_paths).last().map(|c| c.id) {
            chips::remove_chip(&mut p.chips, chip);
            p.chip_selected = None;
            return self.apply_picker_filter_change();
        }
        Effects::none()
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
        let open = match (&workspace, self.buffer.path.as_deref()) {
            (Some(_), _) | (None, None) => WindowOpen::Workspace,
            (None, Some(path)) => WindowOpen::Path {
                path: path.to_string(),
                at: None,
            },
        };
        WindowTarget { workspace, open }
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
        match p.selected_item()? {
            PickerItem::File {
                path_index,
                relative_path,
                ..
            } => Some(WindowTarget {
                workspace: here,
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
                open: WindowOpen::Path {
                    path: abs(*path_index, relative_path)?,
                    at: Some((*line, *col)),
                },
            }),
            // A file-backed buffer opens by path, like a Files row.
            PickerItem::Buffer {
                path_index: Some(pi),
                relative_path: Some(rel),
                ..
            } => Some(WindowTarget {
                workspace: here,
                open: WindowOpen::Path {
                    path: abs(*pi, rel)?,
                    at: None,
                },
            }),
            // A scratch buffer (no path) re-opens by id against the shared daemon — but only when the
            // workspace is CLI-addressable (the new `ae` must activate it before `buffer/open`-by-id).
            PickerItem::Buffer { buffer_id, .. } => here.map(|ws| WindowTarget {
                workspace: Some(ws),
                open: WindowOpen::Buffer(*buffer_id),
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
                    open: WindowOpen::Path {
                        path: format!("{}/{name}", dir.trim_end_matches('/')),
                        at: None,
                    },
                })
            }
            // Open a *different* workspace in a new window — lands on its MRU buffer.
            PickerItem::Workspace { name, .. } => Some(WindowTarget {
                workspace: Some(name.clone()),
                open: WindowOpen::Workspace,
            }),
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
        // Resolve the pick *before* closing. `picker/hide` releases the picker's state
        // server-side, and requests go out in enqueue order (docs/protocol-composites.md), so a
        // `picker/select` behind the close would have no candidate set left to resolve its item
        // against — an `invalid params` error instead of a jump. Closing second also reads right:
        // the row is resolved, then the list goes away.
        let select = self.request::<PickerSelect>(PickerSelectParams { kind, item }, move |__r| {
            Event::PickerSelected {
                result: __r.map_err(|e| e.to_string()),
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
        // Closing is what commits the query to the input history (docs/input-history.md) — grep
        // searches per keystroke, so recording on change would store `h`, `ha`, `han`… Only the
        // query the user actually settled on lands, and only if it was long enough to have run a
        // search at all. Covers accept and dismiss alike: both funnel through here.
        // The whole chip row rides along, so recalling the query later reproduces the search it
        // was — scope, match options and all.
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
            // Ctrl-d in the Buffers picker closes the highlighted row in place (no open) — a live
            // buffer or a dormant (session-restored) one alike, the server resolves which. It shares
            // the `Ctrl-d` key with the delete-file gesture above but not the kind (Buffers vs
            // Files/Explorer/Workspaces), so the two guards stay disjoint; closing a buffer just
            // drops it from the list, it doesn't delete anything on disk. The picker stays open (see
            // `picker_close_buffer`). NOT `Ctrl-x` (tempting for the `Space x` mnemonic): every GUI
            // shell's focused query input claims Ctrl-x as its native Cut and swallows it before the
            // core ever sees it — the iced forward gate in `app.rs` only forwards keys the input left
            // uncaptured, and the web `routeOverlayKey` clip filter drops Ctrl-c/v/x/a outright. Only
            // the TUI (which forwards every Ctrl chord) would see it. Ctrl-d dodges all three.
            KeyCode::Char('d') if mods.ctrl && !mods.alt && p.kind == PickerKind::Buffers => {
                let fx = self.observe_picker_cmd(PickerCmd::CloseBuffer);
                return fx.and(self.picker_close_buffer());
            }
            // Ctrl-j: capture the picker's filtered results into the jumplist and jump to
            // the highlighted row (docs/jumplist.md) — `]`/`[` then step the captured set.
            // Position-shaped kinds only (`captures_to_jumplist`). Safe on the clipboard front
            // (unlike Ctrl-c/v/x/a, GUI query inputs don't claim it) and distinct from Enter in
            // the TUI (crossterm raw mode maps the 0x0A byte to Ctrl-j, not Enter).
            KeyCode::Char('j') if mods.ctrl && !mods.alt && p.kind.captures_to_jumplist() => {
                return self.jumplist_capture();
            }
            // Up/Down recall this picker's query history (docs/input-history.md) — grep only,
            // the one kind whose query is a question you re-ask rather than a live filter over a
            // candidate set. They're free here precisely because the *list* moves on Alt-k/j, and
            // they reach the core in every shell (no text input claims a bare arrow-up).
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
            // Alt-l/h are per-kind: Explorer descends / ascends; every header-grouped kind
            // (grep / git-changes files, keybinding groups, reference sections, workspace
            // diagnostics) jumps the selection to the next / previous group's first row;
            // DocumentSymbols jumps to the next / previous top-level unit; elsewhere Alt-h
            // clears (via picker_back).
            // Alt-l is the accept/advance gesture: adopt the common-prefix completion ghost if the
            // query has one, otherwise descend into the highlighted directory. Both are "go
            // deeper", and folding them keeps Alt-l meaning "accept the suggestion" here as it does
            // in every other completing field — which is what freed Tab for field traversal.
            KeyCode::Char('l') if mods.alt && !mods.ctrl && p.kind == PickerKind::Explorer => {
                if let Some(fx) = self.apply_explorer_completion() {
                    return fx;
                }
                return self.explorer_enter_selected();
            }
            KeyCode::Char('l')
                if mods.alt
                    && !mods.ctrl
                    && (p.kind.renders_group_headers()
                        || p.kind == PickerKind::DocumentSymbols) =>
            {
                return self.picker_section_jump(Direction::Forward);
            }
            KeyCode::Char('h')
                if mods.alt
                    && !mods.ctrl
                    && (p.kind.renders_group_headers()
                        || p.kind == PickerKind::DocumentSymbols) =>
            {
                return self.picker_section_jump(Direction::Backward);
            }
            // Alt-h / Alt-Backspace unwind: clear the query first, then pop chips, then step
            // to the parent (one segment per press), then roots mode (multi-root only).
            KeyCode::Char('h') if mods.alt && !mods.ctrl => return self.picker_back(),
            KeyCode::Backspace if mods.alt && !mods.ctrl => return self.picker_back(),
            // Filter-chip chords (docs/picker-filters.md). Booleans toggle in place; valued
            // filters open the editor line. Gated per kind inside the helpers.
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
            // Up/Down recall this field's prior values (docs/input-history.md): globs and paths
            // keep separate lists — a `*.rs` is never a path — and the root typeahead has none
            // (it's a fixed candidate set, cycled with Alt-j/k). The recalled path text is
            // whatever was committed, so it replays through the same listing refresh a typed
            // value would.
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

    /// Fire `directory/list` for the save-as editor's current (root, dir-portion) pair. The
    /// requested path rides on the result event so a stale response (the editor moved on) can be
    /// discarded. No-op for an invalid root or a closed prompt.
    fn refresh_save_as_listing(&mut self) -> Effects {
        let workspace_paths = self.workspace_paths.clone();
        let path = match self.prompt.as_ref() {
            Some(Prompt::SaveAs(ed)) => ed.dir_listing_path(&workspace_paths),
            _ => None,
        };
        let Some(path) = path else {
            return Effects::none();
        };
        let abs = path.clone();
        self.request::<DirectoryList>(DirectoryListParams { path }, move |__r| {
            Event::SaveAsListing {
                abs,
                result: __r.map_err(|e| e.to_string()),
            }
        })
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
                    return Effects::error(format!("{raw} is outside the workspace's roots"));
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
        if let Some(Prompt::OpenPath(field)) = self.prompt.as_mut() {
            field.set(text);
        }
        Effects::none()
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
            },
            |r| {
                Event::WorkspaceActivated(r.and_then(|a| {
                    a.opened
                        .map(|open| (a.workspace, open))
                        .ok_or_else(|| "open_path returned no buffer".into())
                }))
            },
        )
    }

    /// Jump the open picker's selection to the next / previous section boundary — the next/prev
    /// group's first row for the header-grouped kinds, the next/prev top-level symbol for
    /// DocumentSymbols. The server finds the boundary across the *whole* result list (so it
    /// works past the over-fetch window); the result lands as [`Event::SectionJumped`].
    fn picker_section_jump(&mut self, direction: Direction) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if !(p.kind.renders_group_headers() || p.kind == PickerKind::DocumentSymbols)
            || p.items.is_empty()
        {
            return Effects::none();
        }
        let kind = p.kind;
        self.request::<PickerSectionJump>(
            PickerSectionJumpParams {
                kind,
                from_index: p.selected,
                direction,
            },
            move |__r| Event::SectionJumped(__r.map_err(|e| e.to_string())),
        )
    }

    /// Apply a server notification to the session. Stale pushes (other viewports/buffers,
    /// older picker generations) are discarded per the protocol.
    fn on_server_push(&mut self, n: Notification) -> Effects {
        match n.method.as_str() {
            ViewportLinesChanged::NAME => {
                let Ok(p) = serde_json::from_value::<ViewportLinesChangedParams>(n.params) else {
                    return Effects::none();
                };
                if Some(p.viewport_id) != self.viewport_id {
                    return Effects::none();
                }
                // The notification carries the freshly rendered window for the loaded range
                // — apply it directly, keep the revision fresh (edits that only arrive this
                // way, e.g. another client's), and keep the cursor in view under the new
                // geometry (the shell clamps + reveals).
                self.buffer.revision = p.revision;
                // Server-side cursor moves with no request in flight (e.g. the clamp a watcher
                // reload applies when the file shrank under the cursor) ride the push; adopt
                // before the shells reveal against the new window.
                if let Some(cursor) = p.cursor {
                    self.buffer.cursor = cursor;
                }
                self.window = Some(Window {
                    first_logical_line: p.range.start_logical_line,
                    last_logical_line_exclusive: p.range.end_logical_line_exclusive,
                    line_count: p.line_count,
                    max_scroll_logical_line: p.max_scroll_logical_line,
                    total_visual_rows: p.total_visual_rows,
                    first_visual_row: p.first_visual_row,
                    max_line_width: p.max_line_width,
                    git_status: p.git_status,
                    lines: p.replacement_lines,
                });
                // An in-window edit is also a change signal for the reading view.
                let read_fx = self.maybe_refresh_read(self.buffer.buffer_id, p.revision);
                Effects::one(Effect::WindowAdopted).and(read_fx)
            }
            BufferChanged::NAME => {
                // The revision-only change signal for edits outside the pushed window — the
                // reading view's cue to re-fetch (docs/markdown-view.md §3). Editor rendering
                // ignores it (the window on screen is untouched by an out-of-window edit).
                let Ok(p) = serde_json::from_value::<BufferChangedParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id == self.buffer.buffer_id {
                    self.buffer.revision = p.revision;
                }
                self.maybe_refresh_read(p.buffer_id, p.revision)
            }
            BufferState::NAME => {
                let Ok(p) = serde_json::from_value::<BufferStateParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id != self.buffer.buffer_id {
                    return Effects::none();
                }
                self.buffer.saved_revision = p.saved_revision;
                self.buffer.transient = p.transient;
                // A save-as renames the shared buffer; follow it — adopt the new path and re-derive
                // the label. Only on an actual change, so in-place save/reload pushes are no-ops
                // (and a legacy server omitting `path` never clobbers our label).
                if let Some(new_path) = p.path {
                    if self.buffer.path.as_deref() != Some(new_path.as_str()) {
                        self.buffer.label =
                            super::session::label_for_path(&new_path, &self.workspace_paths);
                        self.buffer.path = Some(new_path);
                    }
                }
                let was_external = self.externally_modified || self.externally_deleted;
                self.externally_modified = p.externally_modified;
                self.externally_deleted = p.externally_deleted;
                // Grouped per buffer: a deleted-then-modified (or repeated) disk event updates the
                // one external-change toast rather than stacking.
                let group = format!("external-change:{}", self.buffer.buffer_id);
                if !was_external && p.externally_deleted {
                    Effects::toast_grouped(
                        "File removed on disk — save to recreate, or close",
                        ToastKind::Warning,
                        group,
                    )
                } else if !was_external && p.externally_modified {
                    Effects::toast_grouped(
                        "File changed on disk — save to overwrite, or reload",
                        ToastKind::Warning,
                        group,
                    )
                } else {
                    Effects::none()
                }
            }
            LspDiagnosticsChanged::NAME => {
                if let Ok(p) = serde_json::from_value::<LspDiagnosticsChangedParams>(n.params) {
                    if p.buffer_id == self.buffer.buffer_id {
                        self.diagnostics = p.counts;
                    }
                }
                Effects::none()
            }
            PickerUpdate::NAME => {
                if let Ok(u) = serde_json::from_value::<PickerUpdateParams>(n.params) {
                    let mut reveal = None;
                    if let Some(p) = &mut self.picker {
                        // A server-resolved highlight (DocumentSymbols' cursor-enclosing symbol on
                        // the async fill) rides the push — adopt it as the pending centre + reveal,
                        // exactly like the view response's `effective_center_on`, before applying.
                        // The server frames the window around it, so adopt that offset too; without
                        // this the offset guard in `apply_update` would discard a push centred on a
                        // symbol far down the list (the bug where deep symbols never selected).
                        if let Some(center) = u.center_on.clone() {
                            p.offset = u.offset;
                            p.pending_center = Some(*center);
                            p.reveal_on_update = Some(Reveal::Minimal);
                        }
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
                    if s.buffer_id == self.buffer.buffer_id
                        && (self.search.active || self.mode == Mode::Search)
                    {
                        self.search.summary = Some(s);
                    }
                }
                Effects::none()
            }
            LspStatusChanged::NAME => {
                let Ok(s) = serde_json::from_value::<LspServerStatus>(n.params) else {
                    return Effects::none();
                };
                let matches_current = self.buffer.lsp_server.as_ref().is_some_and(|r| {
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
                                message: format!("{} restarted", s.name),
                                kind: ToastKind::Success,
                                group: Some(group.clone()),
                            })
                        }
                        LspStatus::Crashed { .. } | LspStatus::Stopped => {
                            self.lsp_restart_pending.remove(&group);
                            Some(Effect::Toast {
                                message: format!("{} failed to restart", s.name),
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
                    self.lsp = Some(s);
                }
                restart_toast.map_or_else(Effects::none, Effects::one)
            }
            BufferClosed::NAME => {
                // Another client (or a path/workspace deletion) closed a buffer; if it's ours,
                // switch to the server-indicated next buffer (or a fresh scratch).
                let Ok(p) = serde_json::from_value::<BufferClosedParams>(n.params) else {
                    return Effects::none();
                };
                // The tether closed out from under us: this client's job is over, however the
                // close happened (docs/tether.md — the future `ae --web file` waiter rides this).
                if self.tether == Some(p.buffer_id) {
                    return Effects::one(Effect::Exit);
                }
                if p.buffer_id != self.buffer.buffer_id {
                    return Effects::none();
                }
                let fx = Effects::toast("Buffer closed by another client", ToastKind::Warning);

                // In an ephemeral context, don't fall back to a fresh scratch when nothing remains
                // — leave the context, same as closing it ourselves (see `close_buffer`). This is
                // the multi-client case: another client closed the shared external file we were
                // both viewing, and there's no other buffer in this throwaway context to land on.
                if aether_protocol::is_ephemeral_workspace_id(&self.workspace)
                    && p.next_buffer_id.is_none()
                {
                    return fx.and(self.leave_ephemeral_workspace());
                }

                fx.and(self.request::<BufferOpen>(
                    BufferOpenParams {
                        buffer_id: p.next_buffer_id,
                        ..Default::default()
                    },
                    move |__r| Event::Switched(__r.map_err(|e| e.to_string())),
                ))
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
                    message: "Settings updated".to_string(),
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
    /// every picker opens at [`PickerReset::All`]. Options used to be sticky across `/`
    /// presses, but a case or regex toggle left over from an earlier search silently changes what
    /// the next one matches; `Up` recalls a past query together with the options it ran under
    /// (docs/input-history.md §4a) when you do want the old configuration back. The snapshot is
    /// taken *before* the reset, so Esc still restores a committed search exactly as it was.
    pub fn enter_search(&mut self, extend_to_cursor: bool) -> Effects {
        self.search.snapshot = Some(SearchSnapshot {
            cursor: self.buffer.cursor,
            query: std::mem::take(&mut self.search.query),
            active: self.search.active,
            options: self.search.options,
        });
        self.search.options = MatchOptions::default();
        self.search.active = false;
        self.search.summary = None;
        self.history.reset();
        self.search.chip_selected = None;
        self.search.extend_to_cursor = extend_to_cursor;
        self.mode = Mode::Search;

        let mut fx = Effects::one(Effect::SaveScrollAnchor);
        fx = fx.and(self.request::<SearchClear>(
            SearchClearParams {
                buffer_id: self.buffer.buffer_id,
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
        let buffer_id = self.buffer.buffer_id;
        if self.search.query.is_empty() {
            self.search.summary = None;

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
                query: self.search.query.clone(),
                anchor: self
                    .search
                    .snapshot
                    .as_ref()
                    .map(|s| min_pos(s.cursor.position, s.cursor.anchor)),
                extend: self.search.extend_to_cursor,
                from_selection: false,
                options: self.search.options,
            },
            move |__r| Event::SearchApplied(__r.map_err(|e| e.to_string())),
        )
    }

    /// Move the cursor back to where the prompt opened (no-op outside incremental search or
    /// when it hasn't moved).
    fn revert_to_snapshot_cursor(&mut self) -> Effects {
        let Some(snap) = self.search.snapshot.as_ref() else {
            return Effects::none();
        };
        if self.buffer.cursor.position == snap.cursor.position
            && self.buffer.cursor.anchor == snap.cursor.anchor
        {
            return Effects::none();
        }

        self.request::<CursorSet>(
            CursorSetParams {
                buffer_id: self.buffer.buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
            move |__r| Event::CursorMsg(__r.map_err(|e| e.to_string())),
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
        pos: LogicalPosition,
        granularity: Granularity,
        extend: bool,
    ) -> Effects {
        let anchor = if extend {
            self.buffer.cursor.anchor
        } else {
            pos
        };
        self.drag = Some((anchor, granularity));
        // A pointer selection is a Normal-mode concept. Double/triple-click (Word/Line) and
        // shift-click create a selection immediately, and a selection can't coexist with the
        // insert-mode bar caret: the selection's endpoint is an inclusive char, the caret is the
        // gap before it, so the two render in different places. Drop to Normal so the block cursor
        // sits on the endpoint. A plain single click stays in Insert — it only repositions the
        // caret (a point cursor, no selection).
        if self.mode == Mode::Insert && (extend || granularity != Granularity::Char) {
            self.mode = Mode::Normal;
        }
        self.request_str::<CursorSet>(
            CursorSetParams {
                buffer_id: self.buffer.buffer_id,
                position: pos,
                anchor,
                granularity,
            },
            Event::CursorMsg,
        )
    }

    /// Pointer drag to a new position while the button is held: extend the selection from the
    /// recorded anchor, preserving the press's granularity. A no-op when no press is active (the
    /// drag began outside the text, or the press was suppressed).
    pub fn pointer_drag(&mut self, pos: LogicalPosition) -> Effects {
        let Some((anchor, granularity)) = self.drag else {
            return Effects::none();
        };
        // Dragging is a selection gesture: once it covers more than the press anchor it's a real
        // selection, so leave Insert for the same reason as `pointer_press`. (Word/Line drags
        // already switched at press time; this catches the Char-granularity drag.)
        if self.mode == Mode::Insert && pos != anchor {
            self.mode = Mode::Normal;
        }
        self.request_str::<CursorSet>(
            CursorSetParams {
                buffer_id: self.buffer.buffer_id,
                position: pos,
                anchor,
                granularity,
            },
            Event::CursorMsg,
        )
    }

    /// Pointer release — ends the drag. The selection stays as last set.
    pub fn pointer_release(&mut self) {
        self.drag = None;
    }

    /// Esc in the prompt: restore the pre-prompt search (query + server state), cursor, and
    /// (via the effect) the shell's scroll anchor.
    pub fn abort_search(&mut self) -> Effects {
        self.mode = self.search_return_mode();
        self.search.extend_to_cursor = false;
        self.history.reset();
        self.search.chip_selected = None;
        let Some(snap) = self.search.snapshot.take() else {
            return Effects::none();
        };
        let buffer_id = self.buffer.buffer_id;
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
                move |__r| Event::SearchRestored(__r.map_err(|e| e.to_string())),
            )
        } else {
            self.search.summary = None;

            self.request::<SearchClear>(SearchClearParams { buffer_id }, move |__r| {
                let _ = __r;
                Event::Noop
            })
        };
        self.search.query = snap.query;
        self.search.active = snap.active;
        self.search.options = snap.options;

        fx = fx.and(self.request::<CursorSet>(
            CursorSetParams {
                buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
            move |__r| Event::CursorMsg(__r.map_err(|e| e.to_string())),
        ));
        fx.push(Effect::RestoreScrollAnchor);
        fx
    }

    /// Enter in the prompt: keep the query as the committed search. Commit is also what makes the
    /// query recallable — the incremental preview types a new query on every keystroke, so
    /// recording anything earlier would fill the history with prefixes.
    pub fn commit_search(&mut self) -> Effects {
        self.search.snapshot = None;
        let mut fx = Effects::none();
        if self.search.query.is_empty() {
            self.search.active = false;
            self.search.summary = None;
        } else {
            self.search.active = true;
            let entry = HistoryEntry::with_options(self.search.query.clone(), self.search.options);
            fx = self.record_history(HistoryKind::Search, entry);
        }
        self.history.reset();
        self.search.extend_to_cursor = false;
        self.search.chip_selected = None;
        self.mode = self.search_return_mode();
        fx
    }

    /// The mode leaving the search prompt returns to: Read when the buffer is displayed as a
    /// reading view (search entered from Read returns to Read), Normal otherwise.
    fn search_return_mode(&self) -> Mode {
        if self.read.is_some() {
            Mode::Read
        } else {
            Mode::Normal
        }
    }

    /// `n`/`Alt-n`: step match-to-match; with no active search, revive the most recent
    /// history entry first. Steps run sequentially in one future.
    pub fn search_cycle(&mut self, direction: Direction, count: u32, extend: bool) -> Effects {
        let revive = if self.search.active {
            None
        } else {
            // Revive the newest entry *with its match options* — the revived query rides the nav
            // RPC below alongside `self.search.options`, and a regex revived as a literal would
            // quietly match nothing.
            match self.history.list(HistoryKind::Search).last().cloned() {
                Some(entry) => {
                    self.search.query = entry.value.clone();
                    self.search.options = entry.filters.match_options();
                    self.search.active = true;
                    Some(entry.value)
                }
                None => return Effects::none(),
            }
        };
        // Revive + count ride the nav RPC itself (docs/protocol-composites.md, I): the
        // server re-sets the query first (skipping the step when it has no matches), then
        // steps `count` times.
        self.request_str::<SearchStep>(
            SearchStepParams {
                buffer_id: self.buffer.buffer_id,
                direction,
                extend,
                count,
                set_query: revive,
                options: self.search.options,
            },
            Event::SearchNav,
        )
    }

    /// `Alt-/`: search for the selected text, literally — the server derives and escapes
    /// the query from its own selection state (docs/protocol-composites.md, H).
    pub fn search_from_selection(&mut self) -> Effects {
        self.request_str::<SearchSet>(
            SearchSetParams {
                buffer_id: self.buffer.buffer_id,
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
        if !(self.search.active || self.search.summary.is_some()) {
            return Effects::none();
        }
        self.search.active = false;
        self.search.summary = None;

        self.request::<SearchClear>(
            SearchClearParams {
                buffer_id: self.buffer.buffer_id,
            },
            move |__r| {
                let _ = __r;
                Event::Noop
            },
        )
    }

    /// `]`/`[` (full) and `Alt-]`/`Alt-[` (`CurrentFile` scope): step through the jumplist —
    /// resolve cursor-relative (stopping, not wrapping, at the ends), open transient at the entry,
    /// record nav, all one server-side composite (docs/protocol-composites.md, J;
    /// docs/jumplist.md). The direction and scope ride into the event so a boundary result can
    /// toast the right message.
    pub fn jumplist_step(
        &mut self,
        direction: Direction,
        count: u32,
        scope: JumplistStepScope,
    ) -> Effects {
        self.request_str::<JumplistStep>(
            JumplistStepParams {
                buffer_id: self.buffer.buffer_id,
                direction,
                count,
                scope,
                open: true,
            },
            move |r| Event::JumplistStepped(r, direction, scope),
        )
    }

    /// Picker `Ctrl-j`: snapshot the picker's filtered results into the jumplist and jump
    /// to the highlighted row — capture + select in one composite (docs/jumplist.md). The
    /// picker closes like an accept; `]`/`[` then step the captured set. No-op while an async
    /// resolve is still filling the list (the snapshot would be partial).
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
                Event::JumplistCaptured(__r.map_err(|e| e.to_string()), kind)
            }),
        )
    }

    // ---- Explorer/Files create + delete --------------------------------------------------

    /// Stage a delete confirm for the highlighted picker entry: trash a Files/Explorer file or
    /// directory (`path/delete`), or forget a workspace (`workspace/delete`) from the switcher. The
    /// absolute path comes from the picker's listed directory (Explorer) or the entry's workspace
    /// root (Files). The picker stays open under the confirm; the refreshed listing arrives via a
    /// `buffer/closed` / `picker/update` push.
    pub fn picker_stage_delete(&mut self) -> Effects {
        // A highlighted workspace: the server refuses to delete the active one (the rug-pull guard),
        // so don't even stage a doomed confirm — say why and bail.
        if let Some(p) = &self.picker {
            if p.kind == PickerKind::Workspaces {
                let Some(PickerItem::Workspace { name, .. }) = p.selected_item() else {
                    return Effects::none();
                };
                if name == &self.workspace {
                    return Effects::error("Can't delete the active workspace — switch away first");
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

    /// `Ctrl-d` in the Buffers picker: close the highlighted buffer without opening it. Unsaved
    /// buffers go through a discard confirm first (mirroring the editor's own close); clean and
    /// externally-changed buffers (no in-buffer edits to lose) close straight away. The picker stays
    /// open and re-lists from the server's `picker/update` push.
    pub fn picker_close_buffer(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.kind != PickerKind::Buffers {
            return Effects::none();
        }
        let Some(PickerItem::Buffer {
            buffer_id,
            status,
            display,
            ..
        }) = p.selected_item()
        else {
            return Effects::none();
        };
        let buffer_id = *buffer_id;
        if matches!(status, BufferDirtyState::Unsaved) {
            self.prompt = Some(Prompt::Confirm {
                kind: ConfirmKind::DiscardOnClose {
                    label: display.clone(),
                },
                action: ConfirmAction::ClosePickerBuffer { buffer_id },
            });
            return Effects::none();
        }
        self.close_picker_buffer(buffer_id)
    }

    /// Fire `buffer/close` for a buffer chosen in the picker. `open_next` is set only when the
    /// closed buffer is the editor's active one — then the server attaches the viewport to the next
    /// MRU buffer (or a fresh scratch) and we adopt it; closing a background buffer leaves the editor
    /// untouched. Either way the picker stays open and re-lists from the server's refresh push (the
    /// switch doesn't tear it down — see [`Self::adopt_switch`]). Closing the
    /// [tether](Session::tether) — active or backgrounded — exits the client instead, like every
    /// other close path.
    fn close_picker_buffer(&mut self, buffer_id: BufferId) -> Effects {
        if self.tether == Some(buffer_id) {
            return self.request_str::<BufferClose>(
                BufferCloseParams {
                    buffer_id,
                    open_next: false,
                },
                |r| Event::TetherClosed(r.map(|_| ())),
            );
        }
        let closing_active = buffer_id == self.buffer.buffer_id;
        self.request_str::<BufferClose>(
            BufferCloseParams {
                buffer_id,
                open_next: closing_active,
            },
            move |r| {
                if closing_active {
                    Event::Switched(r.and_then(|closed| {
                        closed
                            .opened
                            .ok_or_else(|| "buffer/close returned no successor".into())
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
        // Buffers picker's close-and-relist but would strand the explorer over the new file).
        // Creating a *directory* instead steps into it and keeps exploring, and the
        // outside-roots refusal above keeps the explorer up so the name can be fixed.
        let Some((path_index, relative_path)) = strip_longest_root(&abs, &self.workspace_paths)
        else {
            return Effects::error("Path is outside the workspace's roots");
        };
        let from = self.buffer.buffer_id;
        let hide = self.close_picker();
        hide.and(self.request_str::<BufferOpen>(
            BufferOpenParams {
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
            return Effects::error("Workspace name can't contain path separators");
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

    /// Open the workspace-settings overlay (`Space ,`), seeded from the active workspace's name and
    /// roots. Cheap — no RPC. Focus lands on the always-present add-root input row at the bottom,
    /// since most opens (especially the post-create flow) are to add a root; the name field is
    /// above the roots and reached with Alt-k. Migrated from the TUI's `open_workspace_settings`.
    pub fn open_workspace_settings(&mut self) {
        let roots = self.workspace_paths.clone();
        let projects = self.workspace_projects.clone();
        let workspace_name = self.workspace.clone();
        self.workspace_settings = Some(WorkspaceSettings {
            workspace_name: workspace_name.clone(),
            name: TextField::new(workspace_name),
            roots,
            projects,
            selected: 0, // the workspace-name field
            add: TextField::default(),
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
            )),
            error: None,
        });
    }

    /// Keys while the workspace-settings overlay is open. Migrated from the TUI's
    /// `handle_workspace_settings_key`, made sans-IO: rename / add-root / remove-root each emit an
    /// `Effect::Request`, whose result event ([`Event::WorkspaceRenamed`] / `WorkspaceRootAdded` /
    /// `WorkspaceRootRemoved`) updates the overlay. The TUI's "commit-rename-then-advance-only-on-
    /// success" gate is simplified: Enter / blur emits the rename request and navigation is free;
    /// the result event reconciles the name (or sets the error).
    ///
    /// Selection model: index 0 is the name field, `1..=roots.len()` the root rows, and
    /// `roots.len() + 1` the add-root input row. Alt-j/k move between fields; Left/Right move the
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

        if code == KeyCode::Enter {
            match row {
                SettingsRow::Name => return self.commit_rename_if_changed(),
                SettingsRow::AddRoot => return self.commit_add_root(),
                SettingsRow::AddProject => return self.commit_add_project(),
                _ => return Effects::none(),
            }
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
        let multi_root = self.workspace_paths.len() > 1;
        let Some(s) = self.workspace_settings.as_mut() else {
            return rename;
        };
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
        let Some((workspace, path)) = self
            .workspace_settings
            .as_ref()
            .map(|s| (s.workspace_name.clone(), s.add.text.trim().to_string()))
        else {
            return Effects::none();
        };
        if path.is_empty() {
            return Effects::none();
        }
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
        // So do the input-history lists (docs/input-history.md). Empty at a boot chooser — no
        // workspace is active yet — and refetched by the switch that activates one.
        fx.and(self.fetch_history())
    }

    // ---- hints (docs/hints.md) ------------------------------------------------------

    /// The hint context the session is in right now — which curriculum pool the corner draws
    /// from. `None` means hints have nowhere to display: the boot placeholder (with no picker),
    /// confirm/info prompts, the workspace-settings overlay, or an active sneak (whose
    /// keystrokes are query input, not bindings). Precedence mirrors [`Self::dispatch_key`]'s
    /// keyboard ownership — except the picker outranks the placeholder check: the boot chooser
    /// (all shells) is the Workspaces picker over a placeholder session, and it has its own hints.
    fn hint_context(&self) -> Option<HintCtx> {
        if let Some(prompt) = &self.prompt {
            return match prompt {
                Prompt::SaveAs(_) => Some(HintCtx::SaveAs),
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
        if self.sneak.is_some() {
            return None;
        }
        match self.mode {
            Mode::Normal => Some(HintCtx::Normal),
            Mode::Insert => Some(HintCtx::Insert),
            Mode::Search => Some(HintCtx::Search),
            Mode::Read => Some(HintCtx::Read),
        }
    }

    /// The session facts that condition hint display eligibility beyond the context id — the
    /// engine is sans-IO, so it learns these only when stamped in (docs/hints.md).
    fn hint_facts(&self) -> HintFacts {
        HintFacts {
            workspaces_listed: self.picker.as_ref().and_then(|p| p.listed_workspaces()),
            mandatory_chooser: self.is_placeholder(),
            markdown_buffer: self.buffer.language.as_deref() == Some("markdown"),
            read_block_has_targets: self.read_block_has_targets(),
        }
    }

    /// Whether the reading view's focused block contains interactive elements — the fact
    /// gating the link-selection hints (`l/h`, Enter, Tab) to moments they can act.
    fn read_block_has_targets(&self) -> bool {
        let Some(read) = self.read.as_ref() else {
            return false;
        };
        let Some(idx) = read.block_focus(self.buffer.cursor.position) else {
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

    /// Open the application-settings overlay (`Space .`). Cheap — no RPC; the values it shows
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

        // Left/Right step a value row (either font size) without wrapping — a natural stepper.
        // They're inert on a toggle row (Enter/Space flips those).
        let left = code == KeyCode::Left || (mods.alt && code == KeyCode::Char('h'));
        let right = code == KeyCode::Right || (mods.alt && code == KeyCode::Char('l'));
        if left || right {
            return match self.app_setting_rows().get(selected).map(|r| r.id) {
                Some(AppSettingId::BufferFontSize) => {
                    self.set_buffer_font_size(step_font_size(self.buffer_font_size, right, false))
                }
                Some(AppSettingId::UiFontSize) => {
                    self.set_ui_font_size(step_font_size(self.ui_font_size, right, false))
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
        self.buffer_font_size = settings.buffer_font_size;
        self.ui_font_size = settings.ui_font_size;
        // Hints likewise: the engine reads the flag before observing/sampling, and the shells stop
        // rendering the corner hint when it's off.
        self.hints_enabled = settings.hints;
        // The markdown-read default only affects future opens; the current buffer's presentation
        // isn't retroactively flipped.
        self.markdown_read_default = settings.markdown_read;
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
            AppSettingId::BufferFontSize => {
                self.set_buffer_font_size(step_font_size(self.buffer_font_size, true, true))
            }
            AppSettingId::UiFontSize => {
                self.set_ui_font_size(step_font_size(self.ui_font_size, true, true))
            }
            // Hints: same flip (and toast) as `Space Alt-h`.
            AppSettingId::Hints => self.toggle_hints(),
            // Markdown reading view default: applies to future opens (`Space v` flips the
            // current buffer); flip + persist.
            AppSettingId::MarkdownRead => {
                self.markdown_read_default = !self.markdown_read_default;
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
            "Hints disabled — Space Alt-h re-enables"
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
            buffer_font_size: self.buffer_font_size,
            ui_font_size: self.ui_font_size,
            hints: self.hints_enabled,
            markdown_read: self.markdown_read_default,
        }
    }

    /// Persist the session's current app settings (`settings/set`), after a field was mutated in
    /// place. The result only reports persistence trouble — the optimistic local change already
    /// applied.
    fn persist_app_settings(&mut self) -> Effects {
        self.request_str::<SettingsSet>(self.current_app_settings(), Event::AppSettingsSaved)
    }

    /// Persist a new buffer text size + apply it (the GUI/web re-render reads
    /// `self.buffer_font_size`, re-measures its cell and reflows). No-op when unchanged. Shared by
    /// the row's activate-cycle and the Left/Right stepper.
    fn set_buffer_font_size(&mut self, font_size: u32) -> Effects {
        if font_size == self.buffer_font_size {
            return Effects::none();
        }
        self.buffer_font_size = font_size;
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

    /// Keys in the search prompt. Text entry (insert / delete / caret) is owned by each shell's
    /// search input, which syncs the whole value via [`Self::search_set_query`]; the core handles
    /// the Search command keys (commit / abort / history / option toggles) via the keymap table,
    /// plus the option-chip row gestures (mirroring [`Self::on_picker_key`]): with a chip selected,
    /// Left/Right walk the row, Backspace/Delete remove, Enter cycles, Esc/typing deselect. A
    /// forwarded Left/Backspace with no chip selected is the "step into the chips from the query
    /// start" gesture each shell sends when the caret sits at column 0.
    pub fn on_search_key(&mut self, code: KeyCode, mods: Mods, _text: Option<String>) -> Effects {
        let no_chord = !mods.ctrl && !mods.alt;
        if let Some(sel) = self.search.chip_selected {
            let chips = self.search.option_chips();
            if chips.is_empty() {
                self.search.chip_selected = None;
            } else {
                let sel = sel.min(chips.len() - 1);
                match code {
                    KeyCode::Left if no_chord => {
                        self.search.chip_selected = Some(sel.saturating_sub(1));
                        return Effects::none();
                    }
                    KeyCode::Right if no_chord => {
                        self.search.chip_selected = (sel + 1 < chips.len()).then_some(sel + 1);
                        return Effects::none();
                    }
                    KeyCode::Esc => {
                        self.search.chip_selected = None;
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
                        self.search.chip_selected = None;
                    }
                    _ => {}
                }
            }
        } else if no_chord
            && matches!(code, KeyCode::Left | KeyCode::Backspace)
            && !self.search.option_chips().is_empty()
        {
            // Forwarded from the query start: step into the chip row, selecting the rightmost.
            return self.search_select_last_chip();
        }
        match lookup(KeyContext::Search, code, mods) {
            Some(b) => {
                // Hint observation (docs/hints.md): search-mode bindings resolve here rather
                // than through `run_action`, so mirror its pre-dispatch observation — the
                // option hints (Alt-c/w/e) must follow and rotate when their chord fires.
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
        let n = self.search.option_chips().len();
        if n > 0 {
            self.search.chip_selected = Some(n - 1);
        }
        Effects::none()
    }

    /// Remove the selected option chip — reset the option it stands for to its default — and keep
    /// the selection on a neighbouring chip (or clear it when the row empties), then re-run search.
    fn remove_search_chip(&mut self, sel: usize) -> Effects {
        let Some(chip) = self.search.option_chips().get(sel).map(|c| c.id) else {
            return Effects::none();
        };
        match chip {
            ChipId::Case => self.search.options.case = CaseMode::Smart,
            ChipId::Word => self.search.options.whole_word = false,
            ChipId::Regex => self.search.options.regex = false,
            _ => {}
        }
        let remaining = self.search.option_chips().len();
        self.search.chip_selected = (remaining > 0).then(|| sel.min(remaining - 1));
        self.incremental_search()
    }

    /// Enter on the selected chip: cycle/toggle the option it stands for (case cycles
    /// smart → sensitive → insensitive → smart; word / regex flip). Keeps the selection on the
    /// same option while its chip is still present, else clamps into the row, then re-runs search.
    fn cycle_search_chip(&mut self, sel: usize) -> Effects {
        let Some(id) = self.search.option_chips().get(sel).map(|c| c.id) else {
            return Effects::none();
        };
        match id {
            ChipId::Case => self.cycle_search_case(),
            ChipId::Word => self.search.options.whole_word = !self.search.options.whole_word,
            ChipId::Regex => self.search.options.regex = !self.search.options.regex,
            _ => {}
        }
        let chips = self.search.option_chips();
        self.search.chip_selected = chips
            .iter()
            .position(|c| c.id == id)
            .or_else(|| (!chips.is_empty()).then(|| sel.min(chips.len() - 1)));
        self.incremental_search()
    }

    fn cycle_search_case(&mut self) {
        self.search.options.case = match self.search.options.case {
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
            // The Alt-chord toggles deselect any chip — they're the "chord" interaction, distinct
            // from chip-row editing.
            Action::SearchToggleCase => {
                self.search.chip_selected = None;
                self.cycle_search_case();
                self.incremental_search()
            }
            Action::SearchToggleWord => {
                self.search.chip_selected = None;
                self.search.options.whole_word = !self.search.options.whole_word;
                self.incremental_search()
            }
            Action::SearchToggleRegex => {
                self.search.chip_selected = None;
                self.search.options.regex = !self.search.options.regex;
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
        let current = HistoryEntry::with_options(self.search.query.clone(), self.search.options);
        match self.history_step(HistoryKind::Search, dir, current) {
            Some(entry) => {
                self.search.query = entry.value;
                self.search.options = entry.filters.match_options();
                self.incremental_search()
            }
            None => Effects::none(),
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) -> Effects {
        match action {
            ConfirmAction::Save { target, after } => self.save(target, true, after),
            ConfirmAction::ReloadDiscard => self.reload(true),
            ConfirmAction::CloseDiscard => self.close_buffer(),
            ConfirmAction::ClosePickerBuffer { buffer_id } => self.close_picker_buffer(buffer_id),
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
        }
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
        let mut ed = PathEditor::new(input, field, path_index);
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
                buffer_id: self.buffer.buffer_id,
                force,
            },
            move |__r| {
                Event::ReloadTried(match __r {
                    Ok(r) => Ok(ReloadTry::Reloaded(r)),
                    Err(e) if e.code == ErrorCode::WOULD_DISCARD_CHANGES.code() => {
                        Ok(ReloadTry::NeedsConfirm)
                    }
                    Err(e) => Err(e.to_string()),
                })
            },
        )
    }

    /// Twin of [`Self::on_event`] for keystrokes: drives symbol highlighting after the keystroke is
    /// handled. Cursor moves keyed here resolve asynchronously (via `CursorMsg` → `on_event`), so
    /// what this boundary uniquely catches is the *synchronous* search-clear paths — `drop_search`
    /// (Esc in Normal), `abort_search` / `commit_search` (the prompt) — which never reach `on_event`.
    pub fn on_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
        visible_rows: u32,
    ) -> Effects {
        // Every key is hint-relevant activity (the idle gate), and dispatch may have moved the
        // hint context (opened an overlay, left Insert) — re-sync so the corner follows.
        self.hints.note_input();
        let before = self.highlight_trigger_state();
        let fx = self.dispatch_key(code, mods, text, visible_rows);
        let fx = fx.and(self.sync_hint_context());
        self.after_step_highlight(fx, before)
    }

    fn dispatch_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
        visible_rows: u32,
    ) -> Effects {
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
        if self.mode == Mode::Search {
            let fx = self.on_search_key(code, mods, text);
            return fx;
        }

        // An active sneak session owns the keyboard: keystrokes refine the query or pick a label.
        if self.sneak.is_some() {
            return self.on_sneak_key(code, mods, text);
        }

        // Stateful captures run before table lookup, like the TUI.
        match self.pending {
            Pending::Find {
                dir,
                till,
                extend,
                count,
            } => {
                self.pending = Pending::None;
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
                self.pending = Pending::None;
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(delimiter) = ch.filter(|c| !c.is_control()) else {
                    return Effects::none(); // Esc / non-char cancels
                };
                return self.edit::<InputSurround>(InputSurroundParams {
                    buffer_id: self.buffer.buffer_id,
                    delimiter,
                    target,
                });
            }
            Pending::Transform => {
                self.pending = Pending::None;
                // The next key picks the transform; an unmapped key (or Esc) just cancels.
                let kind = text
                    .as_deref()
                    .and_then(|t| t.chars().next())
                    .and_then(CaseKind::from_char);
                let Some(kind) = kind else {
                    return Effects::none();
                };
                return self.edit::<InputTransformCase>(InputTransformCaseParams {
                    buffer_id: self.buffer.buffer_id,
                    kind,
                    // Insert mode has no selection, so the server scans for the identifier
                    // under the caret; Normal mode recases exactly the selection (a point
                    // being the single char under the block).
                    scan_at_cursor: self.mode == Mode::Insert,
                });
            }
            Pending::Leader => {
                self.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::Leader, code, mods) {
                    return self.run_action(b.action, 1, mods.shift, visible_rows);
                }
                return Effects::none();
            }
            Pending::None => {}
        }

        // Count lexer (Normal and Read modes): digits accumulate; `0` only continues a count
        // (it's line-start otherwise).
        if matches!(self.mode, Mode::Normal | Mode::Read) && !mods.ctrl && !mods.alt {
            if let KeyCode::Char(c) = code {
                if c.is_ascii_digit() && (c != '0' || self.count.is_some()) {
                    let d = c.to_digit(10).unwrap();
                    self.count = Some(self.count.unwrap_or(0).saturating_mul(10) + d);
                    return Effects::none();
                }
            }
        }
        let count = self.count.take().unwrap_or(1).max(1);
        // Insert mode never holds a selection, so Shift+motion must not extend one (the arrow
        // bindings match any modifier). It just moves the caret.
        let extend = mods.shift && self.mode != Mode::Insert;

        // Global table first (mode-identical Ctrl shortcuts), then the mode's own. Read mode
        // skips Global entirely — its chords are edits (undo, indent, move lines), and the
        // reading view is read-only by construction (docs/markdown-view.md §1.4).
        let ctx = match self.mode {
            Mode::Normal => KeyContext::Normal,
            Mode::Insert => KeyContext::Insert,
            Mode::Read => KeyContext::Read,
            Mode::Search => return Effects::none(), // handled above
        };
        let global = if self.mode == Mode::Read {
            None
        } else {
            lookup(KeyContext::Global, code, mods)
        };
        if let Some(b) = global.or_else(|| lookup(ctx, code, mods)) {
            return self.run_action(b.action, count, extend, visible_rows);
        }

        // Insert mode: unmatched printable input is text.
        if self.mode == Mode::Insert && !mods.ctrl && !mods.alt {
            if let Some(typed) = text {
                let typed: String = typed
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\t')
                    .collect();
                if !typed.is_empty() {
                    return self.edit::<InputText>(InputTextParams {
                        buffer_id: self.buffer.buffer_id,
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
        extend: bool,
        visible_rows: u32,
    ) -> Effects {
        // Hint observation (docs/hints.md): every resolved binding passes through here — except
        // search-mode keys, which resolve in `on_search_key` and observe there. Observed (and
        // its record requests emitted) *before* dispatch, so the context is the one the hint
        // displayed in (dispatch may open a picker and move it) — and so a `Quit`'s follow
        // record hits the wire ahead of `Effect::Exit` tearing the process down, rather than
        // queuing behind it and being lost.
        let hint_ctx = self.hint_env();
        let enabled = self.hints_enabled;
        // `Space h` owns its hint learning inside the engine's `dismiss()` — observing it here
        // would rotate a followed intro hint before the dismissal ran, dismissing its
        // replacement instead.
        let evs = if matches!(action, Action::DismissHint) {
            Vec::new()
        } else {
            self.hints.observe_action(&action, hint_ctx, enabled)
        };
        let hint_fx = self.emit_hint_events(evs);
        let task = self.dispatch_action(action, count, extend, visible_rows);
        // Remember the action for `.` to replay. Recorded at dispatch (the RPC is still in flight —
        // a failed motion just leaves a harmless no-op target). `RepeatMotion` itself isn't
        // repeatable, so it never overwrites the target with itself; find records its resolved
        // motion at the capture site instead.
        if action.is_repeatable() {
            self.last_repeat = Some(RepeatTarget::Action { action, count });
        }
        hint_fx.and(task)
    }

    fn dispatch_action(
        &mut self,
        action: Action,
        count: u32,
        extend: bool,
        visible_rows: u32,
    ) -> Effects {
        use Action as A;
        let buffer_id = self.buffer.buffer_id;
        // While disconnected (boot `Connecting` or a mid-session `Reconnecting`) the buffer is
        // read-only: the server can't accept edits, so the RPCs are dropped anyway. Entering Insert
        // would leave the user in a mode where typing silently vanishes — it reads as a hang. Refuse
        // the insert-entering actions and stay in Normal, with a hint so the inaction is explained.
        if self.conn != ConnState::Connected
            && matches!(
                action,
                A::EnterInsert(_) | A::OpenLineBelow | A::OpenLineAbove | A::Change | A::CutChange
            )
        {
            // Grouped: each blocked keystroke while disconnected refreshes one hint, not a stack.
            return Effects::toast_grouped(
                "Not connected — editing unavailable",
                ToastKind::Info,
                "edit-blocked",
            );
        }
        match action {
            // ---- motions ----
            A::MoveChar(direction) => self.move_motion(Motion::Char { direction, count }, extend),
            // `b` / `Alt-b` — `w` now selects words (`CursorSelectWord`), so the only word *motion*
            // left in the keymap is the backward one.
            A::MoveWordBack { boundary } => self.move_motion(
                Motion::Word {
                    direction: Direction::Backward,
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
            A::MoveVisualLine(direction) => {
                let Some(viewport_id) = self.viewport_id else {
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
                let line = if last {
                    // `Alt-g` counts from the bottom: bare (count 1) → last line, `N Alt-g` → the
                    // N-th line up from the end. `g`/`Alt-g` are thus mirror absolute jumps.
                    self.window
                        .as_ref()
                        .map(|w| w.line_count.saturating_sub(count))
                        .unwrap_or(0)
                } else {
                    count.saturating_sub(1)
                };
                self.move_jump(
                    Motion::Goto {
                        position: LogicalPosition { line, col: 0 },
                    },
                    extend,
                )
            }
            A::MatchBracket { inner } => self.move_motion(Motion::MatchBracket { inner }, extend),
            A::PageMotion { dir, half } => {
                let Some(viewport_id) = self.viewport_id else {
                    return Effects::none();
                };
                let rows = visible_rows;
                let span = if half { (rows / 2).max(1) } else { rows.max(1) };
                self.move_motion(
                    Motion::VisualLine {
                        viewport_id,
                        direction: dir,
                        count: count.saturating_mul(span),
                    },
                    extend,
                )
            }
            A::NavUnit(Direction::Forward) => {
                self.move_motion(Motion::NextNavigationUnit { count }, extend)
            }
            A::NavUnit(Direction::Backward) => {
                self.move_motion(Motion::PrevNavigationUnit { count }, extend)
            }
            A::BeginFind { dir, till } => {
                self.pending = Pending::Find {
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
                self.sneak = Some(SneakState {
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
                if self.buffer.cursor.is_point() {
                    return Effects::none();
                }
                let pos = self.buffer.cursor.position;
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
                        RepeatTarget::Action { action, count } => {
                            self.dispatch_action(*action, *count, extend, visible_rows)
                        }
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
                self.open_workspace_settings();
                Effects::none()
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
                    Event::AppInfoLoaded(r.map_err(|e| e.to_string()))
                })
            }
            // Dismiss the corner hint (docs/hints.md): a deliberate "not now" — down-weight it
            // (heavier than a lapsed display) and rotate to another. No-op on an empty corner.
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
                    result: res.map_err(|e| e.to_string()),
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
                self.mode = Mode::Insert;
                self.enter_insert_at(where_)
            }
            A::LeaveInsert => {
                self.mode = Mode::Normal;
                Effects::none()
            }
            A::BeginLeader => {
                self.pending = Pending::Leader;
                Effects::none()
            }

            // ---- edits ----
            A::Backspace => self.edit::<InputBackspace>(BufferOnlyParams { buffer_id }),
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
            A::IncrementNumber => self.edit::<InputAdjustNumber>(InputAdjustNumberParams {
                buffer_id,
                delta: count as i32,
                scan_at_cursor: self.mode == Mode::Insert,
            }),
            A::DecrementNumber => self.edit::<InputAdjustNumber>(InputAdjustNumberParams {
                buffer_id,
                delta: -(count as i32),
                scan_at_cursor: self.mode == Mode::Insert,
            }),
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
                self.mode = Mode::Insert;
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
                self.mode = Mode::Insert;
                self.cut(CopyScope::Selection)
            }
            A::CutLine => self.cut(CopyScope::Line),
            A::Paste => read_clipboard_fx(PasteKind::Before { count }),
            A::ReplaceClipboard => read_clipboard_fx(PasteKind::Replace { count }),
            A::PasteAtCursor => read_clipboard_fx(PasteKind::AtCursor),
            A::ReplaceLineClipboard => read_clipboard_fx(PasteKind::Line),
            A::Change => {
                self.mode = Mode::Insert;
                self.edit::<InputChange>(CountedEditParams {
                    buffer_id,
                    count: 1,
                })
            }
            A::ChangeLine => self.edit::<InputChangeLine>(BufferOnlyParams { buffer_id }),
            A::BeginSurround(target) => {
                self.pending = Pending::Surround(target);
                Effects::none()
            }
            A::Unsurround(target) => {
                self.edit::<InputUnsurround>(InputUnsurroundParams { buffer_id, target })
            }
            A::BeginTransform => {
                self.pending = Pending::Transform;
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
            | A::SearchToggleRegex => self.search_action(action),
            A::SearchCycle(direction) => self.search_cycle(direction, count, extend),
            A::SearchFromSelection => self.search_from_selection(),
            A::JumplistStep(direction) => {
                self.jumplist_step(direction, count, JumplistStepScope::Full)
            }
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
            A::SaveAndClose => self.save(None, false, AfterSave::Close),
            A::SaveAs => {
                // Prefill with the buffer's current workspace-relative path, like the web dialog.
                let (path_index, input) = self
                    .buffer
                    .path
                    .as_deref()
                    .and_then(|p| strip_longest_root(p, &self.workspace_paths))
                    .unwrap_or((0, String::new()));
                self.open_save_as(path_index, input)
            }
            A::OpenPath => {
                // Open the workspace-agnostic path overlay (empty field). The shell focuses it and
                // syncs typed text via `open_path_set_input`; Enter opens via `workspace/open_path`.
                self.prompt = Some(Prompt::OpenPath(crate::session::TextField::new(
                    String::new(),
                )));
                Effects::none()
            }
            A::Reload => {
                if self.buffer.path.is_none() {
                    return Effects::toast(
                        "Scratch buffer has no path to reload",
                        ToastKind::Warning,
                    );
                }
                self.reload(false)
            }
            A::ToggleKeep => {
                // Un-keeping the tether *releases* it (docs/tether.md): the buffer demotes to an
                // ordinary transient AND the client stops exiting when it closes — one-way; a
                // re-keep is just a plain keep. Atomic with the demotion, so it inherits the
                // dirty guard — but audibly, since the user asked for a release.
                if self.tethered() {
                    if self.buffer.revision != self.buffer.saved_revision {
                        return Effects::toast(
                            "Unsaved changes — save before releasing",
                            ToastKind::Warning,
                        );
                    }
                    return self.request_str::<BufferSetTransient>(
                        BufferSetTransientParams {
                            buffer_id,
                            transient: true,
                        },
                        |r| Event::TetherReleased(r.map(|res| res.transient)),
                    );
                }
                let target = !self.buffer.transient;
                // Refuse to make a buffer with unsaved edits transient — it would auto-close (and
                // discard them) once hidden. Silent no-op; pinning permanent, or toggling a clean
                // buffer, is fine.
                if target && self.buffer.revision != self.buffer.saved_revision {
                    return Effects::none();
                }
                self.request_str::<BufferSetTransient>(
                    BufferSetTransientParams {
                        buffer_id,
                        transient: target,
                    },
                    |r| Event::KeepToggled(r.map(|res| res.transient)),
                )
            }
            A::CopyRelativePath => self.copy_buffer_path(false),
            A::CopyAbsolutePath => self.copy_buffer_path(true),
            A::NewScratch => {
                // Opening a fresh scratch is a buffer switch — record the origin so Alt-Left
                // returns (folded into the open's `record_nav_from`).
                self.request_str::<BufferOpen>(
                    BufferOpenParams {
                        record_nav_from: Some(buffer_id),
                        ..Default::default()
                    },
                    Event::Switched,
                )
            }
            A::CloseBuffer => {
                if self.buffer.revision != self.buffer.saved_revision {
                    self.prompt = Some(Prompt::Confirm {
                        kind: ConfirmKind::DiscardOnClose {
                            label: self.buffer.label.clone(),
                        },
                        action: ConfirmAction::CloseDiscard,
                    });
                    return Effects::none();
                }

                self.close_buffer()
            }
            // Spawning the new process is irreducibly shell-side (and GUI-only) — the shell reads
            // the workspace/path from its session and detaches a sibling `ae --gui`. The core just
            // asks for it; non-GUI shells ignore the action.
            A::NewWindow => Effects::one(Effect::ShellAction(ShellAction::NewWindow(
                self.current_view_target(),
            ))),

            // ---- git ----
            A::ToggleDiffView => {
                let Some(viewport_id) = self.viewport_id else {
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
            A::NextHunk | A::PrevHunk => {
                let direction = if matches!(action, A::NextHunk) {
                    HunkDirection::Next
                } else {
                    HunkDirection::Prev
                };
                self.request_str::<GitNavigateHunk>(
                    GitNavigateHunkParams {
                        buffer_id,
                        from_line: self.buffer.cursor.position.line,
                        direction,
                        count,
                        extend,
                    },
                    Event::HunkNav,
                )
            }
            A::ToggleStageHunk | A::RevertHunk => {
                let hunk_action = if matches!(action, A::ToggleStageHunk) {
                    HunkAction::Toggle
                } else {
                    HunkAction::Revert
                };
                self.request_str::<GitApplyHunk>(
                    GitApplyHunkParams {
                        buffer_id,
                        action: hunk_action,
                    },
                    move |result| Event::HunkApplied {
                        action: hunk_action,
                        result,
                    },
                )
            }

            // ---- pickers ----
            A::OpenPicker(PickerKind::Explorer) => self.open_explorer(false),
            A::OpenPicker(kind) => self.open_picker(kind, None, None, false, None),
            A::OpenFilesInBufferDir => self.open_files_in_buffer_dir(),
            A::OpenGrepFromSelection => self.open_grep_from_selection(),
            A::OpenExplorerAtRoot => self.open_explorer(true),

            // ---- LSP ----
            A::GotoDefinition => self
                .request_str::<LspGotoDefinition>(LspBufferParams { buffer_id }, Event::Definition),
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

            // ---- markdown reading view (docs/markdown-view.md) ----
            A::ToggleReadView => self.toggle_read_view(),
            A::ReadStep(dir) => self.read_step(
                dir == Direction::Forward,
                count,
                crate::markdown::Element::is_block,
            ),
            A::ReadStepLink(dir) => self.read_step_link_in_block(dir == Direction::Forward, count),
            A::ReadShowTarget => self.read_show_target(),
            A::ReadStepHeading(dir) => self.read_step(dir == Direction::Forward, count, |e| {
                matches!(e, crate::markdown::Element::Heading { .. })
            }),
            A::ReadEnds { last } => self.read_ends(last),
            A::ReadActivate => self.read_activate(),
            A::ReadActivateNewWindow => self.read_activate_new_window(),
            A::ReadCopy => self.read_copy(),
        }
    }

    /// Toggle the reading view on the current buffer (`Space v`). The choice is remembered per
    /// buffer for the session; non-markdown buffers toast instead.
    fn toggle_read_view(&mut self) -> Effects {
        if self.read.is_some() {
            self.read_pref.insert(self.buffer.buffer_id, false);
            self.read = None;
            if self.mode == Mode::Read {
                self.mode = Mode::Normal;
            }
            // The editor window stayed subscribed throughout — just frame the reading position.
            return Effects::one(Effect::RevealCursor(RevealStyle::Jump));
        }
        if self.buffer.language.as_deref() != Some("markdown") {
            return Effects::toast_grouped(
                "Reading view is for Markdown buffers",
                ToastKind::Info,
                "read-view",
            );
        }
        self.read_pref.insert(self.buffer.buffer_id, true);
        self.begin_read()
    }

    /// Step the reading focus (`j`/`k`, `Tab`, `o` — the predicate picks the element class) and
    /// move the server cursor to the landed element's start: focus is derived from the cursor, so
    /// the `Goto` *is* the focus change (docs/markdown-view.md §1.3). Quiet no-op at the ends.
    fn read_step(
        &mut self,
        forward: bool,
        count: u32,
        pred: impl Fn(&crate::markdown::Element) -> bool,
    ) -> Effects {
        let target = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let Some(mut idx) = read.focus(self.buffer.cursor.position) else {
                return Effects::none();
            };
            // Stepping is class-relative: when the derived focus doesn't match the predicate (a
            // focused link while stepping blocks), anchor at the innermost *matching* element
            // containing the cursor. Without this a lone-link paragraph traps `k`: the Goto to
            // the paragraph start re-derives focus to the link (innermost at that byte), and
            // stepping back from the link finds its own containing paragraph, forever.
            if !pred(&read.elements[idx]) {
                let byte = read.byte_of(self.buffer.cursor.position);
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
            // Land outside any leading interactive span, so the step selects the block alone
            // (the bar) — `l` opts into its links (docs/markdown-view.md §2.3).
            read.pos_of(crate::markdown::block_rest_byte(&read.elements, idx))
        };
        self.move_motion(Motion::Goto { position: target }, false)
    }

    /// `h`/`l`: step the Enter target among the interactive elements *inside the focused
    /// block* (docs/markdown-view.md §2.3). `l` with no target selects the block's first
    /// interactive; `h` from the first steps back *out* — the cursor returns to the block's
    /// rest byte, so the bar stands alone again. Past the last link, and `h` with nothing
    /// selected, are quiet no-ops, like `j`/`k` at the document's ends.
    fn read_step_link_in_block(&mut self, forward: bool, count: u32) -> Effects {
        let target = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let cursor = self.buffer.cursor.position;
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
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.buffer.cursor.position) else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Element::Link { href, .. } => href.clone(),
                crate::markdown::Element::Image { src, .. } => src.clone(),
                crate::markdown::Element::FootnoteRef { label, .. } => {
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

    /// A pointer press on the reading view: the shell hit-tests its own rendering to a source
    /// byte (an element's span start) and the core moves the server cursor there — focus then
    /// derives from the cursor exactly like a keyboard step (docs/markdown-view.md §1.3), so
    /// clicking a block sets the reading selection in every shell through one path.
    pub fn read_click(&mut self, byte: u32) -> Effects {
        let target = {
            let Some(read) = self.read.as_ref() else {
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
        let follow = self.read.as_ref().and_then(|read| {
            read.elements.iter().position(|e| {
                e.span().start == byte
                    && matches!(
                        e,
                        crate::markdown::Element::Link { .. }
                            | crate::markdown::Element::FootnoteRef { .. }
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
        let href = self.read.as_ref().and_then(|read| {
            read.elements.iter().find_map(|e| match e {
                crate::markdown::Element::Link { span, href }
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
            let Some(read) = self.read.as_ref() else {
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
    /// footnote's definition. No-op on non-interactive blocks (docs/markdown-view.md §2.3).
    fn read_activate(&mut self) -> Effects {
        let idx = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.buffer.cursor.position) else {
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
        }
        let act = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Element::Link { href, .. } => Act::Link(href.clone()),
                crate::markdown::Element::Image { src, .. } => Act::Image(src.clone()),
                crate::markdown::Element::FootnoteRef { label, .. } => {
                    let Some(span) = crate::markdown::footnote_def_span(&read.blocks, label) else {
                        return Effects::toast_grouped(
                            format!("No definition for footnote [{label}]"),
                            ToastKind::Warning,
                            "read-view",
                        );
                    };
                    Act::Footnote(read.pos_of(span.start))
                }
                _ => return Effects::none(),
            }
        };
        match act {
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
                                buffer_id: self.buffer.buffer_id,
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
                            buffer_id: self.buffer.buffer_id,
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
    /// re-open the current buffer with `record_nav_from` + `jump_to`, the same `buffer/open`
    /// composite cross-file follows and goto-definition ride, so `Backspace` returns. The
    /// same-buffer open is "a move, not a switch": nothing is discarded client- or
    /// server-side (see `adopt_navigation` and the handler's already-open branch), and jumps
    /// deliberately don't feed the motion history — `z` stays the undo for *motions*
    /// (j/k/o/g/v/search), `Backspace` the way back from *jumps*, in both modes. A pathless
    /// (scratch) buffer can't re-open itself: plain Goto, unrecorded.
    fn read_jump_recorded(&mut self, target: LogicalPosition) -> Effects {
        match self.buffer.path.clone() {
            Some(path) => self.open_path_at(path, Some(target), None),
            None => self.move_motion(Motion::Goto { position: target }, false),
        }
    }

    /// `Ctrl-Enter`: the picker's open-in-new-window at reading grain — a *relative-path*
    /// link opens in a new window (GUI) / app tab (web) via [`ShellAction::NewWindow`];
    /// everything else (external links, anchors, images, plain blocks) behaves like `Enter`.
    fn read_activate_new_window(&mut self) -> Effects {
        let href = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.buffer.cursor.position) else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Element::Link { href, .. }
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
            open: WindowOpen::Path { path, at: None },
        })))
    }

    /// Follow a link target from the reading view (docs/markdown-view.md §2.4).
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
                let Some(read) = self.read.as_ref() else {
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
                // Set *after* the open — `open_path_at` clears any stale anchor at entry.
                let fx = self.open_path_at(path, None, None);
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

    /// The deferred half of a *cross-file* anchor at content adoption: resolve the slug
    /// against the freshly-staged parse, send the Goto, and hold the parse back — the
    /// visible view stays "Loading…" for the one `cursor/move` round-trip, and
    /// [`Self::install_staged_read`] swaps it in when the cursor lands, so the document
    /// paints exactly once, already in place (docs/markdown-view.md §2.4 — the editor's
    /// paint-once property for cross-file goto-def). A slug with no match installs
    /// immediately with the in-document branch's toast: nothing to place, and the hold must
    /// never outlive its reason.
    fn stage_read_place(&mut self, staged: ReadView) -> Effects {
        let Some(slug) = self.pending_read_anchor.take() else {
            return Effects::none();
        };
        let Some(read) = self.read.as_mut() else {
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
        let Some(read) = self.read.as_mut() else {
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
            let Some(read) = self.read.as_ref() else {
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
    /// **workspace-root-relative** (GitHub semantics, docs/markdown-view.md §2.4): it joins
    /// the root containing the buffer (longest match, like every root computation). A buffer
    /// outside every root has no anchor, so such a target keeps its filesystem-absolute
    /// reading — the only sensible meaning there. Relative targets join the buffer's
    /// directory; `None` for a scratch buffer with a relative target. Callers scheme-check
    /// first — a URL joined onto either base is never meaningful. `pub`: the iced shell
    /// resolves image sources through this, so links and images can't drift.
    pub fn read_resolve_path(&self, target: &str) -> Option<String> {
        if target.starts_with('/') {
            let root = self
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
        let parent = std::path::Path::new(self.buffer.path.as_deref()?).parent()?;
        Some(parent.join(target).to_string_lossy().into_owned())
    }

    /// `y`: copy the focused element — a link's URL, otherwise its markdown source.
    fn read_copy(&mut self) -> Effects {
        let (text, what) = {
            let Some(read) = self.read.as_ref() else {
                return Effects::none();
            };
            let Some(idx) = read.focus(self.buffer.cursor.position) else {
                return Effects::none();
            };
            match &read.elements[idx] {
                crate::markdown::Element::Link { href, .. } => (href.clone(), "link URL"),
                el => (
                    read.slice(el.span()).trim_end().to_string(),
                    "element source",
                ),
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
                buffer_id: self.buffer.buffer_id,
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
            let Some(sneak) = self.sneak.as_mut() else {
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
        if self.sneak.as_ref().is_some_and(|s| s.labels.contains(&ch)) {
            return self.sneak_select(ch);
        }
        let Some(sneak) = self.sneak.as_mut() else {
            return Effects::none();
        };
        sneak.query.push(ch);
        let query = sneak.query.clone();
        self.sneak_update(query)
    }

    /// Push the current query to the server, which recomputes labels and refreshes the viewport.
    fn sneak_update(&mut self, query: String) -> Effects {
        let Some(viewport_id) = self.viewport_id else {
            return Effects::none();
        };
        let big = self.sneak.as_ref().is_some_and(|s| s.big);
        // Scope to what's actually on screen (reported by the shell). Fall back to the loaded
        // window's range until the shell has reported a scroll position.
        let (first_line, last_line) = self
            .visible_lines
            .or_else(|| {
                self.window
                    .as_ref()
                    .map(|w| (w.first_logical_line, w.last_logical_line_exclusive))
            })
            .unwrap_or((0, 0));
        self.request_str::<SneakUpdate>(
            SneakUpdateParams {
                buffer_id: self.buffer.buffer_id,
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
        let extend = self.sneak.as_ref().is_some_and(|s| s.extend);
        self.sneak = None;
        self.request_str::<SneakSelect>(
            SneakSelectParams {
                buffer_id: self.buffer.buffer_id,
                label,
                extend,
            },
            Event::CursorMsg,
        )
    }

    /// Abandon the session (Esc). The cursor never moved; just clear the labels server-side.
    fn sneak_cancel(&mut self) -> Effects {
        self.sneak = None;
        self.request::<SneakCancel>(
            SneakCancelParams {
                buffer_id: self.buffer.buffer_id,
            },
            |_r| Event::Noop,
        )
    }

    /// Like [`move_motion`](Self::move_motion) but reveals the landing as a jump (go-to-line) —
    /// a targeted destination, so the cursor rests a quarter down instead of minimal-scrolling.
    fn move_jump(&mut self, motion: Motion, extend: bool) -> Effects {
        self.request_str::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.buffer.buffer_id,
                motion,
                extend_selection: extend,
            },
            Event::CursorJump,
        )
    }

    /// A counted edit (`3J`, `3>`, …) — the repeat loop lives server-side
    /// (docs/protocol-composites.md, K).
    fn repeat_edit<M>(&mut self, count: u32) -> Effects
    where
        M: RpcMethod<Params = CountedEditParams, Result = EditResult> + 'static,
    {
        self.edit::<M>(CountedEditParams {
            buffer_id: self.buffer.buffer_id,
            count,
        })
    }

    /// Counted tree expand/contract — repeats server-side, stopping when the cursor stops
    /// changing.
    fn tree_select(&mut self, direction: TreeSelectDirection, count: u32) -> Effects {
        self.request_str::<CursorTreeSelect>(
            CursorTreeSelectParams {
                buffer_id: self.buffer.buffer_id,
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
                buffer_id: self.buffer.buffer_id,
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
                buffer_id: self.buffer.buffer_id,
                count,
                // Insert mode forbids selections — drop the one undo would otherwise restore.
                collapse_selection: self.mode == Mode::Insert,
            },
            Event::UndoRedoDone,
        )
    }

    /// `i`/`a`/`Alt-i`/`Alt-a` — collapse to the chosen selection edge. One RPC: the
    /// server owns the selection, so it resolves the edge (`Motion::SelectionEdge`,
    /// docs/protocol-composites.md change F — formerly a set-cursor-then-adjust chain).
    fn enter_insert_at(&mut self, where_: InsertWhere) -> Effects {
        let edge = match where_ {
            InsertWhere::SelectionStart => SelectionEdge::Start,
            InsertWhere::SelectionEnd => SelectionEdge::AfterEnd,
            InsertWhere::FirstLineStart => SelectionEdge::FirstLineNonblank,
            InsertWhere::LastLineEnd => SelectionEdge::LastLineEnd,
        };
        self.request_str::<CursorMove>(
            CursorMoveParams {
                buffer_id: self.buffer.buffer_id,
                motion: Motion::SelectionEdge { edge },
                extend_selection: false,
            },
            Event::CursorMsg,
        )
    }

    fn copy(&mut self, scope: CopyScope) -> Effects {
        self.request_str::<BufferCopy>(
            BufferCopyParams {
                buffer_id: self.buffer.buffer_id,
                scope,
            },
            Event::CopyDone,
        )
    }

    fn cut(&mut self, scope: CopyScope) -> Effects {
        self.request_str::<BufferCut>(
            BufferCopyParams {
                buffer_id: self.buffer.buffer_id,
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

/// The toast to show when a cursor-relative LSP request (hover / goto-definition) couldn't run
/// because the server wasn't ready — `None` once a ready server has answered, so the caller falls
/// back to its own "nothing here" message ("No hover info" / "No definition found").
fn lsp_readiness_message(readiness: LspReadiness) -> Option<&'static str> {
    match readiness {
        LspReadiness::Ready => None,
        LspReadiness::NoServer => Some("No language server for this buffer"),
        LspReadiness::Starting => Some("Language server still starting"),
        LspReadiness::Unavailable => Some("Language server unavailable"),
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
        s.buffer.path = Some("/proj/docs/a.md".into());
        s.mode = Mode::Read;
        s.read = Some(ReadView::loading(s.buffer.buffer_id));
        s
    }

    /// Cross-file anchors (docs/markdown-view.md §2.4): following `[x](./other.md#section)`
    /// opens the file and arms the fragment; the anchor lands as a `cursor/move` Goto once
    /// the target document's reading view adopts.
    #[test]
    fn cross_file_anchor_lands_after_target_adopts() {
        let mut s = reading_session();
        let fx = s.read_follow_link("./other.md#section-two");
        assert!(
            fx.0.iter()
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "buffer/open")),
            "the link target opens"
        );
        assert_eq!(s.pending_read_anchor.as_deref(), Some("section-two"));

        // The switch lands: the new buffer's reading view fetches and adopts its content.
        s.buffer.buffer_id += 1;
        s.read = Some(ReadView::loading(s.buffer.buffer_id));
        let fx = s.on_event(Event::ReadContent(Ok(BufferContentResult {
            revision: 0,
            text: "# One\n\ntext\n\n## Section Two\n\nbody\n".into(),
        })));
        assert_eq!(s.pending_read_anchor, None, "the anchor is consumed");
        // The parse is *staged*, not installed: the visible view stays "Loading…" for the
        // cursor round-trip, so the document paints exactly once, already in place (§2.4).
        let read = s.read.as_ref().unwrap();
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
                    Effect::Request { method, params, .. } if *method == "cursor/move" => {
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
        let read = s.read.as_ref().unwrap();
        assert!(
            !read.loading && !read.blocks.is_empty(),
            "installed on landing"
        );
        assert!(read.staged.is_none());
        assert_eq!(s.buffer.cursor.position, expected);
    }

    /// A fragment naming no heading in the target document warns (same toast as the
    /// in-document branch) instead of moving the cursor.
    #[test]
    fn cross_file_anchor_missing_heading_warns() {
        let mut s = reading_session();
        s.read_follow_link("./other.md#nope");
        assert_eq!(s.pending_read_anchor.as_deref(), Some("nope"));
        s.buffer.buffer_id += 1;
        s.read = Some(ReadView::loading(s.buffer.buffer_id));
        let fx = s.on_event(Event::ReadContent(Ok(BufferContentResult {
            revision: 0,
            text: "# Only Heading\n".into(),
        })));
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
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "cursor/move")),
            "…and moves nothing"
        );
        // Nothing to place, so no hold: the document installs immediately.
        let read = s.read.as_ref().unwrap();
        assert!(!read.loading && !read.blocks.is_empty() && read.staged.is_none());
    }

    /// In-document anchors are *jumps*, not motions (docs/markdown-view.md §2.4): following
    /// `#section` re-opens the current buffer with `record_nav_from` + `jump_to` — the same
    /// composite cross-file follows and goto-definition ride — so `Backspace` returns.
    #[test]
    fn in_document_anchor_follow_is_nav_recorded() {
        let mut s = reading_session();
        s.read
            .as_mut()
            .unwrap()
            .adopt(0, "# One\n\ntext\n\n## Section Two\n\nbody\n".into());
        let fx = s.read_follow_link("#section-two");
        let open =
            fx.0.iter()
                .find_map(|e| match e {
                    Effect::Request { method, params, .. } if *method == "buffer/open" => {
                        Some(params.clone())
                    }
                    _ => None,
                })
                .expect("an in-document anchor rides buffer/open");
        assert_eq!(
            open["record_nav_from"],
            serde_json::json!(s.buffer.buffer_id),
            "the origin is recorded"
        );
        let read = s.read.as_ref().unwrap();
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
                .any(|e| matches!(e, Effect::Request { method, .. } if *method == "buffer/open")),
            "a root-relative link opens"
        );
        assert_eq!(s.pending_read_anchor.as_deref(), Some("section-two"));

        // Outside every root there is no anchor — the filesystem-absolute reading stands.
        s.buffer.path = Some("/elsewhere/notes.md".into());
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
                PickerKind::Buffers,
                PickerItem::Buffer {
                    buffer_id: 7,
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
                open: WindowOpen::Buffer(7),
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
                    unsaved_buffers: 0,
                    match_indices: vec![],
                },
                0,
            ),
            Some(WindowTarget {
                workspace: Some("other".into()),
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
    let multi_root = workspace_paths.len() > 1;
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
