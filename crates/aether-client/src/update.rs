//! The core update function, grown arm by arm (docs/client-core.md phase 3): each migrated
//! subsystem moves its `Message` variants into [`Event`], its handler logic into
//! [`Session::on_event`], and its RPC chains into effect-returning methods here. The shell
//! bridges with a single `Message::Core(Event)` variant and an effect executor.

use super::chips::{self, ChipEditor, ChipEditorField, ChipId};
use super::effect::{Effect, Effects, ToastKind};
use super::keymap::{lookup, Action, InsertWhere, KeyCode, KeyContext, Mods};
use super::picker::{item_key, DefaultSkip, PickerState, Reveal, FETCH_LIMIT, VISIBLE_ROWS};
use super::session::{
    buffer_info, max_pos, min_pos, severity_label, strip_longest_root, CommitDetails,
    ConfirmAction, ConnState, HoverBlock, HoverText, Mode, PasteKind, Pending, Prompt,
    ReloadTry, RepeatTarget, SaveTry, SearchSnapshot, SearchState, Session,
};
use super::transport::{rpc, SharedTransport};
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferClosed, BufferClosedParams, BufferCopy,
    BufferCopyParams, BufferCopyResult, BufferCut, BufferCutResult, BufferOpen,
    BufferOpenParams, BufferOpenResult, BufferReload, BufferReloadParams, BufferSave,
    BufferSaveParams, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::envelope::{Notification, NotificationMethod};
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorContract, CursorExpand, CursorMove, CursorMoveParams,
    CursorRedo, CursorSelectLine, CursorSelectLineParams, CursorSet, CursorSetParams,
    CursorState, CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo, CursorUndoParams,
    Granularity, Motion,
};
use aether_protocol::envelope::RpcMethod;
use aether_protocol::error::ErrorCode;
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputBackspace, InputChangeLine, InputDedent, InputDelete,
    InputDeleteLine, InputIndent, InputJoinLines, InputMoveLines, InputMoveLinesParams,
    InputNewlineAndIndent, InputRedo, InputReplaceLine, InputReplaceLineParams, InputSurround,
    InputSurroundParams, InputText, InputTextParams, InputToggleComment, InputUndo,
    InputUnsurround, InputUnsurroundParams, UndoResult,
};
use aether_protocol::cursor::Direction;
use aether_protocol::git::{
    ApplyHunkStatus, GitApplyHunk, GitApplyHunkParams, GitApplyHunkResult, GitBlameLine,
    GitBlameLineParams, GitCommitInfo, GitCommitInfoParams, GitNavigateHunk,
    GitNavigateHunkParams, GitNavigateHunkResult, GitSetDiffView, GitSetDiffViewParams,
    HunkAction, HunkDirection,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspBufferParams,
    LspDiagnosticsChanged, LspDiagnosticsChangedParams, LspFormat, LspFormatResult,
    LspGotoDefinition, LspGotoDefinitionResult, LspHover, LspHoverResult,
    LspNavigateDiagnostic, LspNavigateDiagnosticParams, LspNavigateDiagnosticResult,
    LspRestartServer, LspRestartServerParams, LspServerStatus, LspStatusChanged,
};
use aether_protocol::nav::NavStepResult;
use aether_protocol::viewport::{
    DiagnosticSeverity, ViewportLinesChanged, ViewportLinesChangedParams, ViewportWindowResult,
    Window,
};
use aether_protocol::nav::{NavBack, NavForward, NavRecord, NavRecordParams, NavStepParams};
use aether_protocol::directory::{DirectoryList, DirectoryListParams, DirectoryListResult};
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
    pub fn on_event(&mut self, event: Event, t: &SharedTransport) -> Effects<Event> {
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
                self.paste(t, kind, text)
            }

            Event::Switched(Ok(open)) => self.adopt_switch(open),
            Event::Switched(Err(e)) => Effects::error(e),

            Event::SwitchedPrimed(Ok(Some((query, open)))) => {
                let fx = self.adopt_switch(open);
                // adopt_switch reset the search state; adopt the primed query (the
                // server-side search was already set in the open chain).
                self.search.cursor = query.len();
                self.search.query = query.clone();
                self.search.active = true;
                self.push_history(query);
                fx
            }
            Event::SwitchedPrimed(Ok(None)) => {
                Effects::toast("no more grep hits", ToastKind::Info)
            }
            Event::SwitchedPrimed(Err(e)) => Effects::error(e),

            Event::PromptAccept => self.accept_prompt(t),
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
                    self.revert_to_snapshot_cursor(t)
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
                    .and(self.revert_to_snapshot_cursor(t))
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
                    self.open_path_primed(t, location.path, Some(location.position), None)
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
                    FormatStatus::NotReady => {
                        Some("language server still starting".to_string())
                    }
                    FormatStatus::Unavailable => {
                        Some("language server unavailable".to_string())
                    }
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
                if buffer_id == self.buffer.buffer_id
                    && line == self.buffer.cursor.position.line
                {
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
                PickerSelectResult::File { path } => self.open_path_primed(t, path, None, prime),
                PickerSelectResult::FileAt { path, position } => {
                    self.open_path_primed(t, path, Some(position), prime)
                }
                PickerSelectResult::Buffer { buffer_id } => {
                    if buffer_id == self.buffer.buffer_id {
                        return Effects::none(); // already showing it
                    }
                    let from = self.buffer.buffer_id;
                    let t = t.clone();
                    Effects::spawn(async move {
                        let _ = rpc::<NavRecord>(t.as_ref(), NavRecordParams { buffer_id: from })
                            .await;
                        Event::Switched(
                            rpc::<BufferOpen>(
                                t.as_ref(),
                                BufferOpenParams {
                                    buffer_id: Some(buffer_id),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| e.to_string()),
                        )
                    })
                }
                PickerSelectResult::Project { name } => {
                    // Activate, then land on the project's last buffer (or a fresh
                    // transient scratch) — the bootstrap convention.
                    let t = t.clone();
                    Effects::spawn(async move {
                        Event::ProjectActivated(
                            async {
                                let activated = rpc::<ProjectActivate>(
                                    t.as_ref(),
                                    ProjectActivateParams { name },
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                                let open = rpc::<BufferOpen>(
                                    t.as_ref(),
                                    BufferOpenParams {
                                        buffer_id: activated.last_buffer_id,
                                        transient: if activated.last_buffer_id.is_none() {
                                            Some(true)
                                        } else {
                                            None
                                        },
                                        ..Default::default()
                                    },
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                                Ok((activated.project, open))
                            }
                            .await,
                        )
                    })
                }
            },
            Event::PickerSelected {
                result: Err(e), ..
            } => Effects::error(format!("select failed: {e}")),

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
                self.picker_accept(t)
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
                let fut = rpc::<PickerView>(
                    t.as_ref(),
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
                );
                Effects::spawn(async move {
                    Event::PickerViewed {
                        initial: false,
                        result: fut.await.map_err(|e| e.to_string()),
                    }
                })
            }
            Event::GrepFileJumped(Err(e)) => Effects::error(format!("file jump failed: {e}")),

            Event::ServerPush(n) => self.on_server_push(t, n),

            Event::ConnectionLost => {
                if self.conn != ConnState::Connected {
                    return Effects::none(); // already reconnecting (a late echo)
                }
                self.conn = ConnState::Reconnecting {
                    attempt: 0,
                    had_unsaved: self.buffer.revision != self.buffer.saved_revision,
                };
                tracing::warn!(buffer = %self.buffer.label, "connection lost; reconnecting");
                let mut fx = Effects::toast(
                    "server disconnected — reconnecting…",
                    ToastKind::Warning,
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
                self.sent_grid = None;
                self.fetch_in_flight = false;
                self.refetch_queued = false;
                self.blame = None;
                self.blame_requested = None;
                self.prompt = None;
                self.picker = None;
                let buffer_id = self.buffer.buffer_id;
                let mut fx = Effects::one(Effect::Resubscribe);
                // Restore a selection (jump_to only carried the cursor): same buffer only,
                // and a failure (the file shrank on disk) keeps the server's default.
                if same_file && old_cursor.anchor != old_cursor.position {
                    let fut = rpc::<CursorSet>(
                        t.as_ref(),
                        CursorSetParams {
                            buffer_id,
                            position: old_cursor.position,
                            anchor: old_cursor.anchor,
                            granularity: Granularity::Char,
                        },
                    );
                    fx.push(Effect::Spawn(Box::pin(async move {
                        match fut.await {
                            Ok(c) => Event::CursorMsg(Ok(c)),
                            Err(_) => Event::Noop,
                        }
                    })));
                }
                // Re-prime a committed search so highlights and `n` survive the drop.
                if same_file && self.search.active && !self.search.query.is_empty() {
                    let fut = rpc::<SearchSet>(
                        t.as_ref(),
                        SearchSetParams {
                            buffer_id,
                            query: self.search.query.clone(),
                            anchor: None,
                            extend: false,
                        },
                    );
                    fx.push(Effect::Spawn(Box::pin(async move {
                        Event::SearchRestored(fut.await.map_err(|e| e.to_string()))
                    })));
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
    pub fn save(
        &self,
        t: &SharedTransport,
        target: Option<(u32, String)>,
        overwrite: bool,
    ) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let (path_index, relative_path) = match &target {
            Some((i, p)) => (Some(*i), Some(p.clone())),
            None => (None, None),
        };
        let fut = rpc::<BufferSave>(
            t.as_ref(),
            BufferSaveParams {
                buffer_id,
                path_index,
                relative_path,
                overwrite,
            },
        );
        Effects::spawn(async move {
            Event::SaveTried(match fut.await {
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
        })
    }

    /// Fire an edit RPC; the result lands as [`Event::EditDone`].
    pub fn edit<M>(&self, t: &SharedTransport, params: M::Params) -> Effects<Event>
    where
        M: RpcMethod<Result = EditResult> + 'static,
    {
        let fut = rpc::<M>(t.as_ref(), params);
        Effects::spawn(async move { Event::EditDone(fut.await.map_err(|e| e.to_string())) })
    }

    /// Insert clipboard text per the paste gesture. `Before` is a two-step chain (collapse
    /// to the selection start, then insert) — issued lazily in sequence off the shared
    /// transport, since calls enqueue when fired.
    pub fn paste(&self, t: &SharedTransport, kind: PasteKind, text: String) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        match kind {
            PasteKind::Before { count } => {
                let text = text.repeat(count.max(1) as usize);
                let start = min_pos(self.buffer.cursor.position, self.buffer.cursor.anchor);
                let t = t.clone();
                Effects::spawn(async move {
                    let set = rpc::<CursorSet>(
                        t.as_ref(),
                        CursorSetParams {
                            buffer_id,
                            position: start,
                            anchor: start,
                            granularity: Granularity::Char,
                        },
                    )
                    .await;
                    if let Err(e) = set {
                        return Event::EditDone(Err(e.to_string()));
                    }
                    Event::EditDone(
                        rpc::<InputText>(
                            t.as_ref(),
                            InputTextParams {
                                buffer_id,
                                text,
                                select_pasted: true,
                            },
                        )
                        .await
                        .map_err(|e| e.to_string()),
                    )
                })
            }
            PasteKind::Replace { count } => self.edit::<InputText>(
                t,
                InputTextParams {
                    buffer_id,
                    text: text.repeat(count.max(1) as usize),
                    select_pasted: true,
                },
            ),
            PasteKind::AtCursor => self.edit::<InputText>(
                t,
                InputTextParams {
                    buffer_id,
                    text,
                    select_pasted: false,
                },
            ),
            PasteKind::Line => {
                self.edit::<InputReplaceLine>(t, InputReplaceLineParams { buffer_id, text })
            }
        }
    }

    /// Rebind the session to a freshly opened buffer: reset all per-buffer state (modal,
    /// diagnostics, viewport binding, prompts/pickers — an externally-triggered switch can
    /// land mid-pick) and ask the shell to resubscribe. Search history survives switches.
    pub fn adopt_switch(&mut self, open: BufferOpenResult) -> Effects<Event> {
        self.mode = Mode::Normal;
        self.pending = Pending::None;
        self.count = None;
        self.diagnostics = DiagnosticCounts::default();
        self.lsp = None;
        self.externally_modified = false;
        self.externally_deleted = false;
        self.window = None;
        self.viewport_id = None;
        self.fetch_in_flight = false;
        self.refetch_queued = false;
        self.reveal_after_fetch = false;
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

    /// Close the buffer, then attach to the server-indicated next MRU buffer (or a fresh
    /// scratch).
    pub fn close_buffer(&self, t: &SharedTransport) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let t = t.clone();
        Effects::spawn(async move {
            Event::Switched(
                async {
                    let closed = rpc::<BufferClose>(t.as_ref(), BufferCloseParams { buffer_id })
                        .await
                        .map_err(|e| e.to_string())?;
                    rpc::<BufferOpen>(
                        t.as_ref(),
                        BufferOpenParams {
                            buffer_id: closed.next_buffer_id,
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())
                }
                .await,
            )
        })
    }

    /// Open a file by absolute path as a transient preview — result-style navigation (picker
    /// selections, goto-definition). Records the jump origin onto the nav history first.
    /// `prime_search` (grep flows) also sets the opened buffer's search to that query so
    /// `n`/`Alt-n` step matches.
    pub fn open_path_primed(
        &self,
        t: &SharedTransport,
        path: String,
        jump_to: Option<LogicalPosition>,
        prime_search: Option<String>,
    ) -> Effects<Event> {
        let Some((path_index, relative_path)) = strip_longest_root(&path, &self.project_paths)
        else {
            return Effects::error(format!("{path} is outside the project's roots"));
        };
        let buffer_id = self.buffer.buffer_id;
        let t = t.clone();
        Effects::spawn(async move {
            let _ = rpc::<NavRecord>(t.as_ref(), NavRecordParams { buffer_id }).await;
            let open = match rpc::<BufferOpen>(
                t.as_ref(),
                BufferOpenParams {
                    path_index: Some(path_index),
                    relative_path: Some(relative_path),
                    jump_to,
                    transient: Some(true),
                    ..Default::default()
                },
            )
            .await
            {
                Ok(open) => open,
                Err(e) => return Event::Switched(Err(e.to_string())),
            };
            match prime_search {
                Some(query) => {
                    let _ = rpc::<SearchSet>(
                        t.as_ref(),
                        SearchSetParams {
                            buffer_id: open.buffer_id,
                            query: query.clone(),
                            anchor: None,
                            extend: false,
                        },
                    )
                    .await;
                    Event::SwitchedPrimed(Ok(Some((query, open))))
                }
                None => Event::Switched(Ok(open)),
            }
        })
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
    pub fn on_prompt_key(
        &mut self,
        t: &SharedTransport,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Effects<Event> {
        let Some(prompt) = self.prompt.take() else {
            return Effects::none();
        };
        match prompt {
            Prompt::Confirm { message: _, action } => {
                let accepts = !mods.ctrl
                    && !mods.alt
                    && (code == KeyCode::Char('y') || code == KeyCode::Enter);
                if accepts {
                    self.run_confirm(t, action)
                } else {
                    self.decline_confirm(action);
                    Effects::none()
                }
            }
            Prompt::LspInfo(info) => {
                // `r` restarts; any other key closes the dialog.
                if code == KeyCode::Char('r') && !mods.ctrl && !mods.alt {
                    let fut = rpc::<LspRestartServer>(
                        t.as_ref(),
                        LspRestartServerParams {
                            language: info.language.clone(),
                        },
                    );
                    let mut fx = Effects::spawn(async move {
                        let _ = fut.await;
                        Event::Noop
                    });
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
                        return self.save(t, Some(target), false);
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
    pub fn show_diagnostic(&self) -> Effects<Event> {
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

    /// `Space o` — blame the cursor line on demand, then resolve the commit's full details.
    pub fn show_commit_info(&self, t: &SharedTransport) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let line = self.buffer.cursor.position.line;
        let t = t.clone();
        Effects::spawn(async move {
            Event::CommitLookup(
                async {
                    let blame =
                        rpc::<GitBlameLine>(t.as_ref(), GitBlameLineParams { buffer_id, line })
                            .await
                            .map_err(|e| e.to_string())?;
                    let info = match blame.blame {
                        Some(b) if b.is_uncommitted => {
                            return Ok(CommitDetails::Note(
                                "Uncommitted line — no commit details",
                            ))
                        }
                        Some(b) => b,
                        None => {
                            return Ok(CommitDetails::Note("No commit details for this line"))
                        }
                    };
                    let r = rpc::<GitCommitInfo>(
                        t.as_ref(),
                        GitCommitInfoParams {
                            buffer_id,
                            commit: info.commit,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    Ok(match r.info {
                        Some(info) => CommitDetails::Info(Box::new(info)),
                        None => CommitDetails::Note("Commit not found"),
                    })
                }
                .await,
            )
        })
    }

    // ---- pickers ----------------------------------------------------------------------------

    /// Open a picker: subscribe a window and let `picker/update` pushes fill it. Grep resumes
    /// its prior query/hits (centred on the cursor's nearest hit); the rest reset.
    /// `directory_path` seeds the Explorer's listing (its `Space e` = the buffer's directory).
    /// `seed_filters` replaces the server's persisted set (Explorer→Grep/Files switches,
    /// `Space Alt-f/g`); the echo through `PickerViewed` rebuilds the chip row.
    pub fn open_picker(
        &mut self,
        t: &SharedTransport,
        kind: PickerKind,
        directory_path: Option<String>,
        seed_filters: Option<PickerFilters>,
    ) -> Effects<Event> {
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
                let name = std::path::Path::new(path).file_name()?.to_str()?.to_string();
                Some(PickerItem::DirEntry {
                    name,
                    is_dir: false,
                    match_indices: Vec::new(),
                    git_status: None,
                })
            })
            .flatten();
        let fut = rpc::<PickerView>(
            t.as_ref(),
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
        );
        Effects::one(Effect::PickerScrollReset).and(Effects::spawn(async move {
            Event::PickerViewed {
                initial: true,
                result: fut.await.map_err(|e| e.to_string()),
            }
        }))
    }

    /// `Space Alt-f` / `Space Alt-g`: open Files/Grep pre-scoped to the active buffer's
    /// directory — a normal dir filter chip, visible and removable. Falls back to an unscoped
    /// open for scratch buffers or files outside every root.
    pub fn open_picker_in_buffer_dir(
        &mut self,
        t: &SharedTransport,
        kind: PickerKind,
    ) -> Effects<Event> {
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
        self.open_picker(t, kind, None, seed)
    }

    /// `Ctrl-g` / `Ctrl-f` in the Explorer: switch to the Grep / Files picker scoped to the
    /// directory being browsed ("grep here"), the explorer's filters translated along. In
    /// Roots mode no dir scope is seeded — the target covers the whole project.
    fn switch_explorer_picker(
        &mut self,
        t: &SharedTransport,
        target: PickerKind,
    ) -> Effects<Event> {
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
        let hide = self.close_picker(t);
        hide.and(self.open_picker(t, target, None, Some(seeded)))
    }

    /// `Space e` / `Space Alt-e`: Explorer at the buffer's directory, or at its project root.
    /// Scratch buffers fall through to the server default (last listing / first root).
    pub fn open_explorer(&mut self, t: &SharedTransport, at_root: bool) -> Effects<Event> {
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
        self.open_picker(t, PickerKind::Explorer, dir, None)
    }

    /// Explorer navigation: list a different directory (or the project roots). Clears the
    /// query — entering a directory starts a fresh listing — but the filter chips ride along.
    /// `pre_select` lands the highlight on the named entry once the listing arrives.
    fn explorer_navigate(
        &mut self,
        t: &SharedTransport,
        directory_path: Option<String>,
        roots: bool,
        pre_select: Option<String>,
    ) -> Effects<Event> {
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
        let clear_query = rpc::<PickerQuery>(
            t.as_ref(),
            PickerQueryParams {
                kind: PickerKind::Explorer,
                query: String::new(),
                generation,
                // The query RPC replaces the persisted filters too — carry the chips so a
                // racing arrival order can't wipe them under the view below.
                filters: filters.clone(),
            },
        );
        let view = rpc::<PickerView>(
            t.as_ref(),
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
        );
        let mut fx = Effects::one(Effect::PickerScrollReset);
        fx.push(Effect::Spawn(Box::pin(async move {
            let _ = clear_query.await;
            Event::Noop
        })));
        fx.push(Effect::Spawn(Box::pin(async move {
            Event::PickerViewed {
                initial: false,
                result: view.await.map_err(|e| e.to_string()),
            }
        })));
        fx
    }

    /// Move the picker highlight, refetching when it leaves the fetched window and revealing
    /// it otherwise (the shell scrolls the native list the minimum to keep it visible).
    fn picker_move(&mut self, t: &SharedTransport, delta: i64) -> Effects<Event> {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        match p.move_selection(delta) {
            Some(offset) => self.picker_refetch(t, offset),
            None => Effects::one(Effect::RevealPickerSelection(Reveal::Minimal)),
        }
    }

    /// Re-subscribe the picker's window at a new offset (the highlight moved past it).
    pub fn picker_refetch(&mut self, t: &SharedTransport, offset: u32) -> Effects<Event> {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.offset = offset;
        p.items.clear();
        let kind = p.kind;
        let fut = rpc::<PickerView>(
            t.as_ref(),
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
        );
        Effects::spawn(async move {
            Event::PickerViewed {
                initial: false,
                result: fut.await.map_err(|e| e.to_string()),
            }
        })
    }

    /// A query edit: bump the generation (stale pushes get discarded), restart the window at
    /// the top, and tell the server.
    fn picker_query_changed(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        p.generation += 1;
        p.selected = 0;
        p.offset = 0;
        // A query change invalidates any pending pre-selection (centering / skip-the-
        // active-item default) — the user is steering somewhere new.
        p.pending_center = None;
        p.default_skip = None;
        p.reveal_on_update = None;
        let (kind, query, generation) = (p.kind, p.query.clone(), p.generation);
        let filters = p.wire_filters();
        let fut = rpc::<PickerQuery>(
            t.as_ref(),
            PickerQueryParams {
                kind,
                query,
                generation,
                filters,
            },
        );
        let mut fx = Effects::spawn(async move {
            let _ = fut.await;
            Event::Noop
        });
        fx.push(Effect::PickerScrollReset);
        fx.and(self.picker_refetch(t, 0))
    }

    /// Push a filter (chip) change. For Grep/Files a filter change *is* a query change (same
    /// generation mechanics); for the Explorer the filters apply when the listing is built,
    /// so re-view the current directory with the replacement set. No-op for kinds that take
    /// no filters, and for the Explorer's Roots mode (nothing to filter there).
    fn apply_picker_filter_change(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(kind) = self.picker.as_ref().map(|p| p.kind) else {
            return Effects::none();
        };
        match kind {
            PickerKind::Grep | PickerKind::Files => self.picker_query_changed(t),
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
                let fut = rpc::<PickerView>(
                    t.as_ref(),
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
                );
                Effects::one(Effect::PickerScrollReset).and(Effects::spawn(async move {
                    Event::PickerViewed {
                        initial: false,
                        result: fut.await.map_err(|e| e.to_string()),
                    }
                }))
            }
            _ => Effects::none(),
        }
    }

    /// Toggle/cycle the filter a chord (or Enter on a selected chip) names, then push the
    /// change. A chord that doesn't apply to this picker kind is a clean no-op.
    fn toggle_picker_filter(&mut self, t: &SharedTransport, id: ChipId) -> Effects<Event> {
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
        self.apply_picker_filter_change(t)
    }

    /// `Enter` on a selected chip: valued chips re-open their editor pre-filled; everything
    /// else toggles/cycles in place (a plain boolean's chip disappears).
    fn edit_selected_chip(&mut self, t: &SharedTransport, id: ChipId) -> Effects<Event> {
        match id {
            ChipId::Glob(i) => self.open_glob_prompt(Some(i)),
            ChipId::Dir(i) => self.open_dir_prompt(t, Some(i)),
            _ => self.toggle_picker_filter(t, id),
        }
    }

    /// Open the glob editor line. `edit: Some(i)` pre-fills glob `i`; `None` adds a new chip
    /// on commit.
    fn open_glob_prompt(&mut self, edit: Option<usize>) -> Effects<Event> {
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
    fn open_dir_prompt(&mut self, t: &SharedTransport, edit: Option<usize>) -> Effects<Event> {
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
        self.refresh_chip_editor_listing(t)
    }

    /// Fire `directory/list` for the dir-chip editor's current (root, dir-portion) pair. The
    /// requested path rides on the result event so a stale response (the editor moved on)
    /// can be discarded. No-op for glob editors and invalid roots.
    fn refresh_chip_editor_listing(&mut self, t: &SharedTransport) -> Effects<Event> {
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
        let fut = rpc::<DirectoryList>(t.as_ref(), DirectoryListParams { path });
        Effects::spawn(async move {
            Event::PickerChipListing {
                abs,
                result: fut.await.map_err(|e| e.to_string()),
            }
        })
    }

    /// Commit the chip editor line. A dir editor only commits a *valid* scope — a root that
    /// matches some label and a path that exists (or is empty); otherwise the editor stays
    /// open with the invalid segment rendered red.
    fn commit_chip_editor(&mut self, t: &SharedTransport) -> Effects<Event> {
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
                    let path_index = if multi_root { ed.chosen_root(&labels) } else { 0 };
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
        self.apply_picker_filter_change(t)
    }

    /// Alt-l: descend into the highlighted explorer directory (Enter does too, via accept).
    fn explorer_enter_selected(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if let Some(PickerItem::DirEntry {
            name,
            is_dir: true,
            ..
        }) = p.selected_item()
        {
            let dir = match &p.directory {
                Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                None => return Effects::none(),
            };
            return self.explorer_navigate(t, Some(dir), false, None);
        }
        Effects::none()
    }

    /// Alt-h / Alt-Backspace: progressively unwind — clear the query, then pop the rightmost
    /// filter chip (one per press), then (explorer) one directory segment per press — landing
    /// the highlight on the directory just left — then roots mode in multi-root projects.
    fn picker_back(&mut self, t: &SharedTransport) -> Effects<Event> {
        let project_paths = self.project_paths.clone();
        let Some(p) = &mut self.picker else {
            return Effects::none();
        };
        if !p.query.is_empty() {
            p.query.clear();
            p.cursor = 0;
            return self.picker_query_changed(t);
        }
        if let Some(chip) = p.chip_row(&project_paths).last().map(|c| c.id) {
            chips::remove_chip(&mut p.chips, chip);
            p.chip_selected = None;
            return self.apply_picker_filter_change(t);
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
                self.explorer_navigate(t, Some(parent), false, leaving)
            }
            None if p.directory.is_some() => {
                if project_paths.len() > 1 {
                    self.explorer_navigate(t, None, true, None)
                } else {
                    Effects::none()
                }
            }
            None => Effects::none(),
        }
    }

    /// Enter / row click: act on the highlighted item. Directories and roots navigate within
    /// the open explorer; everything else closes the panel and runs `picker/select`.
    fn picker_accept(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        let Some(item) = p.selected_item().cloned() else {
            return Effects::none();
        };
        match &item {
            PickerItem::DirEntry {
                name,
                is_dir: true,
                ..
            } => {
                let dir = match &p.directory {
                    Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                    None => return Effects::none(),
                };
                return self.explorer_navigate(t, Some(dir), false, None);
            }
            PickerItem::Root { path_index, .. } => {
                let dir = self.project_paths.get(*path_index as usize).cloned();
                return self.explorer_navigate(t, dir, false, None);
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
                let hide = self.close_picker(t);
                self.prompt = Some(Prompt::LspInfo(Box::new(info)));
                return hide;
            }
            _ => {}
        }
        let kind = p.kind;
        let prime = (kind == PickerKind::Grep).then(|| p.query.clone());
        let hide = self.close_picker(t);
        let fut = rpc::<PickerSelect>(t.as_ref(), PickerSelectParams { kind, item });
        hide.and(Effects::spawn(async move {
            Event::PickerSelected {
                prime,
                result: fut.await.map_err(|e| e.to_string()),
            }
        }))
    }

    /// Drop the panel and unsubscribe (the server keeps walker/matcher state for resume).
    pub fn close_picker(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(p) = self.picker.take() else {
            return Effects::none();
        };
        let fut = rpc::<PickerHide>(t.as_ref(), PickerHideParams { kind: p.kind });
        Effects::spawn(async move {
            let _ = fut.await;
            Event::Noop
        })
    }

    /// Keys while a picker is open: list navigation + query editing.
    pub fn on_picker_key(
        &mut self,
        t: &SharedTransport,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Effects<Event> {
        // The chip editor line (glob/dir, revealed below the input) owns the keys while open.
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.chip_editor.is_some())
        {
            return self.on_chip_editor_key(t, code, mods, text);
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
                        return self.apply_picker_filter_change(t);
                    }
                    KeyCode::Enter if no_chord => {
                        return self.edit_selected_chip(t, row[sel].id);
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
            KeyCode::Esc => return self.close_picker(t),
            KeyCode::Enter => return self.picker_accept(t),
            // Alt-k/j move the highlight (Up/Down deliberately don't, matching the others).
            KeyCode::Char('k') if mods.alt && !mods.ctrl => return self.picker_move(t, -1),
            KeyCode::Char('j') if mods.alt && !mods.ctrl => return self.picker_move(t, 1),
            // `Ctrl-g` / `Ctrl-f` in the Explorer: switch to Grep / Files scoped to the
            // browsed directory ("grep here").
            KeyCode::Char('g')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer =>
            {
                return self.switch_explorer_picker(t, PickerKind::Grep);
            }
            KeyCode::Char('f')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::Explorer =>
            {
                return self.switch_explorer_picker(t, PickerKind::Files);
            }
            // Alt-l/h are per-kind: Explorer descends / ascends; Grep jumps the selection to
            // the next / previous file's first hit; elsewhere Alt-h clears (via picker_back).
            KeyCode::Char('l')
                if mods.alt && !mods.ctrl && p.kind == PickerKind::Explorer =>
            {
                return self.explorer_enter_selected(t);
            }
            KeyCode::Char('l') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(t, Direction::Forward);
            }
            KeyCode::Char('h') if mods.alt && !mods.ctrl && p.kind == PickerKind::Grep => {
                return self.grep_jump_file(t, Direction::Backward);
            }
            // Alt-h / Alt-Backspace unwind: clear the query first, then pop chips, then step
            // to the parent (one segment per press), then roots mode (multi-root only).
            KeyCode::Char('h') if mods.alt && !mods.ctrl => return self.picker_back(t),
            KeyCode::Backspace if mods.alt && !mods.ctrl => return self.picker_back(t),
            // Filter-chip chords (docs/picker-filters.md). Booleans toggle in place; valued
            // filters open the editor line. Gated per kind inside the helpers.
            KeyCode::Char('c') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Case);
            }
            KeyCode::Char('w') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Word);
            }
            KeyCode::Char('e') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Lit);
            }
            KeyCode::Char('i') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Ignored);
            }
            KeyCode::Char('.') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Hidden);
            }
            KeyCode::Char('m') if mods.alt && !mods.ctrl => {
                return self.toggle_picker_filter(t, ChipId::Changed);
            }
            KeyCode::Char('g') if mods.alt && !mods.ctrl => {
                return self.open_glob_prompt(None);
            }
            KeyCode::Char('d') if mods.alt && !mods.ctrl => {
                return self.open_dir_prompt(t, None);
            }
            KeyCode::PageUp => {
                return self.picker_move(t, -(VISIBLE_ROWS as i64 - 1));
            }
            KeyCode::PageDown => {
                return self.picker_move(t, VISIBLE_ROWS as i64 - 1);
            }
            // LspServers: Ctrl-r restarts the highlighted server in place.
            KeyCode::Char('r')
                if mods.ctrl && !mods.alt && p.kind == PickerKind::LspServers =>
            {
                if let Some(PickerItem::LspServer { name, language, .. }) = p.selected_item() {
                    let (name, language) = (name.clone(), language.clone());
                    let fut =
                        rpc::<LspRestartServer>(t.as_ref(), LspRestartServerParams { language });
                    let mut fx = Effects::spawn(async move {
                        let _ = fut.await;
                        Event::Noop
                    });
                    fx.push(Effect::Toast(
                        format!("restarting {name}"),
                        ToastKind::Info,
                    ));
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
                    return self.picker_query_changed(t);
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
                    return self.picker_query_changed(t);
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
    fn on_chip_editor_key(
        &mut self,
        t: &SharedTransport,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Effects<Event> {
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
            KeyCode::Enter if no_chord => return self.commit_chip_editor(t),
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
                        ed.root_selected = if down { (sel + 1) % n } else { (sel + n - 1) % n };
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
                        let typed: String =
                            typed.chars().filter(|c| !c.is_control()).collect();
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
            return self.refresh_chip_editor_listing(t);
        }
        Effects::none()
    }

    /// Jump the grep picker's selection to the first hit of the next / previous file. The
    /// server finds the boundary across the *whole* result list (so it works past the
    /// over-fetch window); the result lands as [`Event::GrepFileJumped`].
    fn grep_jump_file(&mut self, t: &SharedTransport, direction: Direction) -> Effects<Event> {
        let Some(p) = &self.picker else {
            return Effects::none();
        };
        if p.kind != PickerKind::Grep || p.items.is_empty() {
            return Effects::none();
        }
        let fut = rpc::<PickerGrepFileJump>(
            t.as_ref(),
            PickerGrepFileJumpParams {
                from_index: p.selected,
                direction,
            },
        );
        Effects::spawn(async move {
            Event::GrepFileJumped(fut.await.map_err(|e| e.to_string()))
        })
    }

    /// Apply a server notification to the session. Stale pushes (other viewports/buffers,
    /// older picker generations) are discarded per the protocol.
    fn on_server_push(&mut self, t: &SharedTransport, n: Notification) -> Effects<Event> {
        match n.method.as_str() {
            ViewportLinesChanged::NAME => {
                let Ok(p) = serde_json::from_value::<ViewportLinesChangedParams>(n.params)
                else {
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
                let mut fx =
                    Effects::toast("buffer closed by another client", ToastKind::Warning);
                let fut = rpc::<BufferOpen>(
                    t.as_ref(),
                    BufferOpenParams {
                        buffer_id: p.next_buffer_id,
                        ..Default::default()
                    },
                );
                fx.push(Effect::Spawn(Box::pin(async move {
                    Event::Switched(fut.await.map_err(|e| e.to_string()))
                })));
                fx
            }
            _ => Effects::none(),
        }
    }

    // ---- search ----------------------------------------------------------------------------

    /// `/` or `?`: open the search prompt. Snapshots cursor/query for Esc-restore (the shell
    /// anchors its scroll via the effect) and clears the server-side search so stale
    /// highlights disappear immediately.
    pub fn enter_search(&mut self, t: &SharedTransport, extend_to_cursor: bool) -> Effects<Event> {
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
        let fut = rpc::<SearchClear>(
            t.as_ref(),
            SearchClearParams {
                buffer_id: self.buffer.buffer_id,
            },
        );
        let mut fx = Effects::one(Effect::SaveScrollAnchor);
        fx.push(Effect::Spawn(Box::pin(async move {
            let _ = fut.await;
            Event::Noop
        })));
        fx
    }

    /// One incremental step: hand the server the latest query; it jumps the cursor to the
    /// first match at-or-after the prompt's entry point. An emptied query clears instead.
    fn incremental_search(&mut self, t: &SharedTransport) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        if self.search.query.is_empty() {
            self.search.summary = None;
            let fut = rpc::<SearchClear>(t.as_ref(), SearchClearParams { buffer_id });
            let fx = Effects::spawn(async move {
                let _ = fut.await;
                Event::Noop
            });
            let revert = self.revert_to_snapshot_cursor(t);
            return fx.and(revert);
        }
        let fut = rpc::<SearchSet>(
            t.as_ref(),
            SearchSetParams {
                buffer_id,
                query: self.search.query.clone(),
                anchor: self
                    .search
                    .snapshot
                    .as_ref()
                    .map(|s| min_pos(s.cursor.position, s.cursor.anchor)),
                extend: self.search.extend_to_cursor,
            },
        );
        Effects::spawn(async move {
            Event::SearchApplied(fut.await.map_err(|e| e.to_string()))
        })
    }

    /// Move the cursor back to where the prompt opened (no-op outside incremental search or
    /// when it hasn't moved).
    fn revert_to_snapshot_cursor(&mut self, t: &SharedTransport) -> Effects<Event> {
        let Some(snap) = self.search.snapshot.as_ref() else {
            return Effects::none();
        };
        if self.buffer.cursor.position == snap.cursor.position
            && self.buffer.cursor.anchor == snap.cursor.anchor
        {
            return Effects::none();
        }
        let fut = rpc::<CursorSet>(
            t.as_ref(),
            CursorSetParams {
                buffer_id: self.buffer.buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
        );
        Effects::spawn(async move { Event::CursorMsg(fut.await.map_err(|e| e.to_string())) })
    }

    /// Esc in the prompt: restore the pre-prompt search (query + server state), cursor, and
    /// (via the effect) the shell's scroll anchor.
    pub fn abort_search(&mut self, t: &SharedTransport) -> Effects<Event> {
        self.mode = Mode::Normal;
        self.search.extend_to_cursor = false;
        self.search.history_cursor = None;
        self.search.history_draft.clear();
        let Some(snap) = self.search.snapshot.take() else {
            return Effects::none();
        };
        let buffer_id = self.buffer.buffer_id;
        let mut fx = if snap.active && !snap.query.is_empty() {
            let fut = rpc::<SearchSet>(
                t.as_ref(),
                SearchSetParams {
                    buffer_id,
                    query: snap.query.clone(),
                    anchor: None,
                    extend: false,
                },
            );
            Effects::spawn(async move {
                Event::SearchRestored(fut.await.map_err(|e| e.to_string()))
            })
        } else {
            self.search.summary = None;
            let fut = rpc::<SearchClear>(t.as_ref(), SearchClearParams { buffer_id });
            Effects::spawn(async move {
                let _ = fut.await;
                Event::Noop
            })
        };
        self.search.cursor = snap.query.len();
        self.search.query = snap.query;
        self.search.active = snap.active;
        let restore = rpc::<CursorSet>(
            t.as_ref(),
            CursorSetParams {
                buffer_id,
                position: snap.cursor.position,
                anchor: snap.cursor.anchor,
                granularity: Granularity::Char,
            },
        );
        fx.push(Effect::Spawn(Box::pin(async move {
            Event::CursorMsg(restore.await.map_err(|e| e.to_string()))
        })));
        fx.push(Effect::RestoreScrollAnchor);
        fx
    }

    /// Enter in the prompt: keep the query as the committed search.
    pub fn commit_search(&mut self) -> Effects<Event> {
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
    pub fn search_cycle(
        &mut self,
        t: &SharedTransport,
        direction: Direction,
        count: u32,
        extend: bool,
    ) -> Effects<Event> {
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
        let buffer_id = self.buffer.buffer_id;
        let t = t.clone();
        Effects::spawn(async move {
            Event::SearchNav(
                async {
                    if let Some(query) = revive {
                        let r = rpc::<SearchSet>(
                            t.as_ref(),
                            SearchSetParams {
                                buffer_id,
                                query,
                                anchor: None,
                                extend: false,
                            },
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                        if r.summary.total == 0 {
                            return Ok(SearchNavResult {
                                cursor: r.cursor,
                                summary: r.summary,
                            });
                        }
                    }
                    let mut last: Result<SearchNavResult, String> =
                        Err("search_cycle: no iterations".into());
                    for _ in 0..count.max(1) {
                        let params = SearchNavParams { buffer_id, extend };
                        last = match direction {
                            Direction::Forward => rpc::<SearchNext>(t.as_ref(), params).await,
                            Direction::Backward => rpc::<SearchPrev>(t.as_ref(), params).await,
                        }
                        .map_err(|e| e.to_string());
                        if last.is_err() {
                            break;
                        }
                    }
                    last
                }
                .await,
            )
        })
    }

    /// `Alt-/`: search for the selected text, literally (regex-escaped).
    pub fn search_from_selection(&self, t: &SharedTransport) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let t = t.clone();
        Effects::spawn(async move {
            Event::SearchFromSel(
                async {
                    let copy = rpc::<BufferCopy>(
                        t.as_ref(),
                        BufferCopyParams {
                            buffer_id,
                            scope: CopyScope::Selection,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    if copy.text.is_empty() {
                        return Ok(None);
                    }
                    let query = regex_escape(&copy.text);
                    let r = rpc::<SearchSet>(
                        t.as_ref(),
                        SearchSetParams {
                            buffer_id,
                            query: query.clone(),
                            anchor: None,
                            extend: false,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    Ok(Some((query, r)))
                }
                .await,
            )
        })
    }

    /// `Esc` in Normal — drop the active search (clear highlights).
    pub fn drop_search(&mut self, t: &SharedTransport) -> Effects<Event> {
        if !(self.search.active || self.search.summary.is_some()) {
            return Effects::none();
        }
        self.search.active = false;
        self.search.summary = None;
        let fut = rpc::<SearchClear>(
            t.as_ref(),
            SearchClearParams {
                buffer_id: self.buffer.buffer_id,
            },
        );
        Effects::spawn(async move {
            let _ = fut.await;
            Event::Noop
        })
    }

    /// `<`/`>`: step through cached grep hits server-side, then open + prime in one chain.
    pub fn grep_navigate(&self, t: &SharedTransport, direction: Direction) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let roots = self.project_paths.clone();
        let t = t.clone();
        Effects::spawn(async move {
            Event::SwitchedPrimed(
                async {
                    let target = rpc::<PickerGrepNavigate>(
                        t.as_ref(),
                        PickerGrepNavigateParams {
                            direction,
                            buffer_id,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    let Some(hit) = target else { return Ok(None) };
                    let Some((path_index, relative_path)) =
                        strip_longest_root(&hit.path, &roots)
                    else {
                        return Err(format!("{} is outside the project's roots", hit.path));
                    };
                    let _ = rpc::<NavRecord>(t.as_ref(), NavRecordParams { buffer_id }).await;
                    let open = rpc::<BufferOpen>(
                        t.as_ref(),
                        BufferOpenParams {
                            path_index: Some(path_index),
                            relative_path: Some(relative_path),
                            jump_to: Some(hit.position),
                            transient: Some(true),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    let _ = rpc::<SearchSet>(
                        t.as_ref(),
                        SearchSetParams {
                            buffer_id: open.buffer_id,
                            query: hit.query.clone(),
                            anchor: None,
                            extend: false,
                        },
                    )
                    .await;
                    Ok(Some((hit.query, open)))
                }
                .await,
            )
        })
    }

    /// Keys in the search prompt: the Search keymap table first, then printable input.
    pub fn on_search_key(
        &mut self,
        t: &SharedTransport,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
    ) -> Effects<Event> {
        if let Some(b) = lookup(KeyContext::Search, code, mods) {
            return self.search_action(t, b.action);
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
        self.incremental_search(t)
    }

    /// The Search-table actions (also reachable from the shell's action dispatch).
    pub fn search_action(&mut self, t: &SharedTransport, action: Action) -> Effects<Event> {
        match action {
            Action::SearchCommit => self.commit_search(),
            Action::SearchAbort => self.abort_search(t),
            Action::SearchHistoryPrev => {
                self.history_up();
                self.incremental_search(t)
            }
            Action::SearchHistoryNext => {
                self.history_down();
                self.incremental_search(t)
            }
            Action::SearchCursorLeft => {
                if let Some((i, _)) =
                    self.search.query[..self.search.cursor].char_indices().last()
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
                let Some((i, _)) =
                    self.search.query[..self.search.cursor].char_indices().last()
                else {
                    return Effects::none();
                };
                self.search.query.remove(i);
                self.search.cursor = i;
                self.search.history_cursor = None;
                self.incremental_search(t)
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

    fn run_confirm(&mut self, t: &SharedTransport, action: ConfirmAction) -> Effects<Event> {
        match action {
            ConfirmAction::Save { target } => self.save(t, target, true),
            ConfirmAction::ReloadDiscard => self.reload(t, true),
            ConfirmAction::CloseDiscard => self.close_buffer(t),
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
    fn accept_prompt(&mut self, t: &SharedTransport) -> Effects<Event> {
        match self.prompt.take() {
            Some(Prompt::Confirm { action, .. }) => self.run_confirm(t, action),
            Some(p @ Prompt::SaveAs { .. }) => {
                // Submit via the same path as Enter.
                self.prompt = Some(p);
                self.on_prompt_key(t, KeyCode::Enter, Mods::default(), None)
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
    pub fn reload(&self, t: &SharedTransport, force: bool) -> Effects<Event> {
        let fut = rpc::<BufferReload>(
            t.as_ref(),
            BufferReloadParams {
                buffer_id: self.buffer.buffer_id,
                force,
            },
        );
        Effects::spawn(async move {
            Event::ReloadTried(match fut.await {
                Ok(r) => Ok(ReloadTry::Reloaded(r)),
                Err(e) if e.code == ErrorCode::WOULD_DISCARD_CHANGES.code() => {
                    Ok(ReloadTry::NeedsConfirm)
                }
                Err(e) => Err(e.to_string()),
            })
        })
    }

    pub fn on_key(
        &mut self,
        t: &SharedTransport,
        code: KeyCode,
        mods: Mods,
        text: Option<String>,
        visible_rows: u32,
    ) -> Effects<Event> {
        if self.conn != ConnState::Connected {
            return Effects::none(); // editing input is suspended while the connection is down
        }

        // An open modal prompt owns the keyboard outright; a picker likewise.
        if self.prompt.is_some() {
            let t = t.clone();
            let fx = self.on_prompt_key(&t, code, mods, text);
            return fx;
        }
        if self.picker.is_some() {
            let t = t.clone();
            let fx = self.on_picker_key(&t, code, mods, text);
            return fx;
        }

        // Search mode owns the keyboard: control keys via its table, anything printable is
        // query text (case-preserved — no normalisation of the literal query).
        if self.mode == Mode::Search {
            let t = t.clone();
            let fx = self.on_search_key(&t, code, mods, text);
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
                return self.move_motion(t, motion, extend);
            }
            Pending::Surround(target) => {
                self.pending = Pending::None;
                let ch = text.as_deref().and_then(|t| t.chars().next());
                let Some(delimiter) = ch.filter(|c| !c.is_control()) else {
                    return Effects::none(); // Esc / non-char cancels
                };
                return self.edit::<InputSurround>(t, InputSurroundParams {
                    buffer_id: self.buffer.buffer_id,
                    delimiter,
                    target,
                });
            }
            Pending::Leader => {
                self.pending = Pending::None;
                if let Some(b) = lookup(KeyContext::Leader, code, mods) {
                    return self.run_action(t, b.action, 1, mods.shift, visible_rows);
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
        if let Some(b) =
            lookup(KeyContext::Global, code, mods).or_else(|| lookup(ctx, code, mods))
        {
            return self.run_action(t, b.action, count, extend, visible_rows);
        }

        // Insert mode: unmatched printable input is text.
        if self.mode == Mode::Insert && !mods.ctrl && !mods.alt {
            if let Some(typed) = text {
                let typed: String = typed
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\t')
                    .collect();
                if !typed.is_empty() {
                    return self.edit::<InputText>(
                        t,
                        InputTextParams {
                            buffer_id: self.buffer.buffer_id,
                            text: typed,
                            select_pasted: false,
                        },
                    );
                }
            }
        }
        Effects::none()
    }


    fn run_action(
        &mut self,
        t: &SharedTransport,
        action: Action,
        count: u32,
        extend: bool,
        visible_rows: u32,
    ) -> Effects<Event> {
        let task = self.dispatch_action(t, action, count, extend, visible_rows);
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
        t: &SharedTransport,
        action: Action,
        count: u32,
        extend: bool,
        visible_rows: u32,
    ) -> Effects<Event> {
        use Action as A;
        let buffer_id = self.buffer.buffer_id;
        match action {
            // ---- motions ----
            A::MoveChar(direction) => {
                self.move_motion(t, Motion::Char { direction, count }, extend)
            }
            A::MoveWord { dir, boundary } => self.move_motion(t, 
                Motion::Word {
                    direction: dir,
                    count,
                    boundary,
                    exclusive: dir == Direction::Forward && extend,
                },
                extend,
            ),
            A::MoveWordEnd { dir, boundary } => self.move_motion(t, 
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
                self.move_motion(t, 
                    Motion::VisualLine {
                        viewport_id,
                        direction,
                        count,
                    },
                    extend,
                )
            }
            A::MoveLogicalLine(direction) => self.move_motion(t, 
                Motion::LogicalLine {
                    direction,
                    count,
                    preserve_col: true,
                },
                extend,
            ),
            A::MoveLineStart => self.move_motion(t, Motion::LineStart, extend),
            A::MoveLineEnd => self.move_motion(t, Motion::LineEnd, extend),
            A::MoveLineFirstNonblank => self.move_motion(t, Motion::LineFirstNonblank, extend),
            A::MoveLogicalLineFirstNonblank(direction) => self.move_motion(t, 
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
                self.move_motion(t, 
                    Motion::Goto {
                        position: LogicalPosition { line, col: 0 },
                    },
                    extend,
                )
            }
            A::MatchBracket { inner } => self.move_motion(t, Motion::MatchBracket { inner }, extend),
            A::PageMotion { dir, half } => {
                let Some(viewport_id) = self.viewport_id else {
                    return Effects::none();
                };
                let rows = visible_rows;
                let span = if half { (rows / 2).max(1) } else { rows.max(1) };
                self.move_motion(t, 
                    Motion::VisualLine {
                        viewport_id,
                        direction: dir,
                        count: count.saturating_mul(span),
                    },
                    extend,
                )
            }
            A::NavUnit(Direction::Forward) => self.move_motion(t, Motion::NextNavigationUnit, false),
            A::NavUnit(Direction::Backward) => self.move_motion(t, Motion::PrevNavigationUnit, false),
            A::NavUnitEdge { start: false } => self.move_motion(t, Motion::EndOfNavigationUnit, true),
            A::NavUnitEdge { start: true } => self.move_motion(t, Motion::StartOfNavigationUnit, true),
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
            A::SelectLine(direction) => {
                let t = t.clone();
                task_event(
                    async move {
                        let mut last = Err("select_line: no iterations".to_string());
                        for _ in 0..count.max(1) {
                            last = rpc::<CursorSelectLine>(t.as_ref(), CursorSelectLineParams {
                                    buffer_id,
                                    direction,
                                    extend,
                                })
                                .await
                                .map_err(|e| e.to_string());
                            if last.is_err() {
                                break;
                            }
                        }
                        last
                    },
                    Event::CursorMsg,
                )
            }
            A::SwapAnchor => rpc_event::<CursorSwapAnchor>(
                t,
                CursorSwapAnchorParams { buffer_id },
                Event::CursorMsg,
            ),
            A::CollapseSelection => {
                if self.buffer.cursor.is_point() {
                    return Effects::none();
                }
                let pos = self.buffer.cursor.position;
                rpc_event::<CursorSet>(
                t,
                    CursorSetParams {
                        buffer_id,
                        position: pos,
                        anchor: pos,
                        granularity: Granularity::Char,
                    },
                    Event::CursorMsg,
                )
            }
            A::TreeExpand => self.repeat_cursor::<CursorExpand>(t, count),
            A::TreeContract => self.repeat_cursor::<CursorContract>(t, count),
            A::MotionUndo => self.motion_history::<CursorUndo>(t, count),
            A::MotionRedo => self.motion_history::<CursorRedo>(t, count),
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
                            self.dispatch_action(t, *action, *count, extend, visible_rows)
                        }
                        RepeatTarget::Find(motion) => {
                            self.move_motion(t, motion.clone(), extend)
                        }
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
            A::NavBack | A::NavForward => {
                let forward = matches!(action, A::NavForward);
                let t = t.clone();
                Effects::spawn(async move {
                    let res = if forward {
                        rpc::<NavForward>(t.as_ref(), NavStepParams { buffer_id }).await
                    } else {
                        rpc::<NavBack>(t.as_ref(), NavStepParams { buffer_id }).await
                    };
                    Event::NavDone {
                        forward,
                        result: res.map_err(|e| e.to_string()),
                    }
                })
            }

            // ---- mode transitions ----
            A::EnterInsert(where_) => {
                self.mode = Mode::Insert;
                self.enter_insert_at(t, where_)
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
            A::Backspace => self.edit::<InputBackspace>(t, BufferOnlyParams { buffer_id }),
            A::NewlineIndent => self.edit::<InputNewlineAndIndent>(t, BufferOnlyParams { buffer_id }),
            A::InsertTab => self.edit::<InputText>(t, InputTextParams {
                buffer_id,
                text: "\t".into(),
                select_pasted: false,
            }),
            A::DeletePoint => self.edit::<InputDelete>(t, BufferOnlyParams { buffer_id }),
            A::DeleteSelection => self.repeat_edit::<InputDelete>(t, count),
            A::DeleteLine => self.edit::<InputDeleteLine>(t, BufferOnlyParams { buffer_id }),
            A::Undo => self.undo_redo::<InputUndo>(t, count),
            A::Redo => self.undo_redo::<InputRedo>(t, count),
            A::MoveLines(direction) => {
                let t = t.clone();
                task_event(
                    async move {
                        let mut last = Err("move_lines: no iterations".to_string());
                        for _ in 0..count.max(1) {
                            last = rpc::<InputMoveLines>(t.as_ref(), InputMoveLinesParams {
                                    buffer_id,
                                    direction,
                                })
                                .await
                                .map_err(|e| e.to_string());
                            if last.is_err() {
                                break;
                            }
                        }
                        last
                    },
                    Event::EditDone,
                )
            }
            A::JoinLines => self.repeat_edit::<InputJoinLines>(t, count),
            A::Indent => self.repeat_edit::<InputIndent>(t, count),
            A::Dedent => self.repeat_edit::<InputDedent>(t, count),
            A::ToggleComment => self.edit::<InputToggleComment>(t, BufferOnlyParams { buffer_id }),
            A::OpenLineBelow => {
                // Park at the line's end, newline-and-indent, stay in Insert (TUI semantics).
                self.mode = Mode::Insert;
                let line = self.buffer.cursor.position.line;
                let t = t.clone();
                task_event(
                    async move {
                        let target = LogicalPosition {
                            line,
                            col: u32::MAX,
                        };
                        rpc::<CursorSet>(t.as_ref(), CursorSetParams {
                                buffer_id,
                                position: target,
                                anchor: target,
                                granularity: Granularity::Char,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        rpc::<InputNewlineAndIndent>(t.as_ref(), BufferOnlyParams { buffer_id })
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Event::EditDone,
                )
            }
            A::OpenLineAbove => {
                // Park at col 0, insert "\n" (pushes the line down), step back up (TUI semantics).
                self.mode = Mode::Insert;
                let line = self.buffer.cursor.position.line;
                let t = t.clone();
                task_event(
                    async move {
                        let target = LogicalPosition { line, col: 0 };
                        rpc::<CursorSet>(t.as_ref(), CursorSetParams {
                                buffer_id,
                                position: target,
                                anchor: target,
                                granularity: Granularity::Char,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let r = rpc::<InputText>(t.as_ref(), InputTextParams {
                                buffer_id,
                                text: "\n".into(),
                                select_pasted: false,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        let cursor = rpc::<CursorMove>(t.as_ref(), CursorMoveParams {
                                buffer_id,
                                motion: Motion::LogicalLine {
                                    direction: Direction::Backward,
                                    count: 1,
                                    preserve_col: false,
                                },
                                extend_selection: false,
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(EditResult {
                            revision: r.revision,
                            cursor,
                        })
                    },
                    Event::EditDone,
                )
            }

            // ---- clipboard ----
            A::Copy => self.copy(t, CopyScope::Selection),
            A::CopyLine => self.copy(t, CopyScope::Line),
            A::Cut => self.cut(t, CopyScope::Selection),
            A::CutLine => self.cut(t, CopyScope::Line),
            A::Paste => read_clipboard_fx(PasteKind::Before { count }),
            A::ReplaceClipboard => read_clipboard_fx(PasteKind::Replace { count }),
            A::PasteAtCursor => read_clipboard_fx(PasteKind::AtCursor),
            A::ReplaceLineClipboard => read_clipboard_fx(PasteKind::Line),
            A::Change => {
                self.mode = Mode::Insert;
                self.edit::<InputDelete>(t, BufferOnlyParams { buffer_id })
            }
            A::ChangeLine => self.edit::<InputChangeLine>(t, BufferOnlyParams { buffer_id }),
            A::BeginSurround(target) => {
                self.pending = Pending::Surround(target);
                Effects::none()
            }
            A::Unsurround(target) => self.edit::<InputUnsurround>(t, InputUnsurroundParams {
                buffer_id,
                target,
            }),

            // ---- search (core methods; the prompt-only actions also route here from
            // `Session::on_search_key`'s table lookup) ----
            A::EnterSearch => {
                
                self.enter_search(t, false)
            }
            A::EnterSearchToCursor => {
                
                self.enter_search(t, true)
            }
            A::SearchCommit
            | A::SearchAbort
            | A::SearchHistoryPrev
            | A::SearchHistoryNext
            | A::SearchCursorLeft
            | A::SearchCursorRight
            | A::SearchBackspace => {
                
                self.search_action(t, action)
            }
            A::SearchCycle(direction) => {
                
                self
                    .search_cycle(t, direction, count, extend)
            }
            A::SearchFromSelection => {
                
                self.search_from_selection(t)
            }
            A::GrepNavigate(direction) => {
                
                self.grep_navigate(t, direction)
            }
            A::DropSearch => {
                
                self.drop_search(t)
            }

            // ---- app ----
            // The server tears down all per-client state on disconnect, so quitting is just
            // closing the window.
            A::Quit => Effects::one(Effect::Exit),
            A::Save => self.save(t, None, false),
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
                self.reload(t, false)
            }
            A::NewScratch => {
                // Opening a fresh scratch is a buffer switch — record the origin so Alt-Left
                // returns.
                let t = t.clone();
                task_event(
                    async move {
                        let _ = rpc::<NavRecord>(t.as_ref(), NavRecordParams { buffer_id }).await;
                        rpc::<BufferOpen>(t.as_ref(), BufferOpenParams::default())
                            .await
                            .map_err(|e| e.to_string())
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
                
                self.close_buffer(t)
            }

            // ---- git ----
            A::ToggleDiffView => {
                let Some(viewport_id) = self.viewport_id else {
                    return Effects::none();
                };
                let enabled = !self.diff_view;
                rpc_event::<GitSetDiffView>(
                t,
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
                rpc_event::<GitNavigateHunk>(
                t,
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
                rpc_event::<GitApplyHunk>(
                t,
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
            A::OpenPicker(PickerKind::Explorer) => {
                
                self.open_explorer(t, false)
            }
            A::OpenPicker(kind) => {
                
                self.open_picker(t, kind, None, None)
            }
            A::OpenPickerInBufferDir(kind) => {
                
                self
                    .open_picker_in_buffer_dir(t, kind)
            }
            A::OpenExplorerAtRoot => {
                
                self.open_explorer(t, true)
            }

            // ---- LSP ----
            A::GotoDefinition => {
                rpc_event::<LspGotoDefinition>(t, LspBufferParams { buffer_id }, Event::Definition)
            }
            A::Hover => rpc_event::<LspHover>(t, LspBufferParams { buffer_id }, Event::HoverInfo),
            A::Format => rpc_event::<LspFormat>(t, LspBufferParams { buffer_id }, Event::FormatDone),
            A::ShowDiagnostic => {
                
                self.show_diagnostic()
            }
            A::ShowCommitInfo => {
                
                self.show_commit_info(t)
            }
            A::NextDiagnostic | A::PrevDiagnostic => {
                let direction = if matches!(action, A::NextDiagnostic) {
                    DiagnosticDirection::Next
                } else {
                    DiagnosticDirection::Prev
                };
                rpc_event::<LspNavigateDiagnostic>(
                t,
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

    fn move_motion(&self, t: &SharedTransport, motion: Motion, extend: bool) -> Effects<Event> {
        rpc_event::<CursorMove>(
                t,
            CursorMoveParams {
                buffer_id: self.buffer.buffer_id,
                motion,
                extend_selection: extend,
            },
            Event::CursorMsg,
        )
    }

    /// Run an edit `count` times sequentially (the TUI's `for _ in 0..count` loops).
    fn repeat_edit<M>(&self, t: &SharedTransport, count: u32) -> Effects<Event>
    where
        M: RpcMethod<Params = BufferOnlyParams, Result = EditResult> + 'static,
    {
        let t = t.clone();
        let buffer_id = self.buffer.buffer_id;
        task_event(
            async move {
                let mut last = Err("no iterations".to_string());
                for _ in 0..count.max(1) {
                    last = rpc::<M>(t.as_ref(), BufferOnlyParams { buffer_id })
                        .await
                        .map_err(|e| e.to_string());
                    if last.is_err() {
                        break;
                    }
                }
                last
            },
            Event::EditDone,
        )
    }

    /// Tree expand/contract: repeat until the cursor stops changing (root / empty history).
    fn repeat_cursor<M>(&self, t: &SharedTransport, count: u32) -> Effects<Event>
    where
        M: RpcMethod<Params = CursorBufferOnlyParams, Result = CursorState> + 'static,
    {
        let t = t.clone();
        let buffer_id = self.buffer.buffer_id;
        let mut prev = self.buffer.cursor;
        task_event(
            async move {
                for _ in 0..count.max(1) {
                    match rpc::<M>(t.as_ref(), CursorBufferOnlyParams { buffer_id }).await {
                        Ok(new) if new == prev => break,
                        Ok(new) => prev = new,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(prev)
            },
            Event::CursorMsg,
        )
    }

    /// Cursor-motion undo/redo: repeat until the history runs dry.
    fn motion_history<M>(&self, t: &SharedTransport, count: u32) -> Effects<Event>
    where
        M: RpcMethod<
                Params = CursorUndoParams,
                Result = aether_protocol::cursor::CursorUndoResult,
            > + 'static,
    {
        let t = t.clone();
        let buffer_id = self.buffer.buffer_id;
        let mut cursor = self.buffer.cursor;
        task_event(
            async move {
                for _ in 0..count.max(1) {
                    match rpc::<M>(t.as_ref(), CursorUndoParams { buffer_id }).await {
                        Ok(r) => {
                            if r.applied {
                                cursor = r.cursor;
                            } else {
                                break;
                            }
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(cursor)
            },
            Event::CursorMsg,
        )
    }

    /// Buffer undo/redo: repeat until the stack runs dry.
    fn undo_redo<M>(&self, t: &SharedTransport, count: u32) -> Effects<Event>
    where
        M: RpcMethod<Params = BufferOnlyParams, Result = UndoResult> + 'static,
    {
        let t = t.clone();
        let buffer_id = self.buffer.buffer_id;
        task_event(
            async move {
                let mut last = Err("no iterations".to_string());
                for _ in 0..count.max(1) {
                    match rpc::<M>(t.as_ref(), BufferOnlyParams { buffer_id }).await {
                        Ok(r) => {
                            let applied = r.applied;
                            last = Ok(r);
                            if !applied {
                                break;
                            }
                        }
                        Err(e) => {
                            last = Err(e.to_string());
                            break;
                        }
                    }
                }
                last
            },
            Event::UndoRedoDone,
        )
    }

    /// `i`/`a`/`Alt-i`/`Alt-a` — the TUI's `enter_insert_at` RPC chains.
    fn enter_insert_at(&self, t: &SharedTransport, where_: InsertWhere) -> Effects<Event> {
        let buffer_id = self.buffer.buffer_id;
        let cursor = self.buffer.cursor;
        let t = t.clone();
        let set = move |t: SharedTransport, target: LogicalPosition| async move {
            rpc::<CursorSet>(
                t.as_ref(),
                CursorSetParams {
                    buffer_id,
                    position: target,
                    anchor: target,
                    granularity: Granularity::Char,
                },
            )
            .await
            .map_err(|e| e.to_string())
        };
        Effects::spawn(async move {
            Event::CursorMsg(match where_ {
                InsertWhere::SelectionStart => {
                    set(t, min_pos(cursor.position, cursor.anchor)).await
                }
                InsertWhere::SelectionEnd => {
                    // Set to the selection's max, then step one char forward server-side
                    // (handles multi-byte chars / end-of-line).
                    let max = max_pos(cursor.position, cursor.anchor);
                    match set(t.clone(), max).await {
                        Ok(_) => rpc::<CursorMove>(
                            t.as_ref(),
                            CursorMoveParams {
                                buffer_id,
                                motion: Motion::Char {
                                    direction: Direction::Forward,
                                    count: 1,
                                },
                                extend_selection: false,
                            },
                        )
                        .await
                        .map_err(|e| e.to_string()),
                        Err(e) => Err(e),
                    }
                }
                InsertWhere::FirstLineStart => {
                    let line = cursor.position.line.min(cursor.anchor.line);
                    match set(t.clone(), LogicalPosition { line, col: 0 }).await {
                        Ok(_) => rpc::<CursorMove>(
                            t.as_ref(),
                            CursorMoveParams {
                                buffer_id,
                                motion: Motion::LineFirstNonblank,
                                extend_selection: false,
                            },
                        )
                        .await
                        .map_err(|e| e.to_string()),
                        Err(e) => Err(e),
                    }
                }
                InsertWhere::LastLineEnd => {
                    let line = cursor.position.line.max(cursor.anchor.line);
                    set(
                        t,
                        LogicalPosition {
                            line,
                            col: u32::MAX,
                        },
                    )
                    .await
                }
            })
        })
    }

    fn copy(&self, t: &SharedTransport, scope: CopyScope) -> Effects<Event> {
        rpc_event::<BufferCopy>(
                t,
            BufferCopyParams {
                buffer_id: self.buffer.buffer_id,
                scope,
            },
            Event::CopyDone,
        )
    }

    fn cut(&self, t: &SharedTransport, scope: CopyScope) -> Effects<Event> {
        rpc_event::<BufferCut>(
                t,
            BufferCopyParams {
                buffer_id: self.buffer.buffer_id,
                scope,
            },
            Event::CutDone,
        )
    }
}

/// Escape regex metacharacters so a literal string can be the search term — mirrors the
/// TUI's `regex_escape`.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

/// Run a future, mapping its output to an event (the `Task::perform` analogue).
fn task_event<T: Send + 'static>(
    fut: impl std::future::Future<Output = T> + Send + 'static,
    f: impl FnOnce(T) -> Event + Send + 'static,
) -> Effects<Event> {
    Effects::spawn(async move { f(fut.await) })
}

/// Fire a typed RPC, mapping its (stringified-error) result to an event.
fn rpc_event<M>(
    t: &SharedTransport,
    params: M::Params,
    f: impl FnOnce(Result<M::Result, String>) -> Event + Send + 'static,
) -> Effects<Event>
where
    M: aether_protocol::envelope::RpcMethod + 'static,
{
    let fut = rpc::<M>(t.as_ref(), params);
    Effects::spawn(async move { f(fut.await.map_err(|e| e.to_string())) })
}

/// Ask the shell for the system clipboard; the text comes back as `ClipboardRead`.
fn read_clipboard_fx(kind: PasteKind) -> Effects<Event> {
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
        let seeded =
            seeded_filters_for_switch(&defaults, Some(scope.clone()), PickerKind::Grep);
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
