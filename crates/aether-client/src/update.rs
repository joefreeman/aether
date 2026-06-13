//! The core update function, grown arm by arm (docs/client-core.md phase 3): each migrated
//! subsystem moves its `Message` variants into [`Event`], its handler logic into
//! [`Session::on_event`], and its RPC chains into effect-returning methods here. The shell
//! bridges with a single `Message::Core(Event)` variant and an effect executor.

use super::chips::{self, ChipEditor, ChipEditorField, ChipId};
use super::effect::{Effect, Effects, ToastKind};
use super::keymap::{lookup, Action, InsertWhere, KeyCode, KeyContext, Mods};
use super::picker::{item_key, DefaultSkip, PickerState, Reveal, FETCH_LIMIT, VISIBLE_ROWS};
use super::session::{
    buffer_info, min_pos, severity_label, strip_longest_root, CommitDetails, ConfirmAction,
    ConnState, HoverBlock, HoverText, Mode, PasteKind, Pending, Prompt, ReloadTry, RepeatTarget,
    SaveTry, SearchSnapshot, SearchState, Session,
};
use super::transport::RpcError;
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferClosed, BufferClosedParams, BufferCopy, BufferCopyParams,
    BufferCopyResult, BufferCut, BufferCutResult, BufferOpen, BufferOpenParams, BufferOpenResult,
    BufferReload, BufferReloadParams, BufferSave, BufferSaveParams, BufferState, BufferStateParams,
    CopyScope,
};
use aether_protocol::cursor::Direction;
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorContract, CursorExpand, CursorMove, CursorMoveParams, CursorRedo,
    CursorSelectLine, CursorSelectLineParams, CursorSet, CursorSetParams, CursorState,
    CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo, CursorUndoParams, CursorUndoResult,
    Granularity, Motion, SelectionEdge,
};
use aether_protocol::directory::{
    DirectoryCreate, DirectoryCreateParams, DirectoryCreateResult, DirectoryList,
    DirectoryListParams, DirectoryListResult,
};
use aether_protocol::envelope::RpcMethod;
use aether_protocol::path::{PathDelete, PathDeleteParams, PathDeleteResult};
use aether_protocol::envelope::{Notification, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::git::{
    ApplyHunkStatus, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult, GitBlameLine,
    GitBlameLineParams, GitNavigateHunk, GitNavigateHunkParams, GitNavigateHunkResult,
    GitSetDiffView, GitSetDiffViewParams, HunkAction, HunkDirection,
};
use aether_protocol::input::{
    BufferOnlyParams, CountedEditParams, EditResult, InputBackspace, InputChangeLine, InputDedent,
    InputDelete, InputDeleteLine, InputIndent, InputJoinLines, InputMoveLines,
    InputMoveLinesParams, InputNewlineAndIndent, InputOpenLine, InputOpenLineParams, InputRedo,
    InputReplaceLine, InputReplaceLineParams, InputSurround, InputSurroundParams, InputText,
    InputTextParams, InputToggleComment, InputUndo, InputUnsurround, InputUnsurroundParams,
    LineSide, UndoResult,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspFormat, LspFormatResult, LspGotoDefinition,
    LspGotoDefinitionResult, LspHover, LspHoverResult, LspNavigateDiagnostic,
    LspNavigateDiagnosticParams, LspNavigateDiagnosticResult, LspRestartServer,
    LspRestartServerParams, LspServerStatus, LspStatusChanged,
};
use aether_protocol::nav::NavStepResult;
use aether_protocol::nav::{NavBack, NavForward, NavStepParams};
use aether_protocol::picker::{
    PickerFilters, PickerGrepFileJump, PickerGrepFileJumpParams, PickerGrepNavigate,
    PickerGrepNavigateParams, PickerHide, PickerHideParams, PickerItem, PickerKind, PickerQuery,
    PickerQueryParams, PickerSelect, PickerSelectParams, PickerSelectResult, PickerUpdate,
    PickerUpdateParams, PickerView, PickerViewParams, PickerViewResult, ScopedPath,
};
use aether_protocol::project::{ProjectActivate, ProjectActivateParams, ProjectInfo};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNavResult, SearchNext, SearchPrev,
    SearchSet, SearchSetParams, SearchSetResult, SearchStateChanged, SearchSummary,
};
use aether_protocol::viewport::{
    DiagnosticSeverity, ViewportLinesChanged, ViewportLinesChangedParams, ViewportSubscribeResult,
    ViewportWindowResult, Window, WrapMode,
};
use aether_protocol::LogicalPosition;

/// A core event: an async result (or shell-forwarded input) the core's update consumes.
#[derive(Debug)]
pub enum Event {
    SaveTried(Result<SaveTry, String>),
    ReloadTried(Result<ReloadTry, String>),
    /// A cursor-returning RPC resolved (motions, selections, clicks).
    CursorMsg(Result<CursorState, String>),
    /// An edit resolved: adopt the new revision + cursor.
    EditDone(Result<EditResult, String>),
    UndoRedoDone(Result<UndoResult, String>),
    CopyDone(Result<BufferCopyResult, String>),
    CutDone(Result<BufferCutResult, String>),
    /// The shell read the system clipboard for a paste gesture.
    ClipboardRead(PasteKind, Option<String>),
    /// A buffer switch resolved (close, new scratch, path opens): rebind to this buffer.
    Switched(Result<BufferOpenResult, String>),
    /// A grep-driven switch: like [`Event::Switched`] but priming the buffer search with the
    /// grep query so `n`/`Alt-n` step matches. `Ok(None)` = no more hits.
    SwitchedPrimed(Result<Option<(String, BufferOpenResult)>, String>),
    /// The prompt's Yes/Save button (keyboard accept routes through `on_prompt_key`).
    PromptAccept,
    PromptCancel,
    /// Incremental `search/set` (cursor follows the match; zero matches revert it).
    SearchApplied(Result<SearchSetResult, String>),
    /// Non-incremental `search/set` (abort-restore, search-from-selection revive): summary
    /// only, the cursor wasn't moved server-side.
    SearchRestored(Result<SearchSetResult, String>),
    SearchNav(Result<SearchNavResult, String>),
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
        /// Grep selections prime the opened buffer's search with the picker query.
        prime: Option<String>,
        result: Result<PickerSelectResult, String>,
    },
    /// A picker row was clicked (absolute index) — highlight it and accept.
    PickerClicked(u32),
    /// A filter chip was clicked — select it (virtual selection, like the keyboard path).
    PickerChipClicked(usize),
    /// `directory/list` for the dir-chip editor resolved; `abs` is the staleness key.
    PickerChipListing {
        abs: String,
        result: Result<DirectoryListResult, String>,
    },
    /// `picker/grep_file_jump` resolved: the next/prev file's first hit (None at the ends).
    GrepFileJumped(Result<Option<PickerItem>, String>),
    /// `path/delete` (Explorer/Files trash) resolved. `noun` labels the success toast; the
    /// open picker re-lists. Buffer closes for the deleted path arrive via the `buffer/closed`
    /// push, which already switches us off a deleted current buffer.
    PathDeleted {
        noun: &'static str,
        result: Result<PathDeleteResult, String>,
    },
    /// `directory/create` (Explorer "Ctrl-n name/") resolved: navigate into the new directory.
    DirCreated(Result<DirectoryCreateResult, String>),
    /// Project switch resolved: the activated project + the buffer to land on.
    ProjectActivated(Result<(ProjectInfo, BufferOpenResult), String>),
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
        project: ProjectInfo,
        open: BufferOpenResult,
        restarted: bool,
    },
    /// A fire-and-forget RPC completed; result ignored.
    Noop,
}

impl Session {
    /// Dispatch one core event. The shell feeds these from its bridge variant and executes
    /// the returned effects.
    pub fn on_event(&mut self, event: Event) -> Effects {
        match event {
            Event::CursorMsg(Ok(cursor)) => {
                self.buffer.cursor = cursor;
                Effects::one(Effect::RevealCursor)
            }
            Event::CursorMsg(Err(e)) => Effects::error(e),

            Event::EditDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                Effects::one(Effect::RevealCursor)
            }
            Event::EditDone(Err(e)) => Effects::error(e),

            Event::UndoRedoDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                let mut fx = if r.applied {
                    Effects::none()
                } else {
                    Effects::toast("nothing to undo/redo", ToastKind::Info)
                };
                fx.push(Effect::RevealCursor);
                fx
            }
            Event::UndoRedoDone(Err(e)) => Effects::error(e),

            Event::CopyDone(Ok(r)) => {
                let mut fx =
                    Effects::toast(format!("copied {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx
            }
            Event::CopyDone(Err(e)) => Effects::error(format!("copy failed: {e}")),

            Event::CutDone(Ok(r)) => {
                self.buffer.revision = r.revision;
                self.buffer.cursor = r.cursor;
                let mut fx =
                    Effects::toast(format!("cut {} bytes", r.text.len()), ToastKind::Success);
                fx.push(Effect::WriteClipboard(r.text));
                fx.push(Effect::RevealCursor);
                fx
            }
            Event::CutDone(Err(e)) => Effects::error(format!("cut failed: {e}")),

            Event::ClipboardRead(kind, text) => {
                let Some(text) = text.filter(|t| !t.is_empty()) else {
                    return Effects::error("clipboard is empty");
                };
                self.paste(kind, text)
            }

            Event::Switched(Ok(open)) => self.adopt_switch(open),
            Event::Switched(Err(e)) => Effects::error(e),

            Event::SwitchedPrimed(Ok(Some((query, open)))) => {
                // Grab the primed summary before `open` is consumed: the equivalent
                // `search/state_changed` push races this switch and the client's `buffer_id`
                // guard drops it if it lands before the switch, so the count rode the response.
                let summary = open.search_summary.clone();
                // A hit in the SAME buffer is a move, not a switch: keep the window/viewport/
                // diagnostics and just reposition the cursor, so the shell animates a short scroll to
                // it (consecutive same-file hits glide) instead of resubscribing — which replaces the
                // whole window and reads as an instant jump.
                let fx = if open.buffer_id == self.buffer.buffer_id {
                    self.buffer.cursor = open.cursor;
                    Effects::one(Effect::RevealCursor)
                } else {
                    self.adopt_switch(open)
                };
                // adopt_switch reset the search state; adopt the primed query (the
                // server-side search was already set in the open chain) and its summary.
                self.search.cursor = query.len();
                self.search.query = query.clone();
                self.search.active = true;
                self.search.summary = summary;
                self.push_history(query);
                fx
            }
            Event::SwitchedPrimed(Ok(None)) => Effects::toast("no more grep hits", ToastKind::Info),
            Event::SwitchedPrimed(Err(e)) => Effects::error(e),

            Event::PromptAccept => self.accept_prompt(),
            Event::PromptCancel => {
                self.decline_prompt();
                Effects::none()
            }

            Event::SearchApplied(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                let zero = r.summary.total == 0;
                self.search.summary = Some(r.summary);
                if zero {
                    // A failed keystroke shouldn't strand the user wherever the previous
                    // query had jumped them.
                    self.revert_to_snapshot_cursor()
                } else {
                    Effects::one(Effect::RevealCursor)
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
                Effects::toast("invalid regex", ToastKind::Warning)
                    .and(self.revert_to_snapshot_cursor())
            }

            Event::SearchRestored(Ok(r)) => {
                self.search.summary = Some(r.summary);
                Effects::none()
            }
            Event::SearchRestored(Err(e)) => Effects::error(e),

            Event::SearchNav(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                self.search.summary = Some(r.summary);
                Effects::one(Effect::RevealCursor)
            }
            Event::SearchNav(Err(e)) => Effects::error(e),

            Event::SearchFromSel(Ok(Some((query, r)))) => {
                self.search.cursor = query.len();
                self.search.query = query.clone();
                self.search.active = true;
                self.search.summary = Some(r.summary);
                self.push_history(query);
                Effects::none()
            }
            Event::SearchFromSel(Ok(None)) => Effects::none(), // empty selection
            Event::SearchFromSel(Err(e)) => Effects::error(e),

            Event::NavDone { forward, result } => match result {
                Ok(NavStepResult { target: Some(open) }) => self.adopt_switch(open),
                Ok(_) => Effects::toast(
                    if forward {
                        "no later location in history"
                    } else {
                        "no earlier location in history"
                    },
                    ToastKind::Info,
                ),
                Err(e) => Effects::error(e),
            },

            Event::Definition(Ok(r)) => match r.location {
                Some(location) => {
                    self.open_path_primed(location.path, Some(location.position), None)
                }
                None => Effects::toast("No definition found", ToastKind::Info),
            },
            Event::Definition(Err(e)) => Effects::error(e),

            Event::DiagNav(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                let mut fx = if r.moved {
                    Effects::none()
                } else {
                    Effects::toast("no more diagnostics", ToastKind::Info)
                };
                fx.push(Effect::RevealCursor);
                fx
            }
            Event::DiagNav(Err(e)) => Effects::error(e),

            Event::HoverInfo(Ok(r)) => match r.contents {
                Some(text) => Effects::one(Effect::ShowHover(HoverText::Markdown(text))),
                None => {
                    let mut fx = Effects::one(Effect::DismissHover);
                    fx.push(Effect::Toast("No hover info".into(), ToastKind::Info));
                    fx
                }
            },
            Event::HoverInfo(Err(e)) => Effects::error(format!("hover failed: {e}")),

            Event::FormatDone(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                // Specific feedback per outcome — "nothing happened" has several causes.
                let note = match r.status {
                    FormatStatus::Applied => None,
                    FormatStatus::NoChange => Some("already formatted".to_string()),
                    FormatStatus::NotReady => Some("language server still starting".to_string()),
                    FormatStatus::Unavailable => Some("language server unavailable".to_string()),
                    FormatStatus::Unsupported => Some(match self.buffer.language.as_deref() {
                        Some(lang) => format!("no formatter for {lang}"),
                        None => "no formatter for this file".to_string(),
                    }),
                };
                let mut fx = match note {
                    Some(n) => Effects::toast(n, ToastKind::Info),
                    None => Effects::none(),
                };
                fx.push(Effect::RevealCursor);
                fx
            }
            Event::FormatDone(Err(e)) => Effects::error(format!("format failed: {e}")),

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
            Event::CommitLookup(Err(e)) => Effects::error(format!("commit info failed: {e}")),

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

            Event::HunkNav(Ok(r)) => {
                self.buffer.cursor = r.cursor;
                let mut fx = if r.moved {
                    Effects::none()
                } else {
                    Effects::toast("no more changes", ToastKind::Info)
                };
                fx.push(Effect::RevealCursor);
                fx
            }
            Event::HunkNav(Err(e)) => Effects::error(e),

            Event::HunkApplied { action, result } => match result {
                Ok(r) => {
                    self.buffer.cursor = r.cursor;
                    let (msg, kind) = match r.status {
                        ApplyHunkStatus::Staged => ("staged change", ToastKind::Success),
                        ApplyHunkStatus::Unstaged => ("unstaged change", ToastKind::Success),
                        ApplyHunkStatus::Reverted => ("reverted change", ToastKind::Success),
                        ApplyHunkStatus::NoChange => (
                            match action {
                                HunkAction::Toggle => "no change here",
                                HunkAction::Revert => "no change to revert here",
                            },
                            ToastKind::Info,
                        ),
                        ApplyHunkStatus::DirtyBuffer => {
                            ("unsaved changes — save first", ToastKind::Warning)
                        }
                        ApplyHunkStatus::Unavailable => {
                            ("not in a git repository", ToastKind::Info)
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
                    fx.push(Effect::Toast(
                        format!("diff: {}", if enabled { "on" } else { "off" }),
                        ToastKind::Info,
                    ));
                    fx
                }
                Err(e) => Effects::error(e),
            },

            Event::PickerViewed { initial, result } => match result {
                Ok(r) => {
                    if let Some(p) = &mut self.picker {
                        p.offset = r.effective_offset;
                        if let Some(center) = r.effective_center_on {
                            p.pending_center = Some(center);
                            // Grep centering (cursor-hit opens, file jumps) aligns the
                            // target to the top — there's context below to read.
                            p.reveal_on_update = Some(if p.kind == PickerKind::Grep {
                                Reveal::Top
                            } else {
                                Reveal::Minimal
                            });
                        }
                        p.directory = r.directory_path;
                        p.directory_parent = r.directory_parent;
                        if initial {
                            // Adopt the resumed query (grep preserves it across opens) and
                            // the persisted filters (seeded opens get their seed echoed).
                            p.generation = r.generation;
                            p.cursor = r.query.len();
                            p.query = r.query;
                            p.total_candidates = r.total_candidates;
                            p.adopt_filters(&r.filters);
                        }
                        // Apply the window folded into the response now that generation/offset
                        // are set, so a Grep resume renders its rows even when the redundant
                        // `picker/update` push raced ahead of this response and was discarded.
                        // `apply_update` is generation/offset-guarded — a no-op if it doesn't fit.
                        if let Some(update) = r.update {
                            if p.apply_update(update) && p.pending_center.is_none() {
                                if let Some(reveal) = p.reveal_on_update.take() {
                                    return Effects::one(Effect::RevealPickerSelection(reveal));
                                }
                            }
                        }
                    }
                    Effects::none()
                }
                Err(e) => {
                    self.picker = None;
                    Effects::error(format!("picker failed: {e}"))
                }
            },

            // Selections open in place: the window shows one buffer, and the one being
            // replaced is a `Space b` away (buffers persist server-side). Opens are
            // transient previews — switching away from one closes it.
            Event::PickerSelected {
                prime,
                result: Ok(result),
            } => match result {
                PickerSelectResult::File { path } => self.open_path_primed(path, None, prime),
                PickerSelectResult::FileAt { path, position } => {
                    self.open_path_primed(path, Some(position), prime)
                }
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
                PickerSelectResult::Project { name } => {
                    // Activate and land on the project's last buffer (or a fresh transient
                    // scratch) — the bootstrap convention, now one server-side composite.
                    self.request_str::<ProjectActivate>(
                        ProjectActivateParams {
                            name,
                            open_last: true,
                        },
                        |r| {
                            Event::ProjectActivated(r.and_then(|a| {
                                let opened = a.opened.ok_or_else(|| {
                                    "project/activate returned no landing buffer".to_string()
                                })?;
                                Ok((a.project, opened))
                            }))
                        },
                    )
                }
            },
            Event::PickerSelected { result: Err(e), .. } => {
                Effects::error(format!("select failed: {e}"))
            }

            Event::ProjectActivated(Ok((project, open))) => {
                self.project = project.name;
                self.project_paths = project.paths;
                self.adopt_switch(open)
            }
            Event::ProjectActivated(Err(e)) => {
                Effects::error(format!("project switch failed: {e}"))
            }

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
                Effects::none()
            }

            Event::GrepFileJumped(Ok(None)) => Effects::none(), // already at the first/last
            Event::GrepFileJumped(Ok(Some(target))) => {
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
                        reset: false,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: Some(target),
                        center_on_cursor_grep_hit: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        filters: None,
                    },
                    move |__r| Event::PickerViewed {
                        initial: false,
                        result: __r.map_err(|e| e.to_string()),
                    },
                )
            }
            Event::GrepFileJumped(Err(e)) => Effects::error(format!("file jump failed: {e}")),

            Event::PathDeleted { noun, result } => match result {
                Err(e) => Effects::error(format!("delete failed: {e}")),
                Ok(_) => {
                    // Any close of *our* buffer rides the `buffer/closed` push (it switches us
                    // to the server's successor). Here we just confirm and re-list the picker.
                    let mut fx = Effects::toast(format!("trashed {noun}"), ToastKind::Success);
                    if let Some(p) = &self.picker {
                        if p.kind == PickerKind::Explorer {
                            let dir = p.directory.clone();
                            fx = fx.and(self.explorer_navigate(dir, false, None));
                        } else if p.kind == PickerKind::Files {
                            fx = fx.and(self.open_picker(PickerKind::Files, None, None));
                        }
                    }
                    fx
                }
            },
            Event::DirCreated(Err(e)) => Effects::error(format!("create directory failed: {e}")),
            Event::DirCreated(Ok(r)) => {
                let mut fx = Effects::toast(format!("created {}", r.path), ToastKind::Success);
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
                tracing::warn!(buffer = %self.buffer.label, "connection lost; reconnecting");
                let mut fx =
                    Effects::toast("server disconnected — reconnecting…", ToastKind::Warning);
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
                Effects::error(format!("reconnect failed: {e}"))
            }
            Event::Reestablished {
                project,
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
                self.project = project.name;
                self.project_paths = project.paths;
                let same_file = open.path == self.buffer.path;
                self.buffer = buffer_info(open, &self.project_paths);
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
                        },
                        move |__r| Event::SearchRestored(__r.map_err(|e| e.to_string())),
                    ));
                }
                fx.push(if restarted && had_unsaved {
                    Effect::Toast(
                        "reconnected — the server restarted, unsaved changes were lost".into(),
                        ToastKind::Warning,
                    )
                } else {
                    Effect::Toast("reconnected".into(), ToastKind::Success)
                });
                fx
            }

            Event::Noop => Effects::none(),
            Event::SaveTried(Ok(SaveTry::Saved { result, target })) => {
                self.buffer.revision = result.revision;
                self.buffer.saved_revision = result.revision;
                self.buffer.transient = false; // saving promotes a transient buffer
                self.externally_modified = false;
                self.externally_deleted = false;
                let note = match target {
                    Some((path_index, rel)) => {
                        // Save-as: the buffer's identity changed — adopt the new path/label.
                        let root = self.project_paths.get(path_index as usize);
                        self.buffer.path =
                            root.map(|r| format!("{}/{rel}", r.trim_end_matches('/')));
                        self.buffer.label = rel.clone();
                        format!("saved as {rel} (rev {})", result.revision)
                    }
                    None => format!("saved (rev {})", result.revision),
                };
                Effects::toast(note, ToastKind::Success)
            }
            Event::SaveTried(Ok(SaveTry::NeedsConfirm { message, action })) => {
                self.prompt = Some(Prompt::Confirm { message, action });
                Effects::none()
            }
            Event::SaveTried(Err(e)) => Effects::error(format!("save failed: {e}")),

            Event::ReloadTried(Ok(ReloadTry::Reloaded(r))) => {
                self.buffer.revision = r.revision;
                self.buffer.saved_revision = r.revision;
                self.buffer.transient = false; // reloading promotes, like save
                self.externally_modified = false;
                self.externally_deleted = false;
                Effects::toast(format!("reloaded (rev {})", r.revision), ToastKind::Success)
            }
            Event::ReloadTried(Ok(ReloadTry::NeedsConfirm)) => {
                self.prompt = Some(Prompt::Confirm {
                    message: "discard local changes and reload".into(),
                    action: ConfirmAction::ReloadDiscard,
                });
                Effects::none()
            }
            Event::ReloadTried(Err(e)) => Effects::error(format!("reload failed: {e}")),
        }
    }

    /// `buffer/save`, mapping the server's refusal codes to a `[y/N]` confirmation that
    /// retries with `overwrite: true`. `target` is the save-as `(path_index, relative_path)`.
    pub fn save(&mut self, target: Option<(u32, String)>, overwrite: bool) -> Effects {
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
                    Ok(result) => Ok(SaveTry::Saved { result, target }),
                    Err(e) if e.code == ErrorCode::WOULD_OVERWRITE.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: match &target {
                                Some((_, p)) => format!("overwrite {p}"),
                                None => "overwrite".into(),
                            },
                            action: ConfirmAction::Save { target },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_MODIFIED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: "file changed on disk — overwrite".into(),
                            action: ConfirmAction::Save { target },
                        })
                    }
                    Err(e) if e.code == ErrorCode::EXTERNALLY_DELETED.code() => {
                        Ok(SaveTry::NeedsConfirm {
                            message: "file removed on disk — recreate".into(),
                            action: ConfirmAction::Save { target },
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
                // Insert at the selection start — the collapse rides the edit
                // (docs/protocol-composites.md, D) instead of a prior cursor/set.
                at: Some(SelectionEdge::Start),
            }),
            PasteKind::Replace { count } => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: text.repeat(count.max(1) as usize),
                select_pasted: true,
                at: None,
            }),
            PasteKind::AtCursor => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text,
                select_pasted: false,
                at: None,
            }),
            PasteKind::Line => {
                self.edit::<InputReplaceLine>(InputReplaceLineParams { buffer_id, text })
            }
        }
    }

    /// Insert literal text at the cursor — an IME composition commit (or any shell-supplied text).
    /// Insert mode only: composed text is editing input, not a command. Same edit as a typed key
    /// (no `select_pasted`), so multi-character composed strings land like normal typing.
    pub fn insert_text(&mut self, text: String) -> Effects {
        let text: String = text.chars().filter(|c| !c.is_control() || *c == '\t').collect();
        if self.mode != Mode::Insert || text.is_empty() {
            return Effects::none();
        }
        self.edit::<InputText>(InputTextParams {
            buffer_id: self.buffer.buffer_id,
            text,
            select_pasted: false,
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

    /// Rebind the session to a freshly opened buffer: reset all per-buffer state (modal,
    /// diagnostics, viewport binding, prompts/pickers — an externally-triggered switch can
    /// land mid-pick) and ask the shell to resubscribe. Search history survives switches.
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
        self.picker = None;
        let history = std::mem::take(&mut self.search.history);
        self.search = SearchState {
            history,
            ..SearchState::default()
        };
        self.buffer = buffer_info(open, &self.project_paths);
        Effects::one(Effect::Resubscribe)
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

    /// Close the buffer, then attach to the server-indicated next MRU buffer (or a fresh
    /// scratch).
    pub fn close_buffer(&mut self) -> Effects {
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

    /// Open a file by absolute path as a transient preview — result-style navigation (picker
    /// selections, goto-definition). Records the jump origin onto the nav history first.
    /// `prime_search` (grep flows) also sets the opened buffer's search to that query so
    /// `n`/`Alt-n` step matches.
    pub fn open_path_primed(
        &mut self,
        path: String,
        jump_to: Option<LogicalPosition>,
        prime_search: Option<String>,
    ) -> Effects {
        let Some((path_index, relative_path)) = strip_longest_root(&path, &self.project_paths)
        else {
            return Effects::error(format!("{path} is outside the project's roots"));
        };
        let prime = prime_search.clone();
        self.request_str::<BufferOpen>(
            BufferOpenParams {
                path_index: Some(path_index),
                relative_path: Some(relative_path),
                jump_to,
                transient: Some(true),
                record_nav_from: Some(self.buffer.buffer_id),
                prime_search,
                ..Default::default()
            },
            move |r| match (prime, r) {
                (Some(q), Ok(open)) => Event::SwitchedPrimed(Ok(Some((q, open)))),
                (None, Ok(open)) => Event::Switched(Ok(open)),
                (_, Err(e)) => Event::Switched(Err(e)),
            },
        )
    }

    /// Append a committed search to the history (deduped against the latest entry, capped).
    pub fn push_history(&mut self, query: String) {
        const SEARCH_HISTORY_MAX: usize = 100;
        if query.is_empty() || self.search.history.last() == Some(&query) {
            return;
        }
        self.search.history.push(query);
        let overflow = self.search.history.len().saturating_sub(SEARCH_HISTORY_MAX);
        if overflow > 0 {
            self.search.history.drain(..overflow);
        }
    }

    /// Keys while a modal prompt is open. Confirm: `y`/Enter accepts, anything else declines
    /// (the `[y/N]` default). Save-as: a one-line path editor (Tab cycles the target root).
    pub fn on_prompt_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        let Some(prompt) = self.prompt.take() else {
            return Effects::none();
        };
        match prompt {
            Prompt::Confirm { message: _, action } => {
                let accepts = !mods.ctrl
                    && !mods.alt
                    && (code == KeyCode::Char('y') || code == KeyCode::Enter);
                if accepts {
                    self.run_confirm(action)
                } else {
                    self.decline_confirm(action);
                    Effects::none()
                }
            }
            Prompt::LspInfo(info) => {
                // `r` restarts; any other key closes the dialog.
                if code == KeyCode::Char('r') && !mods.ctrl && !mods.alt {
                    let mut fx = self.request::<LspRestartServer>(
                        LspRestartServerParams {
                            language: info.language.clone(),
                        },
                        move |__r| {
                            let _ = __r;
                            Event::Noop
                        },
                    );
                    fx.push(Effect::Toast(
                        format!("restarting {}", info.name),
                        ToastKind::Info,
                    ));
                    return fx;
                }
                Effects::none()
            }
            Prompt::SaveAs {
                path_index,
                mut input,
                mut cursor,
            } => {
                match code {
                    KeyCode::Esc => return Effects::none(), // prompt stays closed
                    // Tab cycles the target root in multi-root projects.
                    KeyCode::Tab => {
                        let n = self.project_paths.len().max(1) as u32;
                        self.prompt = Some(Prompt::SaveAs {
                            path_index: (path_index + 1) % n,
                            input,
                            cursor,
                        });
                        return Effects::none();
                    }
                    KeyCode::Enter => {
                        let path = input.trim().to_string();
                        if path.is_empty() {
                            self.prompt = Some(Prompt::SaveAs {
                                path_index,
                                input,
                                cursor,
                            });
                            return Effects::none();
                        }
                        // An absolute path re-resolves against the project roots.
                        let target = if path.starts_with('/') {
                            match strip_longest_root(&path, &self.project_paths) {
                                Some(target) => target,
                                None => {
                                    return Effects::error(format!(
                                        "{path} is outside the project's roots"
                                    ));
                                }
                            }
                        } else {
                            (path_index, path)
                        };
                        return self.save(Some(target), false);
                    }
                    KeyCode::Backspace => {
                        if let Some((i, _)) = input[..cursor].char_indices().last() {
                            input.remove(i);
                            cursor = i;
                        }
                    }
                    KeyCode::Left => {
                        if let Some((i, _)) = input[..cursor].char_indices().last() {
                            cursor = i;
                        }
                    }
                    KeyCode::Right => {
                        if let Some(c) = input[cursor..].chars().next() {
                            cursor += c.len_utf8();
                        }
                    }
                    _ => {
                        if !mods.ctrl && !mods.alt {
                            if let Some(t) = text {
                                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                                input.insert_str(cursor, &t);
                                cursor += t.len();
                            }
                        }
                    }
                }
                self.prompt = Some(Prompt::SaveAs {
                    path_index,
                    input,
                    cursor,
                });
                Effects::none()
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
            fx.push(Effect::Toast(
                "No diagnostics on this line".into(),
                ToastKind::Info,
            ));
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

    /// `Space o` — blame the cursor line and resolve the commit's details, one round-trip
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

    /// Open a picker: subscribe a window and let `picker/update` pushes fill it. Grep resumes
    /// its prior query/hits (centred on the cursor's nearest hit); the rest reset.
    /// `directory_path` seeds the Explorer's listing (its `Space e` = the buffer's directory).
    /// `seed_filters` replaces the server's persisted set (Explorer→Grep/Files switches,
    /// `Space Alt-f/g`); the echo through `PickerViewed` rebuilds the chip row.
    pub fn open_picker(
        &mut self,
        kind: PickerKind,
        directory_path: Option<String>,
        seed_filters: Option<PickerFilters>,
    ) -> Effects {
        let reset = !kind.preserves_state();
        self.picker = Some(PickerState::new(kind));
        let buffer_id = self.buffer.buffer_id;
        // Buffers / Projects: default the highlight to the first item that isn't the active
        // buffer/project, so Enter is a quick flip to the previous one (web/TUI behaviour).
        // Resolved by the first non-empty push.
        let skip = match kind {
            PickerKind::Buffers => Some(DefaultSkip::Buffer(buffer_id)),
            PickerKind::Projects => Some(DefaultSkip::Project(self.project.clone())),
            _ => None,
        };
        if let Some(p) = &mut self.picker {
            p.default_skip = skip;
        }
        // Explorer: anchor the highlight on the active buffer's filename, so the listing
        // lands on "where you are" (matched by name via the `effective_center_on` echo).
        let center_on = (kind == PickerKind::Explorer)
            .then(|| {
                let path = self.buffer.path.as_deref()?;
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
            })
            .flatten();

        Effects::one(Effect::PickerScrollReset).and(
            self.request::<PickerView>(
                PickerViewParams {
                    kind,
                    reset,
                    offset: 0,
                    limit: FETCH_LIMIT,
                    center_on,
                    center_on_cursor_grep_hit: (kind == PickerKind::Grep).then_some(buffer_id),
                    directory_path,
                    explorer_roots: false,
                    buffer_id: matches!(kind, PickerKind::Diagnostics | PickerKind::References)
                        .then_some(buffer_id),
                    filters: seed_filters,
                },
                move |__r| Event::PickerViewed {
                    initial: true,
                    result: __r.map_err(|e| e.to_string()),
                },
            ),
        )
    }

    /// `Space Alt-f` / `Space Alt-g`: open Files/Grep pre-scoped to the active buffer's
    /// directory — a normal dir filter chip, visible and removable. Falls back to an unscoped
    /// open for scratch buffers or files outside every root.
    pub fn open_picker_in_buffer_dir(&mut self, kind: PickerKind) -> Effects {
        let seed = self
            .buffer
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).parent())
            .map(|p| p.display().to_string())
            .and_then(|dir| strip_longest_root(&dir, &self.project_paths))
            .map(|(path_index, relative_path)| PickerFilters {
                directories: vec![ScopedPath {
                    path_index,
                    relative_path,
                }],
                ..PickerFilters::default()
            });
        self.open_picker(kind, None, seed)
    }

    /// `Ctrl-g` / `Ctrl-f` in the Explorer: switch to the Grep / Files picker scoped to the
    /// directory being browsed ("grep here"), the explorer's filters translated along. In
    /// Roots mode no dir scope is seeded — the target covers the whole project.
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
            .and_then(|abs| strip_longest_root(abs, &self.project_paths))
            .map(|(path_index, relative_path)| ScopedPath {
                path_index,
                relative_path,
            });
        let seeded = seeded_filters_for_switch(&p.wire_filters(), dir_scope, target);
        let hide = self.close_picker();
        hide.and(self.open_picker(target, None, Some(seeded)))
    }

    /// `Space e` / `Space Alt-e`: Explorer at the buffer's directory, or at its project root.
    /// Scratch buffers fall through to the server default (last listing / first root).
    pub fn open_explorer(&mut self, at_root: bool) -> Effects {
        let dir = self.buffer.path.as_deref().and_then(|path| {
            if at_root {
                let (i, _) = strip_longest_root(path, &self.project_paths)?;
                self.project_paths.get(i as usize).cloned()
            } else {
                std::path::Path::new(path)
                    .parent()
                    .map(|p| p.display().to_string())
            }
        });
        self.open_picker(PickerKind::Explorer, dir, None)
    }

    /// Explorer navigation: list a different directory (or the project roots). Clears the
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
        p.cursor = 0;
        p.selected = 0;
        p.offset = 0;
        p.items.clear();
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
                reset: false,
                offset: 0,
                limit: FETCH_LIMIT,
                center_on,
                center_on_cursor_grep_hit: None,
                directory_path,
                explorer_roots: roots,
                buffer_id: None,
                filters: Some(filters),
            },
            move |__r| Event::PickerViewed {
                initial: false,
                result: __r.map_err(|e| e.to_string()),
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
        match p.move_selection(delta) {
            Some(offset) => self.picker_refetch(offset),
            None => Effects::one(Effect::RevealPickerSelection(Reveal::Minimal)),
        }
    }

    /// Re-subscribe the picker's window at a new offset (the highlight moved past it).
    pub fn picker_refetch(&mut self, offset: u32) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.offset = offset;
        p.items.clear();
        let kind = p.kind;

        self.request::<PickerView>(
            PickerViewParams {
                kind,
                reset: false,
                offset,
                limit: FETCH_LIMIT,
                center_on: None,
                center_on_cursor_grep_hit: None,
                directory_path: None,
                explorer_roots: false,
                buffer_id: None,
                filters: None,
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
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.generation += 1;
        p.selected = 0;
        p.offset = 0;
        // A new query is in flight: mark the picker as searching now, before the first
        // `picker/update` push arrives, so the shell can show progress in the gap (otherwise a slow
        // grep reads as "no matches" until results stream). The server's pushes refine it from here.
        p.ticking = true;
        // A query change invalidates any pending pre-selection (centering / skip-the-
        // active-item default) — the user is steering somewhere new.
        p.pending_center = None;
        p.default_skip = None;
        p.reveal_on_update = None;
        let (kind, query, generation) = (p.kind, p.query.clone(), p.generation);
        let filters = p.wire_filters();

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
        fx.and(self.picker_refetch(0))
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
        p.cursor = query.len();
        p.query = query;
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
        self.search.cursor = self.search.query.len();
        self.search.history_cursor = None;
        self.incremental_search()
    }

    /// Replace the save-as prompt's path input (the web client's native `<input>` owns editing). The
    /// actual save happens on accept; this is pure state. No-op unless a save-as prompt is open.
    pub fn prompt_set_input(&mut self, text: String) -> Effects {
        if let Some(Prompt::SaveAs { input, cursor, .. }) = &mut self.prompt {
            *cursor = text.len();
            *input = text;
        }
        Effects::none()
    }

    /// Replace the chip editor's path-field text wholesale (the web client's native `<input>` owns
    /// editing and syncs the value here). For a dir editor this re-derives the directory suggestion
    /// listing. No-op unless a chip editor is open.
    pub fn chip_editor_set_input(&mut self, text: String) -> Effects {
        let project_paths = self.project_paths.clone();
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
        let refresh = ed.is_dir() && ed.path_edited(&project_paths);
        if refresh {
            self.refresh_chip_editor_listing()
        } else {
            Effects::none()
        }
    }

    /// Replace the multi-root dir editor's root-filter text wholesale (native `<input>` parity).
    /// Resets the typeahead highlight to the best match and re-syncs the listing under the newly
    /// chosen root. No-op unless a chip editor is open.
    pub fn chip_editor_set_root_filter(&mut self, text: String) -> Effects {
        let project_paths = self.project_paths.clone();
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
        let refresh = ed.sync_dir_listing(&project_paths);
        if refresh {
            self.refresh_chip_editor_listing()
        } else {
            Effects::none()
        }
    }

    /// Move focus between the dir editor's root and path segments (the web client lets you click the
    /// unfocused segment). The path can't be entered under an invalid root — focus stays pinned to
    /// the red root, matching the keyboard gate. No-op outside a multi-root dir editor.
    pub fn chip_editor_set_field(&mut self, root: bool) -> Effects {
        let project_paths = self.project_paths.clone();
        let labels = super::labels::root_labels(&project_paths);
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        if !ed.is_dir() || project_paths.len() <= 1 {
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

    /// Push a filter (chip) change. For Grep/Files a filter change *is* a query change (same
    /// generation mechanics); for the Explorer the filters apply when the listing is built,
    /// so re-view the current directory with the replacement set. No-op for kinds that take
    /// no filters, and for the Explorer's Roots mode (nothing to filter there).
    fn apply_picker_filter_change(&mut self) -> Effects {
        let Some(kind) = self.picker.as_ref().map(|p| p.kind) else {
            return Effects::none();
        };
        match kind {
            PickerKind::Grep | PickerKind::Files => self.picker_query_changed(),
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
                    p.wire_filters()
                };

                Effects::one(Effect::PickerScrollReset).and(self.request::<PickerView>(
                    PickerViewParams {
                        kind: PickerKind::Explorer,
                        reset: false,
                        offset: 0,
                        limit: FETCH_LIMIT,
                        center_on: None,
                        center_on_cursor_grep_hit: None,
                        directory_path: None,
                        explorer_roots: false,
                        buffer_id: None,
                        filters: Some(filters),
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

    /// Toggle/cycle the filter a chord (or Enter on a selected chip) names, then push the
    /// change. A chord that doesn't apply to this picker kind is a clean no-op.
    fn toggle_picker_filter(&mut self, id: ChipId) -> Effects {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !chips::filter_applies(p.kind, id) {
            return Effects::none();
        }
        let explorer = p.kind == PickerKind::Explorer;
        if !chips::apply_chip_toggle(&mut p.chips, id, explorer) {
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
        p.chip_editor = Some(ChipEditor::glob(prefill, edit));
        Effects::none()
    }

    /// Open the directory-scope editor line. `edit: Some(i)` re-opens scope `i` pre-filled
    /// (path focused); `None` adds a new chip on commit (multi-root projects focus the root
    /// segment first). Kicks off a `directory/list` so the path field's ghost suggestions
    /// are ready when focus lands there.
    fn open_dir_prompt(&mut self, edit: Option<usize>) -> Effects {
        let project_paths = self.project_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !chips::filter_applies(p.kind, ChipId::Dir(0)) {
            return Effects::none();
        }
        p.chip_selected = None;
        let current = edit.and_then(|i| p.dir_value(i).cloned());
        let multi_root = project_paths.len() > 1;
        let root_index = current.as_ref().map(|d| d.path_index).unwrap_or(0);
        let field = if multi_root && current.is_none() {
            ChipEditorField::Root
        } else {
            ChipEditorField::Path
        };
        let mut ed = ChipEditor::dir(
            current.map(|d| d.relative_path).unwrap_or_default(),
            field,
            root_index,
            edit,
        );
        ed.sync_dir_listing(&project_paths);
        p.chip_editor = Some(ed);
        self.refresh_chip_editor_listing()
    }

    /// Fire `directory/list` for the dir-chip editor's current (root, dir-portion) pair. The
    /// requested path rides on the result event so a stale response (the editor moved on)
    /// can be discarded. No-op for glob editors and invalid roots.
    fn refresh_chip_editor_listing(&mut self) -> Effects {
        let project_paths = self.project_paths.clone();
        let Some(path) = self
            .picker
            .as_ref()
            .and_then(|p| p.chip_editor.as_ref())
            .and_then(|ed| ed.dir_listing_path(&project_paths))
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
        let project_paths = self.project_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if let Some(ed) = p.chip_editor.as_ref() {
            if ed.is_dir() {
                let root_ok = project_paths.len() < 2 || {
                    let labels = super::labels::root_labels(&project_paths);
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
        // A partially typed leaf commits as its highlighted completion — `committed_path` is
        // the typed text for glob editors and whenever there's nothing to complete.
        let text = ed.committed_path().trim().trim_matches('/').to_string();
        let changed = match ed.kind {
            chips::ChipEditorKind::Glob { edit } => {
                let normalized = chips::normalize_glob(&ed.input.text);
                chips::commit_glob_edit(&mut p.chips, normalized, edit)
            }
            chips::ChipEditorKind::Dir { edit } => {
                // An empty path is a whole-root scope in multi-root projects and clears the
                // chip in single-root ones (where "the whole root" means "no narrowing").
                let multi_root = project_paths.len() > 1;
                let value = if text.is_empty() && !multi_root {
                    None
                } else {
                    let labels = super::labels::root_labels(&project_paths);
                    let path_index = if multi_root {
                        ed.chosen_root(&labels)
                    } else {
                        0
                    };
                    Some(ScopedPath {
                        path_index,
                        relative_path: text,
                    })
                };
                chips::commit_dir_edit(&mut p.chips, value, edit)
            }
        };
        if !changed {
            return Effects::none();
        }
        self.apply_picker_filter_change()
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
            let dir = match &p.directory {
                Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                None => return Effects::none(),
            };
            return self.explorer_navigate(Some(dir), false, None);
        }
        Effects::none()
    }

    /// Alt-h / Alt-Backspace: progressively unwind — clear the query, then pop the rightmost
    /// filter chip (one per press), then (explorer) one directory segment per press — landing
    /// the highlight on the directory just left — then roots mode in multi-root projects.
    fn picker_back(&mut self) -> Effects {
        let project_paths = self.project_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.query.is_empty() {
            p.query.clear();
            p.cursor = 0;
            return self.picker_query_changed();
        }
        if let Some(chip) = p.chip_row(&project_paths).last().map(|c| c.id) {
            chips::remove_chip(&mut p.chips, chip);
            p.chip_selected = None;
            return self.apply_picker_filter_change();
        }
        if p.kind != PickerKind::Explorer {
            return Effects::none();
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
                self.explorer_navigate(Some(parent), false, leaving)
            }
            None if p.directory.is_some() => {
                if project_paths.len() > 1 {
                    self.explorer_navigate(None, true, None)
                } else {
                    Effects::none()
                }
            }
            None => Effects::none(),
        }
    }

    /// Enter / row click: act on the highlighted item. Directories and roots navigate within
    /// the open explorer; everything else closes the panel and runs `picker/select`.
    fn picker_accept(&mut self) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        let Some(item) = p.selected_item().cloned() else {
            return Effects::none();
        };
        match &item {
            PickerItem::DirEntry {
                name, is_dir: true, ..
            } => {
                let dir = match &p.directory {
                    Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                    None => return Effects::none(),
                };
                return self.explorer_navigate(Some(dir), false, None);
            }
            PickerItem::Root { path_index, .. } => {
                let dir = self.project_paths.get(*path_index as usize).cloned();
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
                // there and on Ctrl-r in the list).
                let info = LspServerStatus {
                    name: name.clone(),
                    language: language.clone(),
                    workspace_root: workspace_root.clone(),
                    status: status.clone(),
                    progress: progress.clone(),
                };
                let _ = root_label;
                let hide = self.close_picker();
                self.prompt = Some(Prompt::LspInfo(Box::new(info)));
                return hide;
            }
            _ => {}
        }
        let kind = p.kind;
        let prime = (kind == PickerKind::Grep).then(|| p.query.clone());
        let hide = self.close_picker();

        hide.and(
            self.request::<PickerSelect>(PickerSelectParams { kind, item }, move |__r| {
                Event::PickerSelected {
                    prime,
                    result: __r.map_err(|e| e.to_string()),
                }
            }),
        )
    }

    /// Drop the panel and unsubscribe (the server keeps walker/matcher state for resume).
    /// Select the rightmost filter chip (the browser tag-input gesture: Left / Backspace at the start
    /// of the query steps into the chip row). The web client's native query `<input>` owns the caret,
    /// so the shell detects "at query start" itself and calls this, rather than relying on the core's
    /// cursor-based entry in [`Self::on_picker_key`]. No-op when there are no chips. Pure selection
    /// state — no effects. Once a chip is selected, the chip-nav keys route through `on_picker_key`.
    pub fn picker_select_last_chip(&mut self) -> Effects {
        let project_paths = self.project_paths.clone();
        if let Some(p) = &mut self.picker {
            let n = p.chip_row(&project_paths).len();
            if n > 0 {
                p.chip_selected = Some(n - 1);
            }
        }
        Effects::none()
    }

    pub fn close_picker(&mut self) -> Effects {
        let Some(p) = self.picker.take() else {
            return Effects::none();
        };

        self.request::<PickerHide>(PickerHideParams { kind: p.kind }, move |__r| {
            let _ = __r;
            Event::Noop
        })
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
        let project_paths = self.project_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let no_chord = !mods.ctrl && !mods.alt;
        // A selected chip captures the editing keys (Enter edits, Backspace/Delete removes,
        // Left/Right walk the row, Esc deselects, typing deselects back into the query).
        // Anything else falls through to the normal picker vocabulary below.
        if let Some(sel) = p.chip_selected {
            let row = p.chip_row(&project_paths);
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
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        match code {
            KeyCode::Esc => return self.close_picker(),
            KeyCode::Enter => return self.picker_accept(),
            // Delete / Ctrl-d: trash the highlighted entry (Files + Explorer), behind a confirm.
            KeyCode::Delete
                if matches!(p.kind, PickerKind::Files | PickerKind::Explorer) =>
            {
                return self.picker_stage_delete();
            }
            KeyCode::Char('d')
                if mods.ctrl
                    && !mods.alt
                    && matches!(p.kind, PickerKind::Files | PickerKind::Explorer) =>
            {
                return self.picker_stage_delete();
            }
            // Ctrl-n in the Explorer: create the file/dir named by the query (trailing `/` = dir).
            KeyCode::Char('n') if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer => {
                return self.explorer_create_from_query();
            }
            // Alt-k/j move the highlight (Up/Down deliberately don't, matching the others).
            KeyCode::Char('k') if mods.alt && !mods.ctrl => return self.picker_move(-1),
            KeyCode::Char('j') if mods.alt && !mods.ctrl => return self.picker_move(1),
            // `Ctrl-g` / `Ctrl-f` in the Explorer: switch to Grep / Files scoped to the
            // browsed directory ("grep here").
            KeyCode::Char('g') if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer => {
                return self.switch_explorer_picker(PickerKind::Grep);
            }
            KeyCode::Char('f') if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer => {
                return self.switch_explorer_picker(PickerKind::Files);
            }
            // Alt-l/h are per-kind: Explorer descends / ascends; Grep jumps the selection to
            // the next / previous file's first hit; elsewhere Alt-h clears (via picker_back).
            KeyCode::Char('l') if mods.alt && !mods.ctrl && p.kind == PickerKind::Explorer => {
                return self.explorer_enter_selected();
            }
            KeyCode::Char('l') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(Direction::Forward);
            }
            KeyCode::Char('h') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(Direction::Backward);
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
                return self.toggle_picker_filter(ChipId::Lit);
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
            KeyCode::Char('g') if mods.alt && !mods.ctrl => {
                return self.open_glob_prompt(None);
            }
            KeyCode::Char('d') if mods.alt && !mods.ctrl => {
                return self.open_dir_prompt(None);
            }
            KeyCode::PageUp => {
                return self.picker_move(-(VISIBLE_ROWS as i64 - 1));
            }
            KeyCode::PageDown => {
                return self.picker_move(VISIBLE_ROWS as i64 - 1);
            }
            // LspServers: Ctrl-r restarts the highlighted server in place.
            KeyCode::Char('r') if mods.ctrl && !mods.alt && p.kind == PickerKind::LspServers => {
                if let Some(PickerItem::LspServer { name, language, .. }) = p.selected_item() {
                    let (name, language) = (name.clone(), language.clone());

                    let mut fx = self.request::<LspRestartServer>(
                        LspRestartServerParams { language },
                        move |__r| {
                            let _ = __r;
                            Event::Noop
                        },
                    );
                    fx.push(Effect::Toast(format!("restarting {name}"), ToastKind::Info));
                    return fx;
                }
                return Effects::none();
            }
            // `Backspace` at the start of the query selects the rightmost chip (a second
            // press deletes it — two-stage, so holding backspace through a query can't
            // silently destroy a carefully typed glob).
            KeyCode::Backspace if no_chord => {
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.query.remove(i);
                    p.cursor = i;
                    return self.picker_query_changed();
                }
                let n = p.chip_row(&project_paths).len();
                if n > 0 {
                    p.chip_selected = Some(n - 1);
                }
                return Effects::none();
            }
            // `Left` at the start of the query steps into the chip row (rightmost first) —
            // the browser tag-input gesture.
            KeyCode::Left if no_chord => {
                if let Some((i, _)) = p.query[..p.cursor].char_indices().last() {
                    p.cursor = i;
                } else {
                    let n = p.chip_row(&project_paths).len();
                    if n > 0 {
                        p.chip_selected = Some(n - 1);
                    }
                }
                return Effects::none();
            }
            KeyCode::Right if no_chord => {
                if let Some(c) = p.query[p.cursor..].chars().next() {
                    p.cursor += c.len_utf8();
                }
                return Effects::none();
            }
            _ => {}
        }
        if no_chord {
            if let Some(typed) = text {
                let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
                if !typed.is_empty() {
                    let at = p.cursor;
                    p.query.insert_str(at, &typed);
                    p.cursor = at + typed.len();
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
        let project_paths = self.project_paths.clone();
        let labels = super::labels::root_labels(&project_paths);
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        let Some(ed) = p.chip_editor.as_mut() else {
            return Effects::none();
        };
        let is_dir = ed.is_dir();
        let multi_root_dir = is_dir && project_paths.len() > 1;
        let in_root = multi_root_dir && ed.field == ChipEditorField::Root;
        let no_chord = !mods.ctrl && !mods.alt;
        // Whether the path field's suggestion listing went stale and needs a directory/list.
        let mut refresh = false;
        match code {
            KeyCode::Enter if no_chord => return self.commit_chip_editor(),
            KeyCode::Esc => {
                p.chip_editor = None;
                return Effects::none();
            }
            // Tab / Alt-l: accept the focused segment's suggestion. Root — adopt the ghost
            // completion and continue right into the path; path — absorb the ghost directory
            // segment (repeated presses walk down the tree).
            KeyCode::Tab if no_chord && is_dir => {
                if in_root {
                    refresh = ed.commit_root_field(&labels, &project_paths);
                } else {
                    refresh = ed.accept_path_suggestion(&project_paths);
                }
            }
            KeyCode::Char('l') if mods.alt && !mods.ctrl && is_dir => {
                if in_root {
                    refresh = ed.commit_root_field(&labels, &project_paths);
                } else {
                    refresh = ed.accept_path_suggestion(&project_paths);
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
                    refresh = ed.commit_root_field(&labels, &project_paths);
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
                        refresh = ed.pop_path_segment(&project_paths);
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
            // Cycle the focused segment's matches: root typeahead candidates (wrapping), or
            // the path field's directory suggestions (clamped). Glob: no-op — reserved for
            // input history, matching the search bar.
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
                        refresh = ed.sync_dir_listing(&project_paths);
                    }
                } else if is_dir {
                    ed.cycle_path_suggestion(down);
                }
            }
            KeyCode::Backspace if no_chord => {
                if in_root {
                    if ed.root_filter.backspace() {
                        // The match set changed under the highlight — snap back to the best
                        // match; the chosen root may have moved under existing path text.
                        ed.root_selected = 0;
                        refresh = ed.sync_dir_listing(&project_paths);
                    }
                } else if ed.input.backspace() && is_dir {
                    refresh = ed.path_edited(&project_paths);
                }
            }
            KeyCode::Left if no_chord => {
                if in_root {
                    ed.root_filter.move_left();
                } else {
                    ed.input.move_left();
                }
            }
            KeyCode::Right if no_chord => {
                if in_root {
                    ed.root_filter.move_right();
                } else {
                    ed.input.move_right();
                }
            }
            _ => {
                if no_chord {
                    if let Some(typed) = text {
                        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
                        if !typed.is_empty() {
                            if in_root {
                                ed.root_filter.insert_str(&typed);
                                ed.root_selected = 0;
                                refresh = ed.sync_dir_listing(&project_paths);
                            } else {
                                ed.input.insert_str(&typed);
                                if is_dir {
                                    refresh = ed.path_edited(&project_paths);
                                }
                            }
                        }
                    }
                }
            }
        }
        if refresh {
            return self.refresh_chip_editor_listing();
        }
        Effects::none()
    }

    /// Jump the grep picker's selection to the first hit of the next / previous file. The
    /// server finds the boundary across the *whole* result list (so it works past the
    /// over-fetch window); the result lands as [`Event::GrepFileJumped`].
    fn grep_jump_file(&mut self, direction: Direction) -> Effects {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.kind != PickerKind::Grep || p.items.is_empty() {
            return Effects::none();
        }

        self.request::<PickerGrepFileJump>(
            PickerGrepFileJumpParams {
                from_index: p.selected,
                direction,
            },
            move |__r| Event::GrepFileJumped(__r.map_err(|e| e.to_string())),
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
                Effects::one(Effect::WindowAdopted)
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
                let was_external = self.externally_modified || self.externally_deleted;
                self.externally_modified = p.externally_modified;
                self.externally_deleted = p.externally_deleted;
                if !was_external && p.externally_deleted {
                    Effects::toast(
                        "file removed on disk — save to recreate, or close",
                        ToastKind::Warning,
                    )
                } else if !was_external && p.externally_modified {
                    Effects::toast(
                        "file changed on disk — save to overwrite, or reload",
                        ToastKind::Warning,
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
                    if let Some(p) = &mut self.picker {
                        if p.apply_update(u) && p.pending_center.is_none() {
                            if let Some(reveal) = p.reveal_on_update.take() {
                                return Effects::one(Effect::RevealPickerSelection(reveal));
                            }
                        }
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
                if let Ok(s) = serde_json::from_value::<LspServerStatus>(n.params) {
                    let matches = self.buffer.lsp_server.as_ref().is_some_and(|r| {
                        r.language == s.language && r.workspace_root == s.workspace_root
                    });
                    if matches {
                        self.lsp = Some(s);
                    }
                }
                Effects::none()
            }
            BufferClosed::NAME => {
                // Another client (or a path/project deletion) closed a buffer; if it's ours,
                // switch to the server-indicated next buffer (or a fresh scratch).
                let Ok(p) = serde_json::from_value::<BufferClosedParams>(n.params) else {
                    return Effects::none();
                };
                if p.buffer_id != self.buffer.buffer_id {
                    return Effects::none();
                }
                let mut fx = Effects::toast("buffer closed by another client", ToastKind::Warning);

                fx = fx.and(self.request::<BufferOpen>(
                    BufferOpenParams {
                        buffer_id: p.next_buffer_id,
                        ..Default::default()
                    },
                    move |__r| Event::Switched(__r.map_err(|e| e.to_string())),
                ));
                fx
            }
            _ => Effects::none(),
        }
    }

    // ---- search ----------------------------------------------------------------------------

    /// `/` or `?`: open the search prompt. Snapshots cursor/query for Esc-restore (the shell
    /// anchors its scroll via the effect) and clears the server-side search so stale
    /// highlights disappear immediately.
    pub fn enter_search(&mut self, extend_to_cursor: bool) -> Effects {
        self.search.snapshot = Some(SearchSnapshot {
            cursor: self.buffer.cursor,
            query: std::mem::take(&mut self.search.query),
            active: self.search.active,
        });
        self.search.active = false;
        self.search.summary = None;
        self.search.history_cursor = None;
        self.search.history_draft.clear();
        self.search.extend_to_cursor = extend_to_cursor;
        self.search.cursor = 0;
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
        if self.conn != ConnState::Connected {
            return Effects::none();
        }
        let anchor = if extend { self.buffer.cursor.anchor } else { pos };
        self.drag = Some((anchor, granularity));
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
        if self.conn != ConnState::Connected {
            return Effects::none();
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
        self.mode = Mode::Normal;
        self.search.extend_to_cursor = false;
        self.search.history_cursor = None;
        self.search.history_draft.clear();
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
        self.search.cursor = snap.query.len();
        self.search.query = snap.query;
        self.search.active = snap.active;

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

    /// Enter in the prompt: keep the query as the committed search.
    pub fn commit_search(&mut self) -> Effects {
        self.search.snapshot = None;
        if self.search.query.is_empty() {
            self.search.active = false;
            self.search.summary = None;
        } else {
            self.search.active = true;
            let q = self.search.query.clone();
            self.push_history(q);
        }
        self.search.history_cursor = None;
        self.search.history_draft.clear();
        self.search.extend_to_cursor = false;
        self.mode = Mode::Normal;
        Effects::none()
    }

    /// `n`/`Alt-n`: step match-to-match; with no active search, revive the most recent
    /// history entry first. Steps run sequentially in one future.
    pub fn search_cycle(&mut self, direction: Direction, count: u32, extend: bool) -> Effects {
        let revive = if self.search.active {
            None
        } else {
            match self.search.history.last().cloned() {
                Some(q) => {
                    self.search.cursor = q.len();
                    self.search.query = q.clone();
                    self.search.active = true;
                    Some(q)
                }
                None => return Effects::none(),
            }
        };
        // Revive + count ride the nav RPC itself (docs/protocol-composites.md, I): the
        // server re-sets the query first (skipping the step when it has no matches), then
        // steps `count` times.
        let params = SearchNavParams {
            buffer_id: self.buffer.buffer_id,
            extend,
            count,
            set_query: revive,
        };
        match direction {
            Direction::Forward => self.request_str::<SearchNext>(params, Event::SearchNav),
            Direction::Backward => self.request_str::<SearchPrev>(params, Event::SearchNav),
        }
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

    /// `<`/`>`: step through cached grep hits — navigate, open transient at the hit,
    /// record nav, prime search, all one server-side composite
    /// (docs/protocol-composites.md, J).
    pub fn grep_navigate(&mut self, direction: Direction) -> Effects {
        self.request_str::<PickerGrepNavigate>(
            PickerGrepNavigateParams {
                direction,
                buffer_id: self.buffer.buffer_id,
                open: true,
            },
            |r| {
                Event::SwitchedPrimed(
                    r.map(|target| target.and_then(|t| t.opened.map(|open| (t.query, open)))),
                )
            },
        )
    }

    // ---- Explorer/Files create + delete --------------------------------------------------

    /// Trash the highlighted Files/Explorer entry, behind a `[y/N]` confirm in the status bar.
    /// The absolute path comes from the picker's listed directory (Explorer) or the entry's
    /// project root (Files). The picker stays open under the confirm; on accept the `path/delete`
    /// fires and the listing re-lists (a close of *our* buffer rides the `buffer/closed` push).
    pub fn picker_stage_delete(&mut self) -> Effects {
        let staged = {
            let Some(p) = &self.picker else {
                return Effects::none();
            };
            let Some(item) = p.selected_item() else {
                return Effects::none();
            };
            match item {
                PickerItem::DirEntry { name, is_dir, .. } => p.directory.as_ref().map(|dir| {
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
                } => self
                    .project_paths
                    .get(*path_index as usize)
                    .map(|root| {
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
            message: format!("Delete {noun} \"{name}\"? [y/N]"),
            action: ConfirmAction::DeletePath { path, noun },
        });
        Effects::none()
    }

    /// Explorer `Ctrl-n`: create whatever the query names in the listed directory — a directory
    /// when it ends with `/`, otherwise a file (which opens). Multi-segment names create the
    /// intermediate directories server-side. No-op outside the Explorer.
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
            return Effects::error("type a name to create");
        }
        if base
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return Effects::error("invalid name");
        }
        let abs = format!("{}/{base}", dir.trim_end_matches('/'));
        if is_dir {
            return self
                .request_str::<DirectoryCreate>(DirectoryCreateParams { path: abs }, Event::DirCreated);
        }
        // File: address it under a project root, then open with create-on-save.
        let Some((path_index, relative_path)) = strip_longest_root(&abs, &self.project_paths) else {
            return Effects::error("path is outside the project's roots");
        };
        let from = self.buffer.buffer_id;
        self.request_str::<BufferOpen>(
            BufferOpenParams {
                path_index: Some(path_index),
                relative_path: Some(relative_path),
                create_if_missing: true,
                record_nav_from: Some(from),
                ..Default::default()
            },
            Event::Switched,
        )
    }

    /// Keys in the search prompt: the Search keymap table first, then printable input.
    pub fn on_search_key(&mut self, code: KeyCode, mods: Mods, text: Option<String>) -> Effects {
        if let Some(b) = lookup(KeyContext::Search, code, mods) {
            return self.search_action(b.action);
        }
        if mods.ctrl || mods.alt {
            return Effects::none();
        }
        let Some(text) = text else {
            return Effects::none();
        };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() {
            return Effects::none();
        }
        let at = self.search.cursor;
        self.search.query.insert_str(at, &text);
        self.search.cursor = at + text.len();
        self.search.history_cursor = None;
        self.incremental_search()
    }

    /// The Search-table actions (also reachable from the shell's action dispatch).
    pub fn search_action(&mut self, action: Action) -> Effects {
        match action {
            Action::SearchCommit => self.commit_search(),
            Action::SearchAbort => self.abort_search(),
            Action::SearchHistoryPrev => {
                self.history_up();
                self.incremental_search()
            }
            Action::SearchHistoryNext => {
                self.history_down();
                self.incremental_search()
            }
            Action::SearchCursorLeft => {
                if let Some((i, _)) = self.search.query[..self.search.cursor]
                    .char_indices()
                    .last()
                {
                    self.search.cursor = i;
                }
                Effects::none()
            }
            Action::SearchCursorRight => {
                if let Some(c) = self.search.query[self.search.cursor..].chars().next() {
                    self.search.cursor += c.len_utf8();
                }
                Effects::none()
            }
            Action::SearchBackspace => {
                let Some((i, _)) = self.search.query[..self.search.cursor]
                    .char_indices()
                    .last()
                else {
                    return Effects::none();
                };
                self.search.query.remove(i);
                self.search.cursor = i;
                self.search.history_cursor = None;
                self.incremental_search()
            }
            _ => Effects::none(),
        }
    }

    fn history_up(&mut self) {
        if self.search.history.is_empty() {
            return;
        }
        let idx = match self.search.history_cursor {
            None => {
                self.search.history_draft = self.search.query.clone();
                self.search.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.search.history_cursor = Some(idx);
        self.search.query = self.search.history[idx].clone();
        self.search.cursor = self.search.query.len();
    }

    fn history_down(&mut self) {
        match self.search.history_cursor {
            None => {} // already past the newest entry
            Some(i) if i + 1 < self.search.history.len() => {
                self.search.history_cursor = Some(i + 1);
                self.search.query = self.search.history[i + 1].clone();
                self.search.cursor = self.search.query.len();
            }
            Some(_) => {
                self.search.history_cursor = None;
                self.search.query = std::mem::take(&mut self.search.history_draft);
                self.search.cursor = self.search.query.len();
            }
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) -> Effects {
        match action {
            ConfirmAction::Save { target } => self.save(target, true),
            ConfirmAction::ReloadDiscard => self.reload(true),
            ConfirmAction::CloseDiscard => self.close_buffer(),
            ConfirmAction::DeletePath { path, noun } => self.request_str::<PathDelete>(
                PathDeleteParams { path },
                move |result| Event::PathDeleted { noun, result },
            ),
        }
    }

    /// Declining a save-as overwrite returns to the path input (the TUI keeps the prompt
    /// open beneath the confirm); other declines just close the dialog.
    fn decline_confirm(&mut self, action: ConfirmAction) {
        if let ConfirmAction::Save {
            target: Some((path_index, input)),
        } = action
        {
            self.prompt = Some(Prompt::SaveAs {
                path_index,
                cursor: input.len(),
                input,
            });
        }
    }

    /// The prompt's Yes/Save button.
    fn accept_prompt(&mut self) -> Effects {
        match self.prompt.take() {
            Some(Prompt::Confirm { action, .. }) => self.run_confirm(action),
            Some(p @ Prompt::SaveAs { .. }) => {
                // Submit via the same path as Enter.
                self.prompt = Some(p);
                self.on_prompt_key(KeyCode::Enter, Mods::default(), None)
            }
            Some(Prompt::LspInfo(_)) | None => Effects::none(),
        }
    }

    /// Dismiss the prompt without accepting (Esc / backdrop click).
    pub fn decline_prompt(&mut self) {
        if let Some(Prompt::Confirm { action, .. }) = self.prompt.take() {
            self.decline_confirm(action);
        }
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

    pub fn on_key(
        &mut self,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
        visible_rows: u32,
    ) -> Effects {
        if self.conn != ConnState::Connected {
            return Effects::none(); // editing input is suspended while the connection is down
        }

        // An open modal prompt owns the keyboard outright; a picker likewise.
        if self.prompt.is_some() {
            let fx = self.on_prompt_key(code, mods, text);
            return fx;
        }
        if self.picker.is_some() {
            let fx = self.on_picker_key(code, mods, text);
            return fx;
        }

        // Search mode owns the keyboard: control keys via its table, anything printable is
        // query text (case-preserved — no normalisation of the literal query).
        if self.mode == Mode::Search {
            let fx = self.on_search_key(code, mods, text);
            return fx;
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
            Pending::Leader => {
                self.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::Leader, code, mods) {
                    return self.run_action(b.action, 1, mods.shift, visible_rows);
                }
                return Effects::none();
            }
            Pending::None => {}
        }

        // Count lexer (Normal mode): digits accumulate; `0` only continues a count (it's
        // line-start otherwise).
        if self.mode == Mode::Normal && !mods.ctrl && !mods.alt {
            if let KeyCode::Char(c) = code {
                if c.is_ascii_digit() && (c != '0' || self.count.is_some()) {
                    let d = c.to_digit(10).unwrap();
                    self.count = Some(self.count.unwrap_or(0).saturating_mul(10) + d);
                    return Effects::none();
                }
            }
        }
        let count = self.count.take().unwrap_or(1).max(1);
        let extend = mods.shift;

        // Global table first (mode-identical Ctrl shortcuts), then the mode's own.
        let ctx = match self.mode {
            Mode::Normal => KeyContext::Normal,
            Mode::Insert => KeyContext::Insert,
            Mode::Search => return Effects::none(), // handled above
        };
        if let Some(b) = lookup(KeyContext::Global, code, mods).or_else(|| lookup(ctx, code, mods))
        {
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
        let task = self.dispatch_action(action, count, extend, visible_rows);
        // Remember the action for `r`/`Shift-r` to replay. Recorded at dispatch (the TUI records
        // after a successful await; here the RPC is still in flight — a failed motion leaves a
        // harmless no-op target). `RepeatMotion` itself isn't repeatable, so it never overwrites
        // the target with itself; find records its resolved motion at the capture site instead.
        if action.is_repeatable() {
            self.last_repeat = Some(RepeatTarget::Action { action, count });
        }
        task
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
        match action {
            // ---- motions ----
            A::MoveChar(direction) => self.move_motion(Motion::Char { direction, count }, extend),
            A::MoveWord { dir, boundary } => self.move_motion(
                Motion::Word {
                    direction: dir,
                    count,
                    boundary,
                    exclusive: dir == Direction::Forward && extend,
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
                    self.window
                        .as_ref()
                        .map(|w| w.line_count.saturating_sub(1))
                        .unwrap_or(0)
                } else {
                    count.saturating_sub(1)
                };
                self.move_motion(
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
            A::NavUnit(Direction::Forward) => self.move_motion(Motion::NextNavigationUnit, false),
            A::NavUnit(Direction::Backward) => self.move_motion(Motion::PrevNavigationUnit, false),
            A::NavUnitEdge { start: false } => self.move_motion(Motion::EndOfNavigationUnit, true),
            A::NavUnitEdge { start: true } => self.move_motion(Motion::StartOfNavigationUnit, true),
            A::BeginFind { dir, till } => {
                self.pending = Pending::Find {
                    dir,
                    till,
                    extend,
                    count,
                };
                Effects::none()
            }

            // ---- selection ----
            A::SelectLine(direction) => self.request_str::<CursorSelectLine>(
                CursorSelectLineParams {
                    buffer_id,
                    direction,
                    extend,
                    count,
                },
                Event::CursorMsg,
            ),
            A::SwapAnchor => self.request_str::<CursorSwapAnchor>(
                CursorSwapAnchorParams { buffer_id },
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
            A::TreeExpand => self.repeat_cursor::<CursorExpand>(count),
            A::TreeContract => self.repeat_cursor::<CursorContract>(count),
            A::MotionUndo => self.motion_history::<CursorUndo>(count),
            A::MotionRedo => self.motion_history::<CursorRedo>(count),
            A::RepeatMotion => {
                // `r`'s own count is how many times to replay; the stored target keeps the
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
            A::CenterCursor | A::Scroll { .. } | A::ToggleWrap => {
                // Geometry (pixel scroll, cell metrics) and viewport plumbing — the shell
                // executes these against its own state.
                Effects::one(Effect::ShellAction(action))
            }
            A::OpenHelp | A::OpenProjectSettings => {
                // Shell-local overlays (help cheatsheet, project settings). A shell without
                // the overlay ignores the action.
                Effects::one(Effect::ShellAction(action))
            }
            A::NavBack | A::NavForward => {
                let forward = matches!(action, A::NavForward);
                let f = move |res: Result<NavStepResult, RpcError>| Event::NavDone {
                    forward,
                    result: res.map_err(|e| e.to_string()),
                };
                if forward {
                    self.request::<NavForward>(NavStepParams { buffer_id }, f)
                } else {
                    self.request::<NavBack>(NavStepParams { buffer_id }, f)
                }
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
            A::NewlineIndent => self.edit::<InputNewlineAndIndent>(BufferOnlyParams { buffer_id }),
            A::InsertTab => self.edit::<InputText>(InputTextParams {
                buffer_id,
                text: "\t".into(),
                select_pasted: false,
                at: None,
            }),
            A::DeletePoint => self.edit::<InputDelete>(CountedEditParams {
                buffer_id,
                count: 1,
            }),
            A::DeleteSelection => self.repeat_edit::<InputDelete>(count),
            A::DeleteLine => self.edit::<InputDeleteLine>(BufferOnlyParams { buffer_id }),
            A::Undo => self.undo_redo::<InputUndo>(count),
            A::Redo => self.undo_redo::<InputRedo>(count),
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
            A::ToggleComment => self.edit::<InputToggleComment>(BufferOnlyParams { buffer_id }),
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
            A::CutLine => self.cut(CopyScope::Line),
            A::Paste => read_clipboard_fx(PasteKind::Before { count }),
            A::ReplaceClipboard => read_clipboard_fx(PasteKind::Replace { count }),
            A::PasteAtCursor => read_clipboard_fx(PasteKind::AtCursor),
            A::ReplaceLineClipboard => read_clipboard_fx(PasteKind::Line),
            A::Change => {
                self.mode = Mode::Insert;
                self.edit::<InputDelete>(CountedEditParams {
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

            // ---- search (core methods; the prompt-only actions also route here from
            // `Session::on_search_key`'s table lookup) ----
            A::EnterSearch => self.enter_search(false),
            A::EnterSearchToCursor => self.enter_search(true),
            A::SearchCommit
            | A::SearchAbort
            | A::SearchHistoryPrev
            | A::SearchHistoryNext
            | A::SearchCursorLeft
            | A::SearchCursorRight
            | A::SearchBackspace => self.search_action(action),
            A::SearchCycle(direction) => self.search_cycle(direction, count, extend),
            A::SearchFromSelection => self.search_from_selection(),
            A::GrepNavigate(direction) => self.grep_navigate(direction),
            A::DropSearch => self.drop_search(),

            // ---- app ----
            // The server tears down all per-client state on disconnect, so quitting is just
            // closing the window.
            A::Quit => Effects::one(Effect::Exit),
            A::Save => self.save(None, false),
            A::SaveAs => {
                // Prefill with the buffer's current project-relative path, like the web dialog.
                let (path_index, input) = self
                    .buffer
                    .path
                    .as_deref()
                    .and_then(|p| strip_longest_root(p, &self.project_paths))
                    .unwrap_or((0, String::new()));
                self.prompt = Some(Prompt::SaveAs {
                    path_index,
                    cursor: input.len(),
                    input,
                });
                Effects::none()
            }
            A::Reload => {
                if self.buffer.path.is_none() {
                    return Effects::toast(
                        "scratch buffer has no path to reload",
                        ToastKind::Warning,
                    );
                }
                self.reload(false)
            }
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
                        message: format!("discard unsaved changes in {}", self.buffer.label),
                        action: ConfirmAction::CloseDiscard,
                    });
                    return Effects::none();
                }

                self.close_buffer()
            }

            // ---- git ----
            A::ToggleDiffView => {
                let Some(viewport_id) = self.viewport_id else {
                    return Effects::none();
                };
                let enabled = !self.diff_view;
                self.request_str::<GitSetDiffView>(
                    GitSetDiffViewParams {
                        viewport_id,
                        enabled,
                    },
                    move |result| Event::DiffViewSet { enabled, result },
                )
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
            A::OpenPicker(kind) => self.open_picker(kind, None, None),
            A::OpenPickerInBufferDir(kind) => self.open_picker_in_buffer_dir(kind),
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
                        from_line: self.buffer.cursor.position.line,
                        direction,
                    },
                    Event::DiagNav,
                )
            }
        }
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
    fn repeat_cursor<M>(&mut self, count: u32) -> Effects
    where
        M: RpcMethod<Params = CursorBufferOnlyParams, Result = CursorState> + 'static,
    {
        self.request_str::<M>(
            CursorBufferOnlyParams {
                buffer_id: self.buffer.buffer_id,
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
        M: RpcMethod<Params = CountedEditParams, Result = UndoResult> + 'static,
    {
        self.request_str::<M>(
            CountedEditParams {
                buffer_id: self.buffer.buffer_id,
                count,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the TUI's seeded_filters_for_switch tests: the explorer's visibility filters
    // invert for Grep (its walk excludes what the listing shows), and Files takes only
    // dir + changed-only.
    #[test]
    fn explorer_switch_translates_filters() {
        let scope = ScopedPath {
            path_index: 0,
            relative_path: "src".into(),
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

        // Roots mode: no dir scope — the target covers the whole project.
        let seeded = seeded_filters_for_switch(&defaults, None, PickerKind::Grep);
        assert!(seeded.directories.is_empty());
    }
}
